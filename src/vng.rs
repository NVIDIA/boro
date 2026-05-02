// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Thin wrappers around the `vng` (virtme-ng) CLI used by `build` and `test`.
//!
//! - `vng -b` — build the kernel in the current directory.
//! - `vng -r . -- sh -c '<command>'` — boot the just-built kernel under virtme-ng and run a
//!   model-chosen command (or `dmesg` as the fallback).
//!
//! Both wrappers capture combined stdout/stderr and cap the captured text to the trailing
//! `MAX_LOG_CHARS` characters: when the model has to triage a failed build, the last few hundred
//! KB are what matter, and verbose builds easily produce tens of megabytes of progress noise.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::task::JoinHandle;
use tokio::time::timeout as tokio_timeout;

use crate::verbose::VerboseDest;

/// Tail length kept from a captured `vng -b` / `vng -r` log before sending it to the model.
/// 100k chars ≈ 25k tokens — enough to capture the failure context near the end of a build
/// or a typical boot dmesg without blowing up the request size.
pub const MAX_LOG_CHARS: usize = 100_000;

pub struct VngOutput {
    pub exit_status: Option<i32>,
    /// Trailing slice of stderr+stdout, capped at `MAX_LOG_CHARS` chars.
    pub log_tail: String,
    /// Original captured length before truncation (so callers can mention it to the model).
    pub original_chars: usize,
    /// Set when the process was killed because it exceeded the caller-supplied timeout.
    /// Only `run_in_vm` populates this — `run_build` runs without a timeout and always leaves it
    /// `false`.
    pub timed_out: bool,
}

/// Verify that `vng` is on `PATH`. Called once before fanning out commit workers so we fail fast.
pub fn ensure_vng_available() -> Result<()> {
    let out = Command::new("vng").arg("--version").output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => anyhow::bail!(
            "`vng --version` exited with status {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim(),
        ),
        Err(e) => Err(e).context("`vng` not found on PATH (install virtme-ng)"),
    }
}

/// Run `vng -b` in `repo` (the per-commit worktree). Returns combined log tail + exit status.
///
/// `config_fragment`, when supplied, is passed via `--config <path>` so virtme-ng merges it on top
/// of its default kernel config — used by build / test to enable the `CONFIG_*` options
/// that gate the source touched by the patch.
pub fn run_build(
    repo: &Path,
    config_fragment: Option<&Path>,
    vd: &VerboseDest,
) -> Result<VngOutput> {
    let cfg_note = config_fragment
        .map(|p| format!(" --config {}", p.display()))
        .unwrap_or_default();
    vd.line(format!(
        "vng: running `vng -b{cfg_note}` in {}",
        repo.display(),
    ));
    let mut cmd = Command::new("vng");
    cmd.current_dir(repo).arg("-b");
    if let Some(p) = config_fragment {
        cmd.arg("--config").arg(p);
    }
    let out = cmd.output().context("spawn `vng -b`")?;

    Ok(finalize_output(out))
}

/// Run a model-chosen command inside the just-built kernel via `vng -r . -- sh -c <command>`.
/// `command` is shelled-out so the model can include pipes / `&&` / `script -q -c '...'` itself.
///
/// Bounded by `timeout`: if the boot or the command hangs (kernel stuck, init never returns,
/// command spins forever, etc.) the child is killed with SIGKILL and `VngOutput { timed_out: true,
/// .. }` is returned along with whatever output was captured before the kill. Pipes are drained
/// concurrently so we don't deadlock on a child that fills its stdout/stderr buffer.
pub async fn run_in_vm(
    repo: &Path,
    command: &str,
    timeout: Duration,
    vd: &VerboseDest,
) -> Result<VngOutput> {
    vd.line(format!(
        "vng: running `vng -r . -- sh -c {:?}` in {} (timeout {}s)",
        command,
        repo.display(),
        timeout.as_secs(),
    ));
    let mut cmd = TokioCommand::new("vng");
    cmd.current_dir(repo)
        .args(["-r", ".", "--", "sh", "-c", command])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("spawn `vng -r . -- sh -c <command>`")?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // Drain each pipe line-buffered so under `--verbose` the user sees vng's output as it
    // arrives (otherwise nothing surfaces until vng exits, which is unhelpful for the in-VM
    // run that may take minutes). Bytes are still accumulated for the trailing-MAX_LOG_CHARS
    // capture below.
    let stdout_task = drain_pipe_lines(stdout, vd.clone());
    let stderr_task = drain_pipe_lines(stderr, vd.clone());

    let (status_opt, timed_out) = match tokio_timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => (Some(s), false),
        Ok(Err(e)) => return Err(anyhow::Error::from(e).context("wait `vng -r . -- sh -c ...`")),
        Err(_elapsed) => {
            vd.line(format!(
                "vng run: timed out after {}s — sending SIGKILL",
                timeout.as_secs(),
            ));
            let _ = child.start_kill();
            let _ = tokio_timeout(Duration::from_secs(5), child.wait()).await;
            (None, true)
        }
    };

    let stdout_buf = stdout_task.await.unwrap_or_default();
    let stderr_buf = stderr_task.await.unwrap_or_default();

    let mut combined = String::with_capacity(stdout_buf.len() + stderr_buf.len() + 16);
    combined.push_str(&String::from_utf8_lossy(&stderr_buf));
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&String::from_utf8_lossy(&stdout_buf));

    let original_chars = combined.chars().count();
    let log_tail = tail_chars(&combined, MAX_LOG_CHARS);

    Ok(VngOutput {
        exit_status: status_opt.and_then(|s| s.code()),
        log_tail,
        original_chars,
        timed_out,
    })
}

/// Drain a child pipe line-by-line. Each line is mirrored to stderr (when verbose) and also
/// appended (with a trailing `\n`) to a `Vec<u8>` for the caller to use in the
/// trailing-MAX_LOG_CHARS capture.
///
/// Generic over the pipe type so we use the same code for `ChildStdout` and `ChildStderr`. Tokio's
/// `BufReader::lines()` decodes UTF-8 and silently drops any malformed line — fine for kernel
/// boot output and shell-spawned kselftest output, both of which are clean UTF-8 in practice.
fn drain_pipe_lines<R>(pipe: R, vd: VerboseDest) -> JoinHandle<Vec<u8>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe).lines();
        let mut acc: Vec<u8> = Vec::new();
        // EOF or read error: stop draining. We don't surface the error because the captured-so-far
        // bytes are still useful for the model and the exit-status path already encodes the kill /
        // failure.
        while let Ok(Some(line)) = reader.next_line().await {
            if vd.active() {
                vd.line(format!("vng: {line}"));
            }
            acc.extend_from_slice(line.as_bytes());
            acc.push(b'\n');
        }
        acc
    })
}

fn finalize_output(out: std::process::Output) -> VngOutput {
    let mut combined = String::with_capacity(out.stdout.len() + out.stderr.len() + 16);
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&String::from_utf8_lossy(&out.stdout));

    let original_chars = combined.chars().count();
    let log_tail = tail_chars(&combined, MAX_LOG_CHARS);

    VngOutput {
        exit_status: out.status.code(),
        log_tail,
        original_chars,
        timed_out: false,
    }
}

/// Keep the trailing `max` characters (UTF-8 safe).
fn tail_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let skip = total - max;
    s.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_chars_short_input_returned_as_is() {
        assert_eq!(tail_chars("hello", 100), "hello");
    }

    #[test]
    fn tail_chars_truncates_from_head() {
        let s: String = (0..1000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let t = tail_chars(&s, 50);
        assert_eq!(t.chars().count(), 50);
        assert!(s.ends_with(&t));
    }

    #[test]
    fn tail_chars_utf8_boundary_safe() {
        let s = "ééééééé"; // 7 chars, 14 bytes
        let t = tail_chars(s, 3);
        assert_eq!(t, "ééé");
    }
}
