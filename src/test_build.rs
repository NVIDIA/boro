// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `boro build`: per-commit driver.
//!
//! Build the kernel inside the per-commit worktree with `vng -b`, then ask the model to triage
//! the captured log. Emits the same `findings[]` JSON shape as `boro review`, plus a top-level
//! `build_status` so the human report can mark the verdict.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use crate::api::{self, StageUsage, TokenUsage};
use crate::config::ResolvedModel;
use crate::git;
use crate::kconfig;
use crate::progress::WorkerLineCtx;
use crate::snapshot::SnapshotPublisher;
use crate::verbose::VerboseDest;
use crate::vng;

const KCONFIG_STEP: &str = "kconfig fragment";
const VNG_BUILD_STEP: &str = "vng -b";
const BUILD_REVIEW_STEP: &str = "build review";

#[allow(clippy::too_many_arguments)]
pub async fn commit_test_build(
    sha: &str,
    effective_repo: &Path,
    client: &reqwest::Client,
    model: &ResolvedModel,
    vd: &VerboseDest,
    dry_run: bool,
    worker_ctx: Option<&WorkerLineCtx>,
    publisher: &SnapshotPublisher,
) -> Result<Value> {
    if dry_run {
        vd.line("build dry run: skipping `vng -b` and model call");
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
    // Hold the tempfile open until vng -b has finished reading it. Dropping it earlier deletes
    // the backing file from disk before vng reads it.
    drop(kconfig_stage.file);

    let exit_status_str = match build.exit_status {
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
            Some(format!("build failed: exit={exit_status_str}"))
        },
    });

    vd.line(format!(
        "vng -b: exit={exit_status_str} log={} chars (kept tail {} chars)",
        build.original_chars,
        build.log_tail.chars().count(),
    ));

    let user_msg = format_build_user_message(&build, &exit_status_str);
    let (raw, usage, llm_wall, llm_err) = call_model(
        client,
        model,
        api::SYSTEM_TEST_BUILD,
        &user_msg,
        BUILD_REVIEW_STEP,
        vd,
        worker_ctx,
    )
    .await;

    publisher.add_stage(StageUsage {
        step: BUILD_REVIEW_STEP,
        usage,
        wall: llm_wall,
        error: llm_err.clone(),
    });

    let findings = match raw.as_deref() {
        Some(text) => parse_findings_or_fallback(text, vd),
        None => json!([]),
    };

    publisher.set_findings(json!({ "findings": findings.clone() }));

    let build_status = if build_ok { "ok" } else { "failed" };
    let total_usage = json!({
        "prompt_tokens": (kconfig_stage.usage.prompt.unwrap_or(0) + usage.prompt.unwrap_or(0)),
        "completion_tokens": (kconfig_stage.usage.completion.unwrap_or(0) + usage.completion.unwrap_or(0)),
        "api_calls": if raw.is_some() { 2 } else { 1 },
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
            "error": if build_ok { None } else { Some(format!("build failed: exit={exit_status_str}")) },
        },
        {
            "step": BUILD_REVIEW_STEP,
            "prompt_tokens": usage.prompt,
            "completion_tokens": usage.completion,
            "wall_ms": llm_wall.as_millis() as u64,
            "error": llm_err,
        },
    ]);

    Ok(json!({
        "sha": sha,
        "findings": findings,
        "usage": total_usage,
        "usage_steps": usage_steps,
        "build_status": build_status,
        "kconfig_options": kconfig_stage.lines,
    }))
}

fn format_build_user_message(out: &vng::VngOutput, exit_status_str: &str) -> String {
    format!(
        "EXIT_STATUS={exit}\nORIGINAL_LOG_CHARS={orig}\nKEPT_LOG_CHARS={kept}\n--- BUILD LOG (trailing slice) ---\n{log}",
        exit = exit_status_str,
        orig = out.original_chars,
        kept = out.log_tail.chars().count(),
        log = out.log_tail,
    )
}

pub(crate) async fn call_model(
    client: &reqwest::Client,
    model: &ResolvedModel,
    system: &str,
    user: &str,
    step_label: &str,
    vd: &VerboseDest,
    worker_ctx: Option<&WorkerLineCtx>,
) -> (Option<String>, TokenUsage, Duration, Option<String>) {
    if let Some(w) = worker_ctx {
        w.set_line_message(step_label.to_string());
    }
    let t = Instant::now();
    let res = api::chat_completion(
        client,
        model,
        system,
        user,
        model.temperature,
        Some(step_label),
        None,
        vd,
        None,
        worker_ctx,
        std::path::Path::new("."),
    )
    .await;
    let wall = t.elapsed();
    match res {
        Ok((text, usage)) => {
            if let Some(w) = worker_ctx {
                w.record_tokens(
                    usage.prompt,
                    usage.completion,
                    usage.cache_creation,
                    usage.cache_read,
                );
            }
            (Some(text), usage, wall, None)
        }
        Err(e) => {
            let reason = api::short_error_reason(&e);
            vd.line(format!("{step_label}: model call failed ({reason})"));
            (None, TokenUsage::default(), wall, Some(reason))
        }
    }
}

pub(crate) fn parse_findings_or_fallback(raw: &str, vd: &VerboseDest) -> Value {
    match api::parse_findings_json(raw) {
        Ok(v) => v.get("findings").cloned().unwrap_or_else(|| json!([])),
        Err(e) => {
            vd.line(format!(
                "warning: model output was not valid findings JSON ({e:#}); falling back to a single Info finding"
            ));
            json!([
                {
                    "problem": "model output could not be parsed as findings JSON",
                    "severity": "Info",
                    "severity_explanation": format!("raw response head: {}", raw.chars().take(400).collect::<String>()),
                }
            ])
        }
    }
}

/// Generate the per-commit kconfig fragment, recording the stage in the publisher and returning
/// the result so the caller can pass the tempfile to `vng -b` and account for tokens spent.
///
/// Always returns: a stage with `file: None` is the fallback (run `vng -b` without `--config`).
pub(crate) async fn build_kconfig_stage(
    sha: &str,
    effective_repo: &Path,
    client: &reqwest::Client,
    model: &ResolvedModel,
    vd: &VerboseDest,
    worker_ctx: Option<&WorkerLineCtx>,
    publisher: &SnapshotPublisher,
) -> kconfig::KconfigStage {
    if let Some(w) = worker_ctx {
        w.set_line_message(KCONFIG_STEP);
    }
    let diff = match git::show_patch_diff_only(effective_repo, sha) {
        Ok(d) => d,
        Err(e) => {
            // No diff → no informed config selection; fall back to default config.
            let reason = format!("git show diff failed: {e:#}");
            vd.line(format!(
                "{KCONFIG_STEP}: {reason}; falling back to default config"
            ));
            let stage = kconfig::KconfigStage {
                file: None,
                lines: Vec::new(),
                usage: TokenUsage::default(),
                wall: Duration::from_millis(0),
                error: Some(reason),
            };
            publisher.add_stage(StageUsage {
                step: KCONFIG_STEP,
                usage: stage.usage,
                wall: stage.wall,
                error: stage.error.clone(),
            });
            return stage;
        }
    };
    // Changed paths feed kselftest config discovery inside `generate_fragment`. Best-effort:
    // failure here just means the kselftest merge step has no candidates to consider.
    let changed_paths = git::changed_paths(effective_repo, sha).unwrap_or_else(|e| {
        vd.line(format!(
            "{KCONFIG_STEP}: changed_paths failed ({e:#}); skipping kselftest config merge"
        ));
        Vec::new()
    });
    let stage = kconfig::generate_fragment(
        client,
        model,
        &diff,
        &changed_paths,
        KCONFIG_STEP,
        vd,
        worker_ctx,
        effective_repo,
    )
    .await;
    publisher.add_stage(StageUsage {
        step: KCONFIG_STEP,
        usage: stage.usage,
        wall: stage.wall,
        error: stage.error.clone(),
    });
    stage
}
