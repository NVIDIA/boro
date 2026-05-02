// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `--backend opencode`: shell out to the `opencode` CLI in non-interactive mode (`opencode run`).
//!
//! Each call is a single `opencode run --format json [-m provider/model]` invocation. Boro's
//! repo-tool sandbox (`tools.rs`) is bypassed — opencode runs its own agent loop with full access
//! to the working tree (operator must run boro in a safe environment).
//!
//! opencode has no `--append-system-prompt` equivalent, so boro's system prompt is prepended to
//! the user message (separated by a blank line). The user message is piped on stdin.
//!
//! Output is read as a stream of one-JSON-per-line events:
//! - `step_start` / `step_finish` — turn boundaries; `step_finish.part.tokens.{input,output,...}`
//!   carries usage for that turn.
//! - `text` — assistant text part (`part.text`); synthetic continuation prompts have
//!   `part.metadata.compaction_continue: true` and are skipped.
//! - `tool_use` — tool call with `part.tool` and `part.state.{input,output}`.
//!
//! Final answer = concatenation of all non-synthetic `text` events. Token usage = sum across
//! `step_finish` events (`prompt = input + cache.write + cache.read`,
//! `completion = output + reasoning`).

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

/// Run one prompt through the `opencode` CLI and return `(text, token_usage)`.
///
/// `cwd` is the working directory for the subprocess — typically the per-commit
/// worktree path so opencode's built-in tools see the tree pinned at the
/// commit being reviewed.
///
/// `worker_line` + `row_label` (when both available) drive the multi-row spinner: every streamed
/// event is classified into a phase and pushed to that row, so the user can see what the model is
/// doing without enabling `--verbose`.
pub async fn run_opencode(
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
        invoke_opencode(
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
        Err(e) => Err(anyhow!("opencode worker task failed: {e}")),
    }
}

fn invoke_opencode(
    model_id: &str,
    system: &str,
    user: &str,
    dest: &VerboseDest,
    cwd: &PathBuf,
    worker_line: Option<&WorkerLineCtx>,
    row_label: &str,
    stage_timeout: Option<Duration>,
) -> Result<(String, TokenUsage)> {
    let mut cmd = Command::new("opencode");
    cmd.current_dir(cwd).arg("run").arg("--format").arg("json");

    if !model_id.is_empty() {
        cmd.arg("-m").arg(model_id);
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn `opencode` CLI")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("opencode CLI: stdin not captured")?;
        // No --append-system-prompt — prepend the system prompt to the user message.
        if !system.is_empty() {
            stdin
                .write_all(system.as_bytes())
                .context("opencode CLI: write system prompt to stdin")?;
            stdin
                .write_all(b"\n\n")
                .context("opencode CLI: write separator to stdin")?;
        }
        stdin
            .write_all(user.as_bytes())
            .context("opencode CLI: write user prompt to stdin")?;
    }
    drop(child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .context("opencode CLI: stdout not captured")?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .context("opencode CLI: stderr not captured")?;

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
        let line = line_res.context("opencode CLI: read stdout line")?;
        if line.trim().is_empty() {
            continue;
        }

        dest.line(format!("opencode <- {line}"));

        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(w) = worker_line {
                if let Some(phase) = classify_phase(&v) {
                    w.set_line_message(format!("{row_label} {}", phase_tag(&phase)));
                }
            }
            acc.absorb(&v);
        }
    }

    let status = model_timeout::wait_child_poll(&child, "opencode CLI: wait for completion")?;
    let stderr_buf = stderr_thread.join().unwrap_or_default();

    if let Some(watchdog) = watchdog.as_ref() {
        if watchdog.timed_out() {
            return Err(model_timeout::error(
                row_label,
                stage_timeout.unwrap_or(model_timeout::REVIEW_STAGE_TIMEOUT),
            ));
        }
    }

    if !acc.saw_step_finish && acc.text_parts.is_empty() {
        anyhow::bail!(
            "opencode CLI exited with status {} without any events; stderr: {}",
            status,
            stderr_buf
                .lines()
                .next()
                .unwrap_or("(no stderr, empty stdout)")
        );
    }

    Ok(acc.into_result())
}

#[derive(Default)]
struct StreamAccumulator {
    text_parts: Vec<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_write_tokens: u64,
    cache_read_tokens: u64,
    saw_step_finish: bool,
}

impl StreamAccumulator {
    fn absorb(&mut self, v: &Value) {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "text" => {
                let part = match v.get("part") {
                    Some(p) => p,
                    None => return,
                };
                let synthetic = part
                    .get("synthetic")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let compaction = part
                    .get("metadata")
                    .and_then(|m| m.get("compaction_continue"))
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                if synthetic || compaction {
                    return;
                }
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        self.text_parts.push(text.to_string());
                    }
                }
            }
            "step_finish" => {
                self.saw_step_finish = true;
                let tokens = match v.get("part").and_then(|p| p.get("tokens")) {
                    Some(t) => t,
                    None => return,
                };
                let input = u64_field(tokens, "input");
                let output = u64_field(tokens, "output");
                let reasoning = u64_field(tokens, "reasoning");
                let cache = tokens.get("cache");
                let cache_write = cache.map(|c| u64_field(c, "write")).unwrap_or(0);
                let cache_read = cache.map(|c| u64_field(c, "read")).unwrap_or(0);
                self.prompt_tokens = self
                    .prompt_tokens
                    .saturating_add(input + cache_write + cache_read);
                self.completion_tokens = self.completion_tokens.saturating_add(output + reasoning);
                self.cache_write_tokens = self.cache_write_tokens.saturating_add(cache_write);
                self.cache_read_tokens = self.cache_read_tokens.saturating_add(cache_read);
            }
            _ => {}
        }
    }

    fn into_result(self) -> (String, TokenUsage) {
        let text = self.text_parts.join("\n\n");
        let usage = TokenUsage {
            prompt: u64_to_u32(self.prompt_tokens),
            completion: u64_to_u32(self.completion_tokens),
            cache_creation: u64_to_u32(self.cache_write_tokens),
            cache_read: u64_to_u32(self.cache_read_tokens),
        };
        (text, usage)
    }
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

/// Map a streamed event to a short phase label for the worker spinner row, or `None` to leave the
/// row alone. Events that arrive in bursts (e.g. text deltas during a single turn) all map to the
/// same phase, so the row doesn't flicker on every chunk.
fn classify_phase(v: &Value) -> Option<String> {
    let ty = v.get("type").and_then(|t| t.as_str())?;
    match ty {
        "step_start" => Some("thinking".to_string()),
        "tool_use" => {
            let name = v
                .get("part")
                .and_then(|p| p.get("tool"))
                .and_then(|t| t.as_str())
                .unwrap_or("tool");
            Some(format!("tool: {name}"))
        }
        "text" => {
            let part = v.get("part")?;
            let synthetic = part
                .get("synthetic")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let compaction = part
                .get("metadata")
                .and_then(|m| m.get("compaction_continue"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            if synthetic || compaction {
                None
            } else {
                Some("responding".to_string())
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collects_text_and_tokens_across_steps() {
        let mut acc = StreamAccumulator::default();
        acc.absorb(&json!({"type": "step_start"}));
        acc.absorb(&json!({
            "type": "text",
            "part": {"text": "Let me check.", "time": {"start": 0, "end": 1}}
        }));
        acc.absorb(&json!({
            "type": "step_finish",
            "part": {"tokens": {"input": 10, "output": 5, "reasoning": 0,
                                "cache": {"write": 0, "read": 0}}}
        }));
        acc.absorb(&json!({
            "type": "text",
            "part": {"text": "{\"findings\":[]}"}
        }));
        acc.absorb(&json!({
            "type": "step_finish",
            "part": {"tokens": {"input": 2, "output": 8, "reasoning": 0,
                                "cache": {"write": 0, "read": 100}}}
        }));
        let (text, usage) = acc.into_result();
        assert!(text.contains("Let me check."));
        assert!(text.contains("\"findings\""));
        assert_eq!(usage.prompt, Some(112));
        assert_eq!(usage.completion, Some(13));
    }

    #[test]
    fn skips_synthetic_compaction_text() {
        let mut acc = StreamAccumulator::default();
        acc.absorb(&json!({
            "type": "text",
            "part": {
                "text": "Continue if you have next steps.",
                "metadata": {"compaction_continue": true},
                "synthetic": true
            }
        }));
        acc.absorb(&json!({
            "type": "text",
            "part": {"text": "real answer"}
        }));
        let (text, _) = acc.into_result();
        assert_eq!(text, "real answer");
    }

    #[test]
    fn missing_tokens_field_does_not_panic() {
        let mut acc = StreamAccumulator::default();
        acc.absorb(&json!({"type": "step_finish", "part": {}}));
        let (_, usage) = acc.into_result();
        assert_eq!(usage.prompt, None);
        assert_eq!(usage.completion, None);
    }

    #[test]
    fn classify_step_start_is_thinking() {
        let v = json!({"type": "step_start", "step": {"id": "s1"}});
        assert_eq!(classify_phase(&v), Some("thinking".to_string()));
    }

    #[test]
    fn classify_tool_use_extracts_tool_name() {
        let v = json!({"type": "tool_use", "part": {"tool": "read", "state": {}}});
        assert_eq!(classify_phase(&v), Some("tool: read".to_string()));
    }

    #[test]
    fn classify_text_is_responding() {
        let v = json!({"type": "text", "part": {"text": "let me check"}});
        assert_eq!(classify_phase(&v), Some("responding".to_string()));
    }

    #[test]
    fn classify_synthetic_text_is_skipped() {
        let v = json!({
            "type": "text",
            "part": {"text": "continue", "synthetic": true,
                     "metadata": {"compaction_continue": true}}
        });
        assert_eq!(classify_phase(&v), None);
    }

    #[test]
    fn classify_step_finish_is_skipped() {
        let v = json!({"type": "step_finish", "part": {"tokens": {"input": 1, "output": 2}}});
        assert_eq!(classify_phase(&v), None);
    }
}
