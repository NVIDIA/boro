// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `boro test`: per-commit driver.
//!
//! Build the kernel with `vng -b`, ask the model to pick a quick test command based on the patch
//! (falling back to `dmesg` when nothing useful fits), run that command inside virtme-ng, and ask
//! the model to triage the captured output and produce both a summary and any findings. If the
//! build fails, the test stage is skipped and a single `Critical` "build failed" finding is
//! emitted.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use crate::api::{self, StageUsage, TokenUsage};
use crate::config::ResolvedModel;
use crate::git;
use crate::progress::WorkerLineCtx;
use crate::snapshot::SnapshotPublisher;
use crate::test_build::{build_kconfig_stage, call_model, parse_findings_or_fallback};
use crate::verbose::VerboseDest;
use crate::vng;

const KCONFIG_STEP: &str = "kconfig fragment";
const VNG_BUILD_STEP: &str = "vng -b";
const TEST_PICKER_STEP: &str = "test picker";
const VNG_RUN_STEP: &str = "vng run";
const TEST_REVIEW_STEP: &str = "test review";

/// What we run inside the VM when the picker can't think of anything useful.
const FALLBACK_COMMAND: &str = "dmesg";

/// Per-commit driver for `boro test`. `run_timeout` caps the in-VM run; sourced from the
/// `--timeout` CLI flag (default 5 minutes). When the kernel hangs (init never returns, panic
/// with `panic_on_oops=0`, long-running kselftest with no `--timeout` bump) we kill vng and
/// surface the partial output as a `Critical` finding instead of blocking the worker forever.
#[allow(clippy::too_many_arguments)]
pub async fn commit_test_boot(
    sha: &str,
    effective_repo: &Path,
    client: &reqwest::Client,
    model: &ResolvedModel,
    vd: &VerboseDest,
    dry_run: bool,
    run_timeout: Duration,
    worker_ctx: Option<&WorkerLineCtx>,
    publisher: &SnapshotPublisher,
) -> Result<Value> {
    if dry_run {
        vd.line("test dry run: skipping `vng -b` / `vng -r` and model call");
        return Ok(json!({
            "sha": sha,
            "dry_run": true,
            "findings": [],
        }));
    }

    let kconfig_stage = build_kconfig_stage(
        sha,
        effective_repo,
        client,
        model,
        vd,
        worker_ctx,
        publisher,
    )
    .await;

    // The kconfig stage above sets the row to its own phase labels — refresh AFTER it returns so
    // the row reflects the actual `vng -b` work, not whatever the kconfig model call left behind.
    if let Some(w) = worker_ctx {
        w.set_line_message("kernel build");
    }

    let t0 = Instant::now();
    let build = vng::run_build(
        effective_repo,
        kconfig_stage.file.as_ref().map(|f| f.path()),
        vd,
    )?;
    let build_wall = t0.elapsed();
    // Hold the tempfile until vng -b has finished reading it; dropping it deletes the backing
    // file from disk, and we can't reuse the fragment for the test stage.
    drop(kconfig_stage.file);
    let build_exit = match build.exit_status {
        Some(c) => c.to_string(),
        None => "(killed by signal)".to_string(),
    };
    let build_ok = build.exit_status == Some(0);

    publisher.add_stage(StageUsage {
        step: VNG_BUILD_STEP,
        usage: TokenUsage::default(),
        wall: build_wall,
        error: if build_ok {
            None
        } else {
            Some(format!("build failed: exit={build_exit}"))
        },
    });

    vd.line(format!(
        "vng -b: exit={build_exit} log={} chars (kept tail {} chars)",
        build.original_chars,
        build.log_tail.chars().count(),
    ));

    if !build_ok {
        // Skip test stage. Emit a Critical finding so the human report calls it out.
        let finding = json!({
            "problem": format!("build failed before test could run (vng -b exit={build_exit})"),
            "severity": "Critical",
            "severity_explanation": "the kernel did not build, so no test was attempted; see build for build-log triage",
        });
        let findings = json!([finding]);
        publisher.set_findings(json!({ "findings": findings.clone() }));
        return Ok(json!({
            "sha": sha,
            "findings": findings,
            "usage": json!({
                "prompt_tokens": kconfig_stage.usage.prompt.unwrap_or(0),
                "completion_tokens": kconfig_stage.usage.completion.unwrap_or(0),
                "api_calls": if kconfig_stage.usage.prompt.is_some() || kconfig_stage.usage.completion.is_some() { 1 } else { 0 },
            }),
            "usage_steps": json!([
                {
                    "step": KCONFIG_STEP,
                    "prompt_tokens": kconfig_stage.usage.prompt,
                    "completion_tokens": kconfig_stage.usage.completion,
                    "wall_ms": kconfig_stage.wall.as_millis() as u64,
                    "error": kconfig_stage.error,
                },
                {
                    "step": VNG_BUILD_STEP,
                    "prompt_tokens": null,
                    "completion_tokens": null,
                    "wall_ms": build_wall.as_millis() as u64,
                    "error": format!("build failed: exit={build_exit}"),
                }
            ]),
            "build_status": "failed",
            "boot_status": "skipped",
            "kconfig_options": kconfig_stage.lines,
        }));
    }

    // Build succeeded — pick a quick test command.
    let picker = pick_test_command(sha, effective_repo, client, model, vd, worker_ctx).await;
    publisher.add_stage(StageUsage {
        step: TEST_PICKER_STEP,
        usage: picker.usage,
        wall: picker.wall,
        error: picker.error.clone(),
    });
    let chosen_command = test_command_or_fallback(picker.command.as_deref());
    vd.line(format!(
        "test picker: chose `{chosen_command}` (rationale: {})",
        picker.rationale
    ));

    // Run the chosen command inside the VM. Refresh the row label so it reflects the actual
    // in-VM work, not the picker stage's residual phase tag — show the command itself, truncated
    // so long kselftest invocations don't blow the row width.
    if let Some(w) = worker_ctx {
        w.set_line_message(format!("vng: {}", truncate_for_row(&chosen_command, 60)));
    }
    let t1 = Instant::now();
    let run = vng::run_in_vm(effective_repo, &chosen_command, run_timeout, vd).await?;
    let run_wall = t1.elapsed();
    let run_exit = match run.exit_status {
        Some(c) => c.to_string(),
        None if run.timed_out => format!("(killed: timed out after {}s)", run_timeout.as_secs()),
        None => "(killed by signal)".to_string(),
    };
    let run_ok = run.exit_status == Some(0);

    let run_stage_error = if run_ok {
        None
    } else if run.timed_out {
        Some(format!(
            "vng run timed out after {}s; system stuck",
            run_timeout.as_secs()
        ))
    } else {
        Some(format!("vng run failed: exit={run_exit}"))
    };

    publisher.add_stage(StageUsage {
        step: VNG_RUN_STEP,
        usage: TokenUsage::default(),
        wall: run_wall,
        error: run_stage_error.clone(),
    });

    vd.line(format!(
        "vng run: exit={run_exit}{timeout_note} log={} chars (kept tail {} chars)",
        run.original_chars,
        run.log_tail.chars().count(),
        timeout_note = if run.timed_out { " (timed out)" } else { "" },
    ));

    let user_msg = format_run_user_message(
        &run,
        &run_exit,
        &chosen_command,
        &picker.rationale,
        run_timeout,
    );
    let (raw, usage, llm_wall, llm_err) = call_model(
        client,
        model,
        api::SYSTEM_TEST_BOOT,
        &user_msg,
        TEST_REVIEW_STEP,
        vd,
        worker_ctx,
    )
    .await;

    publisher.add_stage(StageUsage {
        step: TEST_REVIEW_STEP,
        usage,
        wall: llm_wall,
        error: llm_err.clone(),
    });

    let mut findings_vec: Vec<Value> = Vec::new();
    if run.timed_out {
        // Synthetic finding so the human report always reflects the stuck-system verdict, even
        // when the model later fails or returns no findings from the partial output.
        findings_vec.push(json!({
            "problem": format!(
                "system hung during test: `vng -r . -- sh -c {chosen_command:?}` did not complete within {}s and was killed",
                run_timeout.as_secs()
            ),
            "severity": "Critical",
            "severity_explanation": "the kernel under test did not return from the chosen command within the budget; a captured tail of whatever the guest produced before the kill is included for the model's review",
        }));
    }

    // Parse the model's reply: prefer the new `{summary, findings}` shape; fall back to the legacy
    // `{findings}` shape on parse failure so older models still produce something usable.
    let mut model_summary = String::new();
    if let Some(text) = raw.as_deref() {
        match api::parse_findings_with_summary(text) {
            Ok((s, parsed)) => {
                model_summary = s;
                if let Some(arr) = parsed.get("findings").and_then(|f| f.as_array()) {
                    findings_vec.extend(arr.iter().cloned());
                }
            }
            Err(e) => {
                vd.line(format!(
                    "test review: summary parse failed ({e}); falling back to legacy findings parser"
                ));
                if let Some(arr) = parse_findings_or_fallback(text, vd).as_array() {
                    findings_vec.extend(arr.iter().cloned());
                }
            }
        }
    }
    let findings = Value::Array(findings_vec);
    publisher.set_findings(json!({ "findings": findings.clone() }));

    // Total picker stage usage rolls into the per-commit total too.
    let total_prompt = kconfig_stage.usage.prompt.unwrap_or(0)
        + picker.usage.prompt.unwrap_or(0)
        + usage.prompt.unwrap_or(0);
    let total_completion = kconfig_stage.usage.completion.unwrap_or(0)
        + picker.usage.completion.unwrap_or(0)
        + usage.completion.unwrap_or(0);
    let api_calls =
        (if kconfig_stage.usage.prompt.is_some() || kconfig_stage.usage.completion.is_some() {
            1u32
        } else {
            0
        }) + (if picker.usage.prompt.is_some() || picker.usage.completion.is_some() {
            1
        } else {
            0
        }) + (if raw.is_some() { 1 } else { 0 });
    let total_usage = json!({
        "prompt_tokens": total_prompt,
        "completion_tokens": total_completion,
        "api_calls": api_calls,
    });

    let usage_steps = json!([
        {
            "step": KCONFIG_STEP,
            "prompt_tokens": kconfig_stage.usage.prompt,
            "completion_tokens": kconfig_stage.usage.completion,
            "wall_ms": kconfig_stage.wall.as_millis() as u64,
            "error": kconfig_stage.error,
        },
        {
            "step": VNG_BUILD_STEP,
            "prompt_tokens": null,
            "completion_tokens": null,
            "wall_ms": build_wall.as_millis() as u64,
            "error": null,
        },
        {
            "step": TEST_PICKER_STEP,
            "prompt_tokens": picker.usage.prompt,
            "completion_tokens": picker.usage.completion,
            "wall_ms": picker.wall.as_millis() as u64,
            "error": picker.error,
        },
        {
            "step": VNG_RUN_STEP,
            "prompt_tokens": null,
            "completion_tokens": null,
            "wall_ms": run_wall.as_millis() as u64,
            "error": run_stage_error,
        },
        {
            "step": TEST_REVIEW_STEP,
            "prompt_tokens": usage.prompt,
            "completion_tokens": usage.completion,
            "wall_ms": llm_wall.as_millis() as u64,
            "error": llm_err,
        },
    ]);

    let boot_status = if run.timed_out {
        "timed_out"
    } else if run_ok {
        "ok"
    } else {
        "failed"
    };

    Ok(json!({
        "sha": sha,
        "findings": findings,
        "usage": total_usage,
        "usage_steps": usage_steps,
        "build_status": "ok",
        "boot_status": boot_status,
        "test_command": chosen_command,
        "test_summary": model_summary,
        "kconfig_options": kconfig_stage.lines,
    }))
}

struct PickerStage {
    command: Option<String>,
    rationale: String,
    usage: TokenUsage,
    wall: Duration,
    error: Option<String>,
}

/// Single-shot model call that proposes a quick test command for this commit. Falls back to
/// `command: None` (→ `dmesg` at the call site) on API or parse failure; never aborts the run.
async fn pick_test_command(
    sha: &str,
    effective_repo: &Path,
    client: &reqwest::Client,
    model: &ResolvedModel,
    vd: &VerboseDest,
    worker_ctx: Option<&WorkerLineCtx>,
) -> PickerStage {
    let user_msg = build_picker_user_message(sha, effective_repo, vd);
    let (raw, usage, wall, err) = call_model(
        client,
        model,
        api::system_test_picker(),
        &user_msg,
        TEST_PICKER_STEP,
        vd,
        worker_ctx,
    )
    .await;
    let Some(text) = raw else {
        return PickerStage {
            command: None,
            rationale: format!(
                "picker call failed ({}); falling back to dmesg",
                err.as_deref().unwrap_or("unknown error")
            ),
            usage,
            wall,
            error: err,
        };
    };
    match parse_picker_json(&text) {
        Ok((cmd, rationale)) => PickerStage {
            command: cmd,
            rationale,
            usage,
            wall,
            error: err,
        },
        Err(parse_err) => {
            vd.line(format!(
                "test picker: parse failed ({parse_err}); falling back to dmesg"
            ));
            PickerStage {
                command: None,
                rationale: format!(
                    "picker output unparseable ({parse_err}); falling back to dmesg"
                ),
                usage,
                wall,
                error: err.or_else(|| Some(format!("parse: {parse_err}"))),
            }
        }
    }
}

fn build_picker_user_message(sha: &str, effective_repo: &Path, vd: &VerboseDest) -> String {
    let patch = git::show_patch(effective_repo, sha).unwrap_or_else(|e| {
        vd.line(format!(
            "test picker: git show failed ({e}); proceeding with empty patch"
        ));
        String::new()
    });
    let paths = git::changed_paths(effective_repo, sha).unwrap_or_else(|e| {
        vd.line(format!(
            "test picker: changed_paths failed ({e}); proceeding without file list"
        ));
        Vec::new()
    });
    let path_list = if paths.is_empty() {
        "(none)".to_string()
    } else {
        paths.join("\n")
    };
    format!(
        "COMMIT_SHA={sha}\nCHANGED_FILES:\n{path_list}\n\n--- PATCH (git show {sha}) ---\n{patch}"
    )
}

fn parse_picker_json(raw: &str) -> Result<(Option<String>, String)> {
    let trimmed = api::strip_json_fences(raw);
    let v: Value = serde_json::from_str(trimmed.trim()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let command = match v.get("command") {
        Some(Value::String(s)) => normalize_picker_command(s),
        _ => None,
    };
    let rationale = v
        .get("rationale")
        .and_then(|x| x.as_str())
        .unwrap_or("(no rationale)")
        .trim()
        .to_string();
    Ok((command, rationale))
}

fn normalize_picker_command(raw: &str) -> Option<String> {
    let command = raw.trim();
    if command.is_empty() {
        return None;
    }

    match command.to_ascii_lowercase().as_str() {
        "null" | "none" | "nil" | "n/a" | "na" => None,
        _ => Some(command.to_string()),
    }
}

fn test_command_or_fallback(command: Option<&str>) -> String {
    command
        .and_then(normalize_picker_command)
        .unwrap_or_else(|| FALLBACK_COMMAND.to_string())
}

fn truncate_for_row(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(3).max(1);
    let mut out: String = chars.into_iter().take(keep).collect();
    out.push_str("...");
    out
}

fn format_run_user_message(
    out: &vng::VngOutput,
    exit_status_str: &str,
    command: &str,
    rationale: &str,
    run_timeout: Duration,
) -> String {
    let timeout_header = if out.timed_out {
        format!(
            "VNG_TIMED_OUT=true\nVNG_TIMEOUT_SECONDS={}\nNOTE=The system did not complete the chosen command within the budget; vng was killed and the captured output below is whatever the guest produced before the kill (it may be empty or end mid-line). Treat this as a Critical hang and report any panic / lockup / deadlock evidence visible in the partial output.\n",
            run_timeout.as_secs(),
        )
    } else {
        String::new()
    };
    format!(
        "RAN_COMMAND={cmd}\nPICKER_RATIONALE={rationale}\n{timeout_header}VNG_EXIT_STATUS={exit}\nORIGINAL_LOG_CHARS={orig}\nKEPT_LOG_CHARS={kept}\n--- CAPTURED OUTPUT (trailing slice of vng combined stdout/stderr) ---\n{log}",
        cmd = command,
        rationale = rationale,
        exit = exit_status_str,
        orig = out.original_chars,
        kept = out.log_tail.chars().count(),
        log = out.log_tail,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_json_parses_command_and_rationale() {
        let raw = r#"{"command":"dmesg | head","rationale":"smoke check"}"#;
        let (cmd, rat) = parse_picker_json(raw).unwrap();
        assert_eq!(cmd.as_deref(), Some("dmesg | head"));
        assert_eq!(rat, "smoke check");
    }

    #[test]
    fn picker_json_null_command_signals_fallback() {
        let raw = r#"{"command":null,"rationale":"doc-only change"}"#;
        let (cmd, rat) = parse_picker_json(raw).unwrap();
        assert!(cmd.is_none());
        assert_eq!(rat, "doc-only change");
    }

    #[test]
    fn picker_json_string_null_command_signals_fallback() {
        let raw = r#"{"command":"null","rationale":"no useful quick test"}"#;
        let (cmd, rat) = parse_picker_json(raw).unwrap();
        assert!(cmd.is_none());
        assert_eq!(rat, "no useful quick test");
    }

    #[test]
    fn picker_json_string_none_command_signals_fallback() {
        let raw = r#"{"command":" None ","rationale":"no useful quick test"}"#;
        let (cmd, _) = parse_picker_json(raw).unwrap();
        assert!(cmd.is_none());
    }

    #[test]
    fn picker_json_empty_string_command_treated_as_null() {
        let raw = r#"{"command":"   ","rationale":"can't think of one"}"#;
        let (cmd, _) = parse_picker_json(raw).unwrap();
        assert!(cmd.is_none());
    }

    #[test]
    fn final_command_selection_falls_back_for_absent_command() {
        assert_eq!(test_command_or_fallback(None), FALLBACK_COMMAND);
    }

    #[test]
    fn final_command_selection_falls_back_for_string_null() {
        assert_eq!(test_command_or_fallback(Some(" null ")), FALLBACK_COMMAND);
    }

    #[test]
    fn final_command_selection_keeps_real_command() {
        assert_eq!(test_command_or_fallback(Some(" uname -r ")), "uname -r");
    }

    #[test]
    fn picker_json_inside_markdown_fence() {
        let raw = "```json\n{\"command\":\"uname -r\",\"rationale\":\"ok\"}\n```";
        let (cmd, _) = parse_picker_json(raw).unwrap();
        assert_eq!(cmd.as_deref(), Some("uname -r"));
    }

    #[test]
    fn truncate_row_short_passthrough() {
        assert_eq!(truncate_for_row("dmesg", 60), "dmesg");
    }

    #[test]
    fn truncate_row_long_gets_ellipsis() {
        let long = "a".repeat(80);
        let out = truncate_for_row(&long, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("..."));
    }
}
