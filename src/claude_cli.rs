// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `--backend claude`: shell out to the `claude` CLI in non-interactive mode.
//!
//! Each call is a single `claude --print --output-format stream-json --verbose
//! --dangerously-skip-permissions` invocation. Boro's repo-tool sandbox (`tools.rs`) is bypassed
//! in this mode — the CLI runs its own agent loop with full access to the kernel tree (operator
//! must run boro in a safe environment). The system prompt is forwarded via
//! `--append-system-prompt`; the user message is piped on stdin so it isn't bounded by argv
//! length.
//!
//! Output is read as a stream of one-JSON-per-line events (`type: system|assistant|user|result`).
//! Every event is surfaced on stderr (when `--verbose`) as it arrives so `tail -f` on a
//! redirected stream can follow the full session (assistant turns, tool calls, tool results,
//! thinking blocks). The terminal `result` event yields the final text and token usage returned
//! to the caller.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::api::TokenUsage;
use crate::config::ResolvedModel;
use crate::model_timeout;
use crate::progress::{phase_tag, WorkerLineCtx};
use crate::verbose::VerboseDest;

/// Run one prompt through the `claude` CLI and return `(text, token_usage)`.
///
/// `cwd` is the working directory for the subprocess — typically the per-commit
/// worktree path so the CLI's built-in tools see the tree pinned at the
/// commit being reviewed.
///
/// `worker_line` + `row_label` (when both available) drive the multi-row spinner: every streamed
/// event is classified into a phase (`thinking`, `tool: ...`) and pushed to that row. Cheap mirror
/// of what `--verbose` dumps, gated to one short line so the row stays informative without flooding.
pub async fn run_claude_cli(
    model: &ResolvedModel,
    system: &str,
    user: &str,
    dest: &VerboseDest,
    cwd: &Path,
    worker_line: Option<WorkerLineCtx>,
    row_label: String,
    stage_timeout: Option<Duration>,
) -> Result<(String, TokenUsage)> {
    let model_id = model.model_id.clone();
    let system = system.to_string();
    let user = user.to_string();
    let dest = dest.clone();
    let cwd = cwd.to_path_buf();

    let join = tokio::task::spawn_blocking(move || {
        invoke_claude(
            &model_id,
            &system,
            &user,
            &dest,
            &cwd,
            worker_line.as_ref(),
            &row_label,
            stage_timeout,
        )
    })
    .await;

    match join {
        Ok(res) => res,
        Err(e) => Err(anyhow!("claude worker task failed: {e}")),
    }
}

fn invoke_claude(
    model_id: &str,
    system: &str,
    user: &str,
    dest: &VerboseDest,
    cwd: &PathBuf,
    worker_line: Option<&WorkerLineCtx>,
    row_label: &str,
    stage_timeout: Option<Duration>,
) -> Result<(String, TokenUsage)> {
    let mut cmd = Command::new("claude");
    cmd.current_dir(cwd)
        .arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        // stream-json with --print requires --verbose to actually emit per-turn events.
        .arg("--verbose")
        .arg("--dangerously-skip-permissions");

    if !model_id.is_empty() {
        cmd.arg("--model").arg(model_id);
    }
    if !system.is_empty() {
        cmd.arg("--append-system-prompt").arg(system);
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    forward_anthropic_auth_env(&mut cmd);

    let mut child = cmd.spawn().context("spawn `claude` CLI")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("claude CLI: stdin not captured")?;
        stdin
            .write_all(user.as_bytes())
            .context("claude CLI: write user prompt to stdin")?;
    }
    // Close stdin so claude sees EOF and starts processing.
    drop(child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .context("claude CLI: stdout not captured")?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .context("claude CLI: stderr not captured")?;

    // Drain stderr on a side thread — without this, a chatty stderr can fill its pipe and block
    // claude while we're consuming stdout, deadlocking both ends.
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });
    let child = Arc::new(Mutex::new(child));
    let watchdog = stage_timeout
        .map(|timeout| model_timeout::ChildTimeoutGuard::spawn(Arc::clone(&child), timeout));

    let reader = BufReader::new(stdout);
    let mut final_event: Option<Value> = None;

    if let Some(w) = worker_line {
        w.set_line_message(format!("{row_label} {}", phase_tag("starting")));
    }

    for line_res in reader.lines() {
        let line = line_res.context("claude CLI: read stdout line")?;
        if line.trim().is_empty() {
            continue;
        }

        // Surface every raw event on stderr (when --verbose) so `tail -f` on a redirected
        // stream can follow the full session.
        dest.line(format!("claude <- {line}"));

        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(w) = worker_line {
                if let Some(phase) = classify_phase(&v) {
                    w.set_line_message(format!("{row_label} {}", phase_tag(&phase)));
                }
            }
            if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                final_event = Some(v);
            }
        }
    }

    let status = model_timeout::wait_child_poll(&child, "claude CLI: wait for completion")?;
    let stderr_buf = stderr_thread.join().unwrap_or_default();

    if let Some(watchdog) = watchdog.as_ref() {
        if watchdog.timed_out() {
            return Err(model_timeout::error(
                row_label,
                stage_timeout.unwrap_or(model_timeout::REVIEW_STAGE_TIMEOUT),
            ));
        }
    }

    if let Some(v) = final_event {
        return parse_result_event(&v);
    }

    anyhow::bail!(
        "claude CLI exited with status {} without a 'result' event; stderr: {}",
        status,
        stderr_buf
            .lines()
            .next()
            .unwrap_or("(no stderr, empty stdout)")
    );
}

/// Forward `BORO_KEY` / `BORO_URL` to the subprocess as `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL`
/// when the `ANTHROPIC_*` variants aren't already set, so users with only `BORO_*` configured can
/// switch backends without re-exporting. Existing `ANTHROPIC_*` vars in the parent env are
/// preserved (they propagate via the default env inheritance and we leave them alone).
fn forward_anthropic_auth_env(cmd: &mut Command) {
    if std::env::var_os("ANTHROPIC_API_KEY").is_none() {
        if let Ok(k) = std::env::var("BORO_KEY") {
            let k = k.trim();
            if !k.is_empty() {
                cmd.env("ANTHROPIC_API_KEY", k);
            }
        }
    }
    if std::env::var_os("ANTHROPIC_BASE_URL").is_none() {
        if let Ok(u) = std::env::var("BORO_URL") {
            let u = u.trim().trim_end_matches('/');
            if !u.is_empty() {
                cmd.env("ANTHROPIC_BASE_URL", u);
            }
        }
    }
}

/// Map a streamed event to a short phase label for the worker spinner row, or `None` when the
/// event doesn't change phase (e.g. raw deltas already covered by the previous tick).
///
/// Priority within a single `assistant` event: tool_use > thinking > text. So if the model emits
/// a turn that ends in a tool call, the row reflects the tool — which is what the user is waiting
/// on next.
fn classify_phase(v: &Value) -> Option<String> {
    let ty = v.get("type").and_then(|t| t.as_str())?;
    match ty {
        "system" => {
            // The init event arrives before any model work; useful as the first phase signal.
            if v.get("subtype").and_then(|x| x.as_str()) == Some("init") {
                Some("starting".to_string())
            } else {
                None
            }
        }
        "assistant" => {
            let blocks = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())?;
            // Prefer tool_use; fall back to thinking; fall back to text (final answer streaming).
            let tool = blocks
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
            if let Some(b) = tool {
                let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                return Some(format!("tool: {name}"));
            }
            let kind = blocks
                .iter()
                .find_map(|b| b.get("type").and_then(|t| t.as_str()))?;
            match kind {
                "thinking" => Some("thinking".to_string()),
                "text" => Some("responding".to_string()),
                _ => None,
            }
        }
        "user" => {
            // Tool result returned to the model — model is about to think again.
            let is_tool_result = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                })
                .unwrap_or(false);
            if is_tool_result {
                Some("thinking".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_result_event(v: &Value) -> Result<(String, TokenUsage)> {
    if v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false) {
        let msg = v
            .get("result")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("error").and_then(|e| e.as_str()))
            .unwrap_or("(no message)");
        anyhow::bail!("claude CLI reported error: {msg}");
    }

    let result = v
        .get("result")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let usage = v.get("usage");
    let prompt = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|x| x.as_u64())
        .map(|n| n as u32);
    let completion = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|x| x.as_u64())
        .map(|n| n as u32);
    let cache_creation = usage
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|x| x.as_u64())
        .map(|n| n as u32);
    let cache_read = usage
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|x| x.as_u64())
        .map(|n| n as u32);

    Ok((
        result,
        TokenUsage {
            prompt,
            completion,
            cache_creation,
            cache_read,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_success_event() {
        let v = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "hello",
            "usage": {"input_tokens": 12, "output_tokens": 34}
        });
        let (text, usage) = parse_result_event(&v).unwrap();
        assert_eq!(text, "hello");
        assert_eq!(usage.prompt, Some(12));
        assert_eq!(usage.completion, Some(34));
    }

    #[test]
    fn flags_is_error_event() {
        let v = json!({"type": "result", "is_error": true, "result": "boom"});
        let err = parse_result_event(&v).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn missing_result_text_is_empty_string() {
        let v = json!({"type": "result", "is_error": false});
        let (text, usage) = parse_result_event(&v).unwrap();
        assert_eq!(text, "");
        assert_eq!(usage.prompt, None);
        assert_eq!(usage.completion, None);
    }

    #[test]
    fn classify_assistant_tool_use_wins_over_text() {
        let v = json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "text", "text": "let me check"},
                {"type": "tool_use", "id": "x", "name": "Read", "input": {"file_path": "a.c"}}
            ]}
        });
        assert_eq!(classify_phase(&v), Some("tool: Read".to_string()));
    }

    #[test]
    fn classify_assistant_thinking() {
        let v = json!({
            "type": "assistant",
            "message": {"content": [{"type": "thinking", "thinking": "..."}]}
        });
        assert_eq!(classify_phase(&v), Some("thinking".to_string()));
    }

    #[test]
    fn classify_assistant_text_block_is_responding() {
        let v = json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "answer"}]}
        });
        assert_eq!(classify_phase(&v), Some("responding".to_string()));
    }

    #[test]
    fn classify_user_tool_result_is_thinking() {
        let v = json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "x", "content": "ok"}]}
        });
        assert_eq!(classify_phase(&v), Some("thinking".to_string()));
    }

    #[test]
    fn classify_system_init_is_starting() {
        let v = json!({"type": "system", "subtype": "init"});
        assert_eq!(classify_phase(&v), Some("starting".to_string()));
    }

    #[test]
    fn classify_unknown_event_yields_none() {
        let v = json!({"type": "result", "is_error": false});
        assert_eq!(classify_phase(&v), None);
    }
}
