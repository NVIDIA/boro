// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Deterministic `scripts/checkpatch.pl` evidence for the reviewer.
//!
//! The review model otherwise infers style and obvious-correctness issues from the diff
//! text alone, guessing at findings `checkpatch.pl` already produces deterministically.
//! This stage runs checkpatch on each reviewed commit and feeds its error/warning-level
//! findings into `prompts::build_reference_context` as a `# --- checkpatch ---` block,
//! the same way lore follow-up data is appended.
//!
//! Opt-in via `BORO_CHECKPATCH_ENABLED` (default off) with graceful degradation: a missing
//! `scripts/checkpatch.pl`, a missing `perl`, or a checkpatch failure never abort the review
//! (they yield [`CheckpatchOutcome::Failed`], which the caller logs and continues past).
//! checkpatch is run with `--no-tree` on the commit patch, so its output does not depend on
//! the worktree's checked-out state.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::process::Command;

const TRUNCATION_MARKER: &str = "\n[... additional checkpatch findings truncated ...]";

/// Resolved config for the checkpatch stage. Populated from `BORO_CHECKPATCH_*` env vars.
#[derive(Debug, Clone)]
pub struct CheckpatchConfig {
    pub enabled: bool,
    pub max_bytes: usize,
}

impl CheckpatchConfig {
    pub fn from_env() -> Self {
        let enabled = std::env::var("BORO_CHECKPATCH_ENABLED")
            .ok()
            .map(|s| parse_enabled(&s))
            .unwrap_or(false);
        let max_bytes = std::env::var("BORO_CHECKPATCH_MAX_BYTES")
            .ok()
            .and_then(|s| parse_max_bytes(&s))
            .unwrap_or(DEFAULT_MAX_BYTES);
        Self { enabled, max_bytes }
    }
}

impl Default for CheckpatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

const DEFAULT_MAX_BYTES: usize = 16_384;

/// Parse the `BORO_CHECKPATCH_ENABLED` value. Opt-in: true only for an explicit truthy value
/// (default off), the inverse of the opt-out `BORO_LORE_ENABLED`. Matched case-insensitively
/// so a mis-cased `True`/`On` does not silently leave the opt-in stage disabled.
fn parse_enabled(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Parse the `BORO_CHECKPATCH_MAX_BYTES` value, rejecting non-numeric and zero (which would
/// yield an empty budget). `None` means "fall back to the default".
fn parse_max_bytes(raw: &str) -> Option<usize> {
    raw.trim().parse::<usize>().ok().filter(|n| *n > 0)
}

/// Path to `scripts/checkpatch.pl` inside `tree`, if present. Detection is per-tree (the
/// script is a tree file, not a `$PATH` binary), so it is checked on every call rather than
/// cached in a process-global `OnceLock`.
pub fn checkpatch_script(tree: &Path) -> Option<PathBuf> {
    let script = tree.join("scripts/checkpatch.pl");
    script.is_file().then_some(script)
}

/// True when `perl` is runnable on `$PATH`. Probed once per process and cached: unlike the
/// checkpatch script, `perl`'s presence is process-global.
pub fn perl_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("perl")
            .args(["-e", "0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Whether the checkpatch stage should run: opt-in enabled, a kernel review, and the tree
/// actually ships `scripts/checkpatch.pl`. Extracted so the gating is unit-testable rather
/// than inlined in the review loop.
pub fn is_active(cfg: &CheckpatchConfig, target_is_kernel: bool, tree: &Path) -> bool {
    cfg.enabled && target_is_kernel && checkpatch_script(tree).is_some()
}

/// Outcome of a checkpatch run. The three cases are logged differently by the caller: usable
/// findings are injected into the review context, a clean commit is silent, and a failure is
/// logged (never aborts the review) so a user who opted in can see a broken integration
/// instead of mistaking it for a clean commit.
#[derive(Debug)]
pub enum CheckpatchOutcome {
    /// checkpatch produced error/warning-level findings (rendered, truncated summary).
    Findings(String),
    /// checkpatch ran and reported no error/warning-level findings.
    Clean,
    /// The stage could not run, or checkpatch failed; carries a short reason for logging.
    Failed(String),
}

/// Run checkpatch on `patch` using the script in `tree`.
///
/// The findings path is independent of the exit status: checkpatch exits non-zero precisely
/// when it has findings, so stdout is captured and inspected *before* the status is consulted.
/// The status is used only to tell a clean commit (exit 0, no findings) apart from a failed
/// invocation (non-zero exit with no error/warning output, e.g. a bad flag or perl error),
/// which is surfaced via [`CheckpatchOutcome::Failed`] rather than masqueraded as clean.
pub async fn run_checkpatch(tree: &Path, patch: &str, max_bytes: usize) -> CheckpatchOutcome {
    let Some(script) = checkpatch_script(tree) else {
        return CheckpatchOutcome::Failed(
            "scripts/checkpatch.pl not found in the reviewed tree".to_string(),
        );
    };
    if !perl_available() {
        return CheckpatchOutcome::Failed("perl not found on $PATH".to_string());
    }

    // checkpatch reads a patch file argument; write the commit patch to a temp file so we do
    // not have to plumb it through stdin.
    let tmp = match write_temp_patch(patch) {
        Ok(tmp) => tmp,
        Err(e) => return CheckpatchOutcome::Failed(format!("could not stage patch: {e}")),
    };

    let output = match Command::new("perl")
        .arg(&script)
        .args(["--no-tree", "--terse", "--no-summary"])
        .arg(tmp.path())
        .current_dir(tree)
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => return CheckpatchOutcome::Failed(format!("could not run checkpatch: {e}")),
    };

    let raw = String::from_utf8_lossy(&output.stdout);
    let findings = filter_findings(&raw);
    if !findings.is_empty() {
        return CheckpatchOutcome::Findings(render_summary(&truncate_lines_to_bytes(
            &findings, max_bytes,
        )));
    }

    // No error/warning findings. A zero exit means a genuinely clean patch; a non-zero exit
    // with no findings means checkpatch itself failed (bad usage, perl error) and should be
    // reported, not silently treated as clean.
    if output.status.success() {
        CheckpatchOutcome::Clean
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = if stderr.trim().is_empty() {
            format!("checkpatch exited with status {:?}", output.status.code())
        } else {
            stderr.trim().to_string()
        };
        CheckpatchOutcome::Failed(reason)
    }
}

fn write_temp_patch(patch: &str) -> std::io::Result<tempfile::NamedTempFile> {
    let mut tmp = tempfile::NamedTempFile::new()?;
    tmp.write_all(patch.as_bytes())?;
    tmp.flush()?;
    Ok(tmp)
}

/// Keep only error/warning-level checkpatch lines, dropping `CHECK:` lines and any summary
/// or blank noise. With `--terse`, each finding is a single `path:line: TYPE: message` line;
/// the substring match is intentionally lenient so it survives spacing differences across
/// checkpatch versions.
fn filter_findings(raw: &str) -> String {
    raw.lines()
        .filter(|line| line.contains("ERROR:") || line.contains("WARNING:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Truncate line-oriented text to at most `max_bytes`, on whole-line boundaries. When the
/// budget is non-zero the first line is always kept (even if it alone exceeds the budget) so
/// at least one finding reaches the model; a zero budget yields an empty string. If any lines
/// are dropped, a truncation marker is appended so the model does not treat the visible set as
/// complete (the marker itself may push the result slightly past `max_bytes`).
fn truncate_lines_to_bytes(text: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return String::new();
    };
    let mut out = first.to_string();
    let mut truncated = false;
    for line in lines {
        if out.len() + 1 + line.len() <= max_bytes {
            out.push('\n');
            out.push_str(line);
        } else {
            truncated = true;
            break;
        }
    }
    if truncated {
        out.push_str(TRUNCATION_MARKER);
    }
    out
}

/// Wrap the filtered findings with a short instruction so the model treats them as
/// authoritative deterministic evidence. The `# --- checkpatch ---` section header is added
/// by `prompts::build_reference_context`.
fn render_summary(findings: &str) -> String {
    format!(
        "`scripts/checkpatch.pl` reported the following error- and warning-level findings on \
         this commit. Treat them as authoritative style/correctness ground truth rather than \
         re-deriving them from the diff:\n\n{findings}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
net/foo.c:12: ERROR: spaces required around that '='
net/foo.c:20: WARNING: line over 80 characters
net/foo.c:33: CHECK: Alignment should match open parenthesis
total: 1 errors, 1 warnings, 1 checks, 40 lines checked";

    fn write_stub_checkpatch(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/checkpatch.pl"), body).unwrap();
    }

    #[test]
    fn parse_enabled_is_case_insensitive_and_opt_in() {
        for v in ["1", "true", "TRUE", "Yes", "On", " on "] {
            assert!(parse_enabled(v), "{v:?} should enable");
        }
        for v in ["0", "false", "no", "", "x", "2", "enabled"] {
            assert!(!parse_enabled(v), "{v:?} should not enable");
        }
    }

    #[test]
    fn parse_max_bytes_rejects_zero_and_nonnumeric() {
        assert_eq!(parse_max_bytes("4096"), Some(4096));
        assert_eq!(parse_max_bytes("  8192 "), Some(8192));
        assert_eq!(parse_max_bytes("0"), None);
        assert_eq!(parse_max_bytes("-1"), None);
        assert_eq!(parse_max_bytes("nope"), None);
        assert_eq!(parse_max_bytes(""), None);
    }

    #[test]
    fn config_default_is_opt_out() {
        let cfg = CheckpatchConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_bytes, DEFAULT_MAX_BYTES);
    }

    #[test]
    fn filter_findings_keeps_errors_and_warnings_only() {
        let out = filter_findings(SAMPLE);
        assert!(out.contains("ERROR: spaces required"));
        assert!(out.contains("WARNING: line over 80"));
        assert!(!out.contains("CHECK:"));
        assert!(!out.contains("total:"));
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn filter_findings_empty_when_no_error_or_warning() {
        let clean = "net/foo.c:33: CHECK: nit\ntotal: 0 errors, 0 warnings, 1 checks";
        assert!(filter_findings(clean).is_empty());
    }

    #[test]
    fn truncate_lines_keeps_first_line_even_over_budget() {
        let text = "aaaaaaaaaa\nbbbbbbbbbb";
        // Budget smaller than the first line: first line is kept whole, rest dropped, and a
        // truncation marker is appended.
        let out = truncate_lines_to_bytes(text, 3);
        assert!(out.starts_with("aaaaaaaaaa"));
        assert!(!out.contains("bbbbbbbbbb"));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn truncate_lines_stops_on_whole_line_boundary() {
        let text = "line1\nline2\nline3";
        // Room for "line1\nline2" (11 bytes) but not the third line → third dropped + marker.
        let out = truncate_lines_to_bytes(text, 11);
        assert!(out.starts_with("line1\nline2"));
        assert!(!out.contains("line3"));
        assert!(out.contains("truncated"));
        assert_eq!(truncate_lines_to_bytes(text, 0), "");
    }

    #[test]
    fn truncate_lines_no_marker_when_nothing_dropped() {
        let text = "line1\nline2";
        assert_eq!(truncate_lines_to_bytes(text, 4096), "line1\nline2");
    }

    #[test]
    fn render_summary_includes_findings_and_instruction() {
        let s = render_summary("net/foo.c:12: ERROR: bad");
        assert!(s.contains("checkpatch.pl"));
        assert!(s.contains("net/foo.c:12: ERROR: bad"));
    }

    #[test]
    fn is_active_requires_enabled_kernel_and_script() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_checkpatch(dir.path(), "x");
        let empty = tempfile::tempdir().unwrap();
        let on = CheckpatchConfig {
            enabled: true,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        let off = CheckpatchConfig {
            enabled: false,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        assert!(is_active(&on, true, dir.path()));
        assert!(!is_active(&off, true, dir.path()), "disabled");
        assert!(!is_active(&on, false, dir.path()), "not kernel");
        assert!(!is_active(&on, true, empty.path()), "no script");
    }

    #[test]
    fn checkpatch_script_absent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(checkpatch_script(dir.path()).is_none());
    }

    #[test]
    fn checkpatch_script_found_when_present() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_checkpatch(dir.path(), "#!/usr/bin/perl\n");
        assert!(checkpatch_script(dir.path()).is_some());
    }

    #[tokio::test]
    async fn run_checkpatch_reports_failure_without_script() {
        // No scripts/checkpatch.pl → Failed (logged, never aborts) rather than silent.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            run_checkpatch(dir.path(), "some patch text", 4096).await,
            CheckpatchOutcome::Failed(_)
        ));
    }

    #[tokio::test]
    async fn run_checkpatch_captures_findings_despite_nonzero_exit() {
        // checkpatch exits non-zero precisely when it has findings; the stage must still
        // capture stdout. A stub emits terse ERROR/WARNING/CHECK lines and exits 1.
        if !perl_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_stub_checkpatch(
            dir.path(),
            "print \"net/foo.c:1: ERROR: bad thing\\n\";\n\
             print \"net/foo.c:2: WARNING: iffy thing\\n\";\n\
             print \"net/foo.c:3: CHECK: minor nit\\n\";\n\
             exit 1;\n",
        );
        match run_checkpatch(dir.path(), "diff --git a/x b/x\n", 4096).await {
            CheckpatchOutcome::Findings(summary) => {
                assert!(summary.contains("ERROR: bad thing"));
                assert!(summary.contains("WARNING: iffy thing"));
                assert!(!summary.contains("CHECK:"));
                assert!(summary.contains("checkpatch.pl"));
            }
            other => panic!("expected Findings, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_checkpatch_clean_when_no_findings_and_zero_exit() {
        // Only CHECK-level output and a clean (exit 0) run → Clean, no injected block.
        if !perl_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_stub_checkpatch(
            dir.path(),
            "print \"net/foo.c:3: CHECK: minor nit\\n\";\nexit 0;\n",
        );
        assert!(matches!(
            run_checkpatch(dir.path(), "diff\n", 4096).await,
            CheckpatchOutcome::Clean
        ));
    }

    #[tokio::test]
    async fn run_checkpatch_failed_when_nonzero_exit_without_findings() {
        // Non-zero exit with no error/warning output (e.g. a usage error) → Failed, surfacing
        // stderr, instead of being mistaken for a clean commit.
        if !perl_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_stub_checkpatch(
            dir.path(),
            "print STDERR \"Usage: checkpatch.pl ...\\n\";\nexit 2;\n",
        );
        match run_checkpatch(dir.path(), "diff\n", 4096).await {
            CheckpatchOutcome::Failed(reason) => assert!(reason.contains("Usage")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_checkpatch_truncates_and_marks_large_findings() {
        // Many findings with a tiny budget → summary is truncated and marked.
        if !perl_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_stub_checkpatch(
            dir.path(),
            "for my $i (1..50) { print \"net/foo.c:$i: ERROR: problem number $i\\n\"; }\nexit 1;\n",
        );
        match run_checkpatch(dir.path(), "diff\n", 120).await {
            CheckpatchOutcome::Findings(summary) => {
                assert!(summary.contains("truncated"));
                assert!(!summary.contains("problem number 50"));
            }
            other => panic!("expected Findings, got {other:?}"),
        }
    }
}
