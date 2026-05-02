// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `--backend codex`: shell out to the `codex` CLI in non-interactive mode.
//!
//! Each call is a single `codex exec --json --output-last-message ...` invocation. Boro's
//! repo-tool sandbox (`tools.rs`) is bypassed in this mode - the CLI runs its own agent loop with
//! full access to the working tree. The command is launched with approval/sandbox bypass flags
//! (`--ask-for-approval never` plus `--dangerously-bypass-approvals-and-sandbox`) so automated
//! reviews cannot block waiting for an interactive approval prompt.
//!
//! Codex does not expose a system-prompt flag on `exec`, so boro prepends the system prompt to the
//! user message and pipes the combined prompt on stdin. JSONL events are consumed for verbose logs,
//! worker-row phases, and token usage. The final answer is read from `--output-last-message`, with
//! streamed `agent_message` text as a fallback.

use std::fs;
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

/// Run one prompt through the `codex` CLI and return `(text, token_usage)`.
///
/// `cwd` is the working directory for the subprocess - typically the per-commit
/// worktree path so Codex's built-in tools see the tree pinned at the commit
/// being reviewed.
pub async fn run_codex_cli(
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
        invoke_codex(
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
        Err(e) => Err(anyhow!("codex worker task failed: {e}")),
    }
}

fn invoke_codex(
    model_id: &str,
    system: &str,
    user: &str,
    dest: &VerboseDest,
    cwd: &PathBuf,
    worker_line: Option<&WorkerLineCtx>,
    row_label: &str,
    stage_timeout: Option<Duration>,
) -> Result<(String, TokenUsage)> {
    let output_file =
        tempfile::NamedTempFile::new().context("codex CLI: create output-last-message file")?;
    let output_path = output_file.path().to_path_buf();

    let mut cmd = Command::new("codex");
    cmd.current_dir(cwd)
        // Global option: never escalate to an interactive approval prompt.
        .arg("--ask-for-approval")
        .arg("never")
        .arg("exec")
        .arg("--json")
        .arg("--color")
        .arg("never")
        .arg("--output-last-message")
        .arg(&output_path)
        .arg("-C")
        .arg(cwd)
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg("--skip-git-repo-check")
        .arg("--ephemeral");

    if !model_id.is_empty() {
        cmd.arg("--model").arg(model_id);
    }

    // Read the full prompt from stdin. Passing "-" avoids Codex treating a piped stdin as
    // additional input appended to an argv prompt.
    cmd.arg("-");

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn `codex` CLI")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("codex CLI: stdin not captured")?;
        if !system.is_empty() {
            stdin
                .write_all(system.as_bytes())
                .context("codex CLI: write system prompt to stdin")?;
            stdin
                .write_all(b"\n\n")
                .context("codex CLI: write separator to stdin")?;
        }
        stdin
            .write_all(user.as_bytes())
            .context("codex CLI: write user prompt to stdin")?;
    }
    drop(child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .context("codex CLI: stdout not captured")?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .context("codex CLI: stderr not captured")?;

    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });
    let child = Arc::new(Mutex::new(child));
    let watchdog = stage_timeout
        .map(|timeout| model_timeout::ChildTimeoutGuard::spawn(Arc::clone(&child), timeout));

    let reader = BufReader::new(stdout);
    let mut acc = StreamAccumulator::default();

    if let Some(w) = worker_line {
        w.set_line_message(format!("{row_label} {}", phase_tag("starting")));
    }

    for line_res in reader.lines() {
        let line = line_res.context("codex CLI: read stdout line")?;
        if line.trim().is_empty() {
            continue;
        }

        dest.line(format!("codex <- {line}"));

        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(w) = worker_line {
                if let Some(phase) = classify_phase(&v) {
                    w.set_line_message(format!("{row_label} {}", phase_tag(&phase)));
                }
            }
            acc.absorb(&v);
        }
    }

    let status = model_timeout::wait_child_poll(&child, "codex CLI: wait for completion")?;
    let stderr_buf = stderr_thread.join().unwrap_or_default();
    let final_text = fs::read_to_string(&output_path).unwrap_or_default();

    if let Some(watchdog) = watchdog.as_ref() {
        if watchdog.timed_out() {
            return Err(model_timeout::error(
                row_label,
                stage_timeout.unwrap_or(model_timeout::REVIEW_STAGE_TIMEOUT),
            ));
        }
    }

    if !status.success() {
        let detail = acc
            .error
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| stderr_summary(&stderr_buf));
        anyhow::bail!("codex CLI exited with status {status}; stderr: {detail}");
    }

    if final_text.trim().is_empty() && acc.text_parts.is_empty() {
        anyhow::bail!(
            "codex CLI exited with status {} without a final message; stderr: {}",
            status,
            stderr_summary(&stderr_buf)
        );
    }

    Ok(acc.into_result(final_text))
}

#[derive(Default)]
struct StreamAccumulator {
    text_parts: Vec<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_write_tokens: u64,
    cache_read_tokens: u64,
    error: Option<String>,
}

impl StreamAccumulator {
    fn absorb(&mut self, v: &Value) {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "item.completed" => self.absorb_completed_item(v),
            "turn.completed" => {
                if let Some(usage) = v.get("usage") {
                    self.absorb_usage(usage);
                }
            }
            "turn.failed" | "error" => {
                self.error = extract_error(v);
            }
            _ => {}
        }
    }

    fn absorb_completed_item(&mut self, v: &Value) {
        let item = match v.get("item") {
            Some(item) => item,
            None => return,
        };
        let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty != "agent_message" {
            return;
        }
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            if !text.is_empty() {
                self.text_parts.push(text.to_string());
            }
        }
    }

    fn absorb_usage(&mut self, usage: &Value) {
        let input = u64_field(usage, "input_tokens");
        let output = u64_field(usage, "output_tokens");
        let reasoning = u64_field(usage, "reasoning_output_tokens");
        let cache_write = u64_field(usage, "cache_creation_input_tokens");
        let cache_read = u64_field(usage, "cached_input_tokens");

        self.prompt_tokens = self.prompt_tokens.saturating_add(input);
        self.completion_tokens =
            self.completion_tokens
                .saturating_add(if output == 0 { reasoning } else { output });
        self.cache_write_tokens = self.cache_write_tokens.saturating_add(cache_write);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(cache_read);
    }

    fn into_result(self, output_last_message: String) -> (String, TokenUsage) {
        let file_text = output_last_message
            .trim_end_matches(['\r', '\n'])
            .to_string();
        let text = if file_text.is_empty() {
            self.text_parts.join("\n\n")
        } else {
            file_text
        };
        let usage = TokenUsage {
            prompt: u64_to_u32(self.prompt_tokens),
            completion: u64_to_u32(self.completion_tokens),
            cache_creation: u64_to_u32(self.cache_write_tokens),
            cache_read: u64_to_u32(self.cache_read_tokens),
        };
        (text, usage)
    }
}

fn classify_phase(v: &Value) -> Option<String> {
    let ty = v.get("type").and_then(|t| t.as_str())?;
    match ty {
        "thread.started" => Some("starting".to_string()),
        "turn.started" => Some("thinking".to_string()),
        "item.started" | "item.completed" => {
            let item = v.get("item")?;
            classify_item_phase(item)
        }
        _ => None,
    }
}

fn classify_item_phase(item: &Value) -> Option<String> {
    let ty = item.get("type").and_then(|t| t.as_str())?;
    match ty {
        "reasoning" => Some("thinking".to_string()),
        "agent_message" => Some("responding".to_string()),
        "command_execution" => Some(format!("tool: {}", command_label(item))),
        "tool_call" | "mcp_tool_call" | "function_call" => {
            Some(format!("tool: {}", tool_label(item)))
        }
        other if other.contains("tool") || other.contains("command") => {
            Some(format!("tool: {}", tool_label(item)))
        }
        _ => None,
    }
}

fn command_label(item: &Value) -> String {
    item.get("command")
        .and_then(|c| c.as_str())
        .and_then(|cmd| cmd.split_whitespace().next())
        .filter(|cmd| !cmd.is_empty())
        .unwrap_or("command")
        .to_string()
}

fn tool_label(item: &Value) -> String {
    item.get("name")
        .and_then(|n| n.as_str())
        .or_else(|| item.get("tool").and_then(|t| t.as_str()))
        .filter(|name| !name.is_empty())
        .unwrap_or("tool")
        .to_string()
}

fn extract_error(v: &Value) -> Option<String> {
    v.get("error")
        .and_then(|e| {
            e.as_str().map(str::to_string).or_else(|| {
                e.get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
        })
        .or_else(|| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
}

fn stderr_summary(stderr: &str) -> &str {
    stderr.lines().next().unwrap_or("(no stderr)")
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn u64_to_u32(n: u64) -> Option<u32> {
    if n == 0 {
        None
    } else {
        Some(u32::try_from(n).unwrap_or(u32::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collects_agent_message_and_usage() {
        let mut acc = StreamAccumulator::default();
        acc.absorb(&json!({
            "type": "item.completed",
            "item": {"id": "item_0", "type": "agent_message", "text": "answer"}
        }));
        acc.absorb(&json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": 100,
                "cached_input_tokens": 25,
                "output_tokens": 40,
                "reasoning_output_tokens": 30
            }
        }));

        let (text, usage) = acc.into_result(String::new());
        assert_eq!(text, "answer");
        assert_eq!(usage.prompt, Some(100));
        assert_eq!(usage.completion, Some(40));
        assert_eq!(usage.cache_read, Some(25));
        assert_eq!(usage.cache_creation, None);
    }

    #[test]
    fn output_last_message_wins_over_streamed_text() {
        let mut acc = StreamAccumulator::default();
        acc.absorb(&json!({
            "type": "item.completed",
            "item": {"type": "agent_message", "text": "streamed"}
        }));

        let (text, _) = acc.into_result("final from file\n".to_string());
        assert_eq!(text, "final from file");
    }

    #[test]
    fn captures_error_message() {
        let mut acc = StreamAccumulator::default();
        acc.absorb(&json!({
            "type": "turn.failed",
            "error": {"message": "boom"}
        }));
        assert_eq!(acc.error.as_deref(), Some("boom"));
    }

    #[test]
    fn classify_turn_started_is_thinking() {
        let v = json!({"type": "turn.started"});
        assert_eq!(classify_phase(&v), Some("thinking".to_string()));
    }

    #[test]
    fn classify_command_execution_extracts_first_word() {
        let v = json!({
            "type": "item.started",
            "item": {"type": "command_execution", "command": "rg -n codex src"}
        });
        assert_eq!(classify_phase(&v), Some("tool: rg".to_string()));
    }

    #[test]
    fn classify_agent_message_is_responding() {
        let v = json!({
            "type": "item.completed",
            "item": {"type": "agent_message", "text": "answer"}
        });
        assert_eq!(classify_phase(&v), Some("responding".to_string()));
    }
}
