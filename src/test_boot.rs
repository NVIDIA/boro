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
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use crate::api::{self, StageUsage, TokenUsage};
use crate::config::ResolvedModel;
use crate::git;
use crate::kconfig;
use crate::progress::WorkerLineCtx;
use crate::snapshot::SnapshotPublisher;
use crate::test_build::{build_kconfig_stage, call_model, parse_findings_or_fallback};
use crate::verbose::VerboseDest;
use crate::vng;

const KCONFIG_STEP: &str = "kconfig fragment";
const VNG_BUILD_STEP: &str = "vng -b";
const TEST_PICKER_STEP: &str = "test picker";
const TEST_PLAN_STEP: &str = "test plan";
const VNG_RUN_STEP: &str = "vng run";
const TEST_REVIEW_STEP: &str = "test review";

/// What we run inside the VM when the picker can't think of anything useful.
const FALLBACK_COMMAND: &str = "dmesg";
const MAX_CONFIG_CONTEXT_LINES: usize = 200;
const MAX_CONFIG_CONTEXT_CHARS: usize = 60_000;
const MAX_RANGE_PLAN_PATCH_CHARS: usize = 300_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestTarget {
    Commit,
    CommitRange { range: String, commits: Vec<String> },
    Config(ConfigTestTarget),
}

impl TestTarget {
    pub fn from_config_arg(raw: &str) -> Result<Self> {
        Ok(Self::Config(parse_config_test_target(raw)?))
    }

    pub fn is_config(&self) -> bool {
        matches!(self, Self::Config(_))
    }

    pub fn uses_commit_metadata(&self) -> bool {
        matches!(self, Self::Commit)
    }

    pub fn display_name(&self, commit_arg: &str) -> String {
        match self {
            Self::Commit => git::normalize_commit_range_arg(commit_arg),
            Self::CommitRange { range, .. } => range.clone(),
            Self::Config(cfg) => cfg.config_line.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigTestTarget {
    symbol: String,
    config_line: String,
}

impl ConfigTestTarget {
    fn kconfig_name(&self) -> &str {
        self.symbol.trim_start_matches("CONFIG_")
    }
}

fn parse_config_test_target(raw: &str) -> Result<ConfigTestTarget> {
    let s = raw.trim();
    if s.is_empty() {
        anyhow::bail!("empty CONFIG_ option");
    }

    if let Some(rest) = s.strip_prefix("# CONFIG_") {
        let Some(name) = rest.strip_suffix(" is not set") else {
            anyhow::bail!(
                "invalid disabled CONFIG_ form {s:?}; expected `# CONFIG_FOO is not set`"
            );
        };
        validate_config_name(name)?;
        let symbol = format!("CONFIG_{name}");
        return Ok(ConfigTestTarget {
            symbol,
            config_line: s.to_string(),
        });
    }

    let Some(rest) = s.strip_prefix("CONFIG_") else {
        anyhow::bail!("expected CONFIG_ option, got {s:?}");
    };
    let (name, value) = match rest.split_once('=') {
        Some((name, value)) => (name, Some(value.trim())),
        None => (rest, None),
    };
    validate_config_name(name)?;

    let value = match value {
        Some(v) => validate_config_value(name, v)?,
        None => "y",
    };

    let symbol = format!("CONFIG_{name}");
    Ok(ConfigTestTarget {
        config_line: format!("{symbol}={value}"),
        symbol,
    })
}

fn validate_config_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!("invalid CONFIG_ option name {name:?}");
    }
    Ok(())
}

fn validate_config_value<'a>(name: &str, value: &'a str) -> Result<&'a str> {
    if value.is_empty() {
        anyhow::bail!("invalid value for CONFIG_{name}: value must not be empty");
    }
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!("invalid value for CONFIG_{name}: value must be a single line");
    }
    Ok(value)
}

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
    target: &TestTarget,
    vd: &VerboseDest,
    dry_run: bool,
    plan_only: bool,
    run_timeout: Duration,
    worker_ctx: Option<&WorkerLineCtx>,
    publisher: &SnapshotPublisher,
) -> Result<Value> {
    if dry_run {
        vd.line("test dry run: skipping `vng -b` / `vng -r` and model call");
        return Ok(with_test_target_fields(
            json!({
                "sha": sha,
                "dry_run": true,
                "findings": [],
            }),
            target,
        ));
    }

    if plan_only {
        let picker =
            pick_test_plan(sha, effective_repo, target, client, model, vd, worker_ctx).await;
        publisher.add_stage(StageUsage {
            step: TEST_PLAN_STEP,
            usage: picker.usage,
            wall: picker.wall,
            error: picker.error.clone(),
        });
        vd.line(format!(
            "test plan: chose `{}` (rationale: {})",
            picker.plan.command, picker.plan.rationale
        ));

        let api_calls = if picker.usage.prompt.is_some() || picker.usage.completion.is_some() {
            1
        } else {
            0
        };
        let total_usage = json!({
            "prompt_tokens": picker.usage.prompt.unwrap_or(0),
            "completion_tokens": picker.usage.completion.unwrap_or(0),
            "api_calls": api_calls,
        });
        let usage_steps = json!([
            {
                "step": TEST_PLAN_STEP,
                "prompt_tokens": picker.usage.prompt,
                "completion_tokens": picker.usage.completion,
                "wall_ms": picker.wall.as_millis() as u64,
                "error": picker.error,
            }
        ]);
        let findings = json!([]);
        publisher.set_findings(json!({ "findings": findings.clone() }));

        return Ok(with_test_target_fields(
            json!({
                "sha": sha,
                "plan": true,
                "findings": findings,
                "usage": total_usage,
                "usage_steps": usage_steps,
                "build_status": "skipped",
                "boot_status": "planned",
                "test_command": picker.plan.command,
                "test_description": picker.plan.description,
                "test_rationale": picker.plan.rationale,
                "test_plan": picker.plan.to_json(),
            }),
            target,
        ));
    }

    if matches!(target, TestTarget::CommitRange { .. }) {
        anyhow::bail!("commit range test targets are only supported with --plan");
    }

    let kconfig_stage = match target {
        TestTarget::Commit => {
            build_kconfig_stage(
                sha,
                effective_repo,
                client,
                model,
                vd,
                worker_ctx,
                publisher,
            )
            .await
        }
        TestTarget::Config(cfg) => {
            if let Some(w) = worker_ctx {
                w.set_line_message(KCONFIG_STEP);
            }
            let stage =
                kconfig::fragment_from_lines(vec![cfg.config_line.clone()], KCONFIG_STEP, vd);
            publisher.add_stage(StageUsage {
                step: KCONFIG_STEP,
                usage: stage.usage,
                wall: stage.wall,
                error: stage.error.clone(),
            });
            stage
        }
        TestTarget::CommitRange { .. } => unreachable!("guarded above"),
    };

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
        return Ok(with_test_target_fields(
            json!({
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
            }),
            target,
        ));
    }

    // Build succeeded — pick a quick test command.
    let picker =
        pick_test_command(sha, effective_repo, target, client, model, vd, worker_ctx).await;
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
        target,
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

    Ok(with_test_target_fields(
        json!({
            "sha": sha,
            "findings": findings,
            "usage": total_usage,
            "usage_steps": usage_steps,
            "build_status": "ok",
            "boot_status": boot_status,
            "test_command": chosen_command,
            "test_summary": model_summary,
            "kconfig_options": kconfig_stage.lines,
        }),
        target,
    ))
}

fn with_test_target_fields(mut obj: Value, target: &TestTarget) -> Value {
    match target {
        TestTarget::Commit => {}
        TestTarget::CommitRange { range, commits } => {
            obj["sha"] = json!("range");
            obj["test_target"] = json!("range");
            obj["range"] = json!(range);
            obj["range_commit_count"] = json!(commits.len());
            obj["range_commits"] = json!(commits);
            obj["subject"] = json!(range);
        }
        TestTarget::Config(cfg) => {
            obj["test_target"] = json!("config");
            obj["config_option"] = json!(cfg.symbol);
            obj["config_fragment"] = json!(cfg.config_line);
            obj["subject"] = json!(cfg.config_line);
        }
    }
    obj
}

struct PickerStage {
    command: Option<String>,
    rationale: String,
    usage: TokenUsage,
    wall: Duration,
    error: Option<String>,
}

struct PlanStage {
    plan: TestPlan,
    usage: TokenUsage,
    wall: Duration,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct TestPlan {
    command: String,
    description: String,
    script: Option<String>,
    requirements: Vec<String>,
    steps: Vec<String>,
    expected_results: Vec<String>,
    rationale: String,
}

impl TestPlan {
    fn from_picker(command: Option<String>, rationale: String) -> Self {
        let command = command
            .and_then(|c| normalize_picker_command(&c))
            .unwrap_or_else(|| FALLBACK_COMMAND.to_string());
        let description = if rationale.trim().is_empty() {
            format!("Run `{command}` and inspect the output for regressions related to this patch.")
        } else {
            rationale.trim().to_string()
        };
        Self {
            command,
            description,
            script: None,
            requirements: Vec::new(),
            steps: Vec::new(),
            expected_results: Vec::new(),
            rationale,
        }
    }

    fn fallback(reason: impl AsRef<str>) -> Self {
        let reason = reason.as_ref().trim();
        let description = if reason.is_empty() {
            "Plan generation failed. As a fallback, boot the patched kernel and inspect the full dmesg for warnings, splats, or subsystem-specific regressions.".to_string()
        } else {
            format!(
                "Plan generation failed ({reason}). As a fallback, boot the patched kernel and inspect the full dmesg for warnings, splats, or subsystem-specific regressions."
            )
        };
        Self {
            command: FALLBACK_COMMAND.to_string(),
            description,
            script: None,
            requirements: vec!["A bootable kernel built from the patched tree".to_string()],
            steps: vec![
                "Build and boot the patched kernel".to_string(),
                "Run `dmesg` after boot".to_string(),
                "Inspect the log for warnings, crashes, lockdep reports, or subsystem-specific errors"
                    .to_string(),
            ],
            expected_results: vec![
                "The kernel boots cleanly".to_string(),
                "dmesg contains no new warnings, oopses, BUG splats, or regressions tied to the changed code"
                    .to_string(),
            ],
            rationale: "The model did not produce a usable detailed plan, so the safest generic smoke test is to boot the kernel and inspect the full log.".to_string(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "command": &self.command,
            "description": &self.description,
            "script": &self.script,
            "requirements": &self.requirements,
            "steps": &self.steps,
            "expected_results": &self.expected_results,
            "rationale": &self.rationale,
        })
    }
}

/// Single-shot model call that proposes a quick test command for this commit. Falls back to
/// `command: None` (→ `dmesg` at the call site) on API or parse failure; never aborts the run.
async fn pick_test_command(
    sha: &str,
    effective_repo: &Path,
    target: &TestTarget,
    client: &reqwest::Client,
    model: &ResolvedModel,
    vd: &VerboseDest,
    worker_ctx: Option<&WorkerLineCtx>,
) -> PickerStage {
    let user_msg = build_picker_user_message(sha, effective_repo, target, vd);
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

/// Single-shot model call for `boro test --plan`. This intentionally uses a different prompt from
/// the executable picker: plan mode is allowed to describe complex, long-running, or hardware-backed
/// tests because boro will not run the result.
async fn pick_test_plan(
    sha: &str,
    effective_repo: &Path,
    target: &TestTarget,
    client: &reqwest::Client,
    model: &ResolvedModel,
    vd: &VerboseDest,
    worker_ctx: Option<&WorkerLineCtx>,
) -> PlanStage {
    let user_msg = build_picker_user_message(sha, effective_repo, target, vd);
    let (raw, usage, wall, err) = call_model(
        client,
        model,
        api::system_test_plan_picker(),
        &user_msg,
        TEST_PLAN_STEP,
        vd,
        worker_ctx,
    )
    .await;
    let Some(text) = raw else {
        return PlanStage {
            plan: TestPlan::fallback(format!(
                "plan call failed: {}",
                err.as_deref().unwrap_or("unknown error")
            )),
            usage,
            wall,
            error: err,
        };
    };

    match parse_test_plan_json(&text) {
        Ok(plan) => PlanStage {
            plan,
            usage,
            wall,
            error: err,
        },
        Err(parse_err) => {
            vd.line(format!(
                "test plan: parse failed ({parse_err}); trying command/rationale fallback"
            ));
            let plan = match parse_picker_json(&text) {
                Ok((cmd, rationale)) => TestPlan::from_picker(cmd, rationale),
                Err(_) => TestPlan::fallback(format!("plan output unparseable: {parse_err}")),
            };
            PlanStage {
                plan,
                usage,
                wall,
                error: err.or_else(|| Some(format!("parse: {parse_err}"))),
            }
        }
    }
}

fn build_picker_user_message(
    sha: &str,
    effective_repo: &Path,
    target: &TestTarget,
    vd: &VerboseDest,
) -> String {
    match target {
        TestTarget::Commit => build_commit_picker_user_message(sha, effective_repo, vd),
        TestTarget::CommitRange { range, commits } => {
            build_range_picker_user_message(range, commits, effective_repo, vd)
        }
        TestTarget::Config(cfg) => build_config_picker_user_message(cfg, effective_repo, vd),
    }
}

fn build_commit_picker_user_message(sha: &str, effective_repo: &Path, vd: &VerboseDest) -> String {
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

fn build_range_picker_user_message(
    range: &str,
    commits: &[String],
    effective_repo: &Path,
    vd: &VerboseDest,
) -> String {
    let mut commit_lines = Vec::new();
    let mut all_paths = std::collections::BTreeSet::new();
    let mut patch_blocks = Vec::new();

    for sha in commits {
        let short = short_sha_for_context(sha);
        let subject = git::commit_subject(effective_repo, sha).unwrap_or_else(|e| {
            vd.line(format!(
                "test plan: subject lookup failed for {short} ({e:#})"
            ));
            "(subject unavailable)".to_string()
        });
        let paths = git::changed_paths(effective_repo, sha).unwrap_or_else(|e| {
            vd.line(format!(
                "test plan: changed_paths failed for {short} ({e:#})"
            ));
            Vec::new()
        });
        for path in &paths {
            all_paths.insert(path.clone());
        }
        let path_summary = if paths.is_empty() {
            "(no changed paths)".to_string()
        } else {
            paths.join(", ")
        };
        commit_lines.push(format!("{short} {subject}\n  files: {path_summary}"));

        let patch = git::show_patch(effective_repo, sha).unwrap_or_else(|e| {
            vd.line(format!(
                "test plan: git show failed for {short} ({e:#}); continuing without that patch"
            ));
            String::new()
        });
        if !patch.trim().is_empty() {
            patch_blocks.push(format!("--- COMMIT {short}: {subject} ---\n{patch}"));
        }
    }

    let commit_list = if commit_lines.is_empty() {
        "(none)".to_string()
    } else {
        commit_lines.join("\n")
    };
    let path_list = if all_paths.is_empty() {
        "(none)".to_string()
    } else {
        all_paths.into_iter().collect::<Vec<_>>().join("\n")
    };
    let patches = if patch_blocks.is_empty() {
        "(none)".to_string()
    } else {
        api::cap_utf8(&patch_blocks.join("\n\n"), MAX_RANGE_PLAN_PATCH_CHARS)
    };

    format!(
        "TEST_TARGET=COMMIT_RANGE\nCOMMIT_RANGE={range}\nCOMMIT_COUNT={count}\n\nCOMMITS:\n{commit_list}\n\nCHANGED_FILES_ACROSS_RANGE:\n{path_list}\n\n--- PATCH SERIES ({range}) ---\n{patches}",
        count = commits.len(),
    )
}

fn short_sha_for_context(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn build_config_picker_user_message(
    cfg: &ConfigTestTarget,
    effective_repo: &Path,
    vd: &VerboseDest,
) -> String {
    let definitions = kconfig_definition_context(effective_repo, cfg.kconfig_name(), vd);
    let references = config_reference_context(effective_repo, &cfg.symbol, vd);
    format!(
        "TEST_TARGET=CONFIG\nCONFIG_OPTION={symbol}\nCONFIG_FRAGMENT_LINE={line}\nKCONFIG_SYMBOL={kconfig}\n\n\
The kernel will be built from HEAD with CONFIG_FRAGMENT_LINE merged into virtme-ng's default config. Pick a quick test command that best exercises this config option, or return null for plain dmesg when no focused quick test exists.\n\n\
--- KCONFIG DEFINITIONS ---\n{definitions}\n\n\
--- REFERENCES TO {symbol} ---\n{references}",
        symbol = cfg.symbol,
        line = cfg.config_line,
        kconfig = cfg.kconfig_name(),
        definitions = definitions,
        references = references,
    )
}

fn kconfig_definition_context(repo: &Path, kconfig_name: &str, vd: &VerboseDest) -> String {
    let pattern =
        format!("^[[:space:]]*(config|menuconfig)[[:space:]]+{kconfig_name}([[:space:]]|$)");
    let lines = git_grep_lines(
        repo,
        &["grep", "-n", "-E", &pattern],
        "kconfig definition",
        vd,
    );
    let mut blocks = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();
    for line in lines {
        let Some((path, line_no, _text)) = parse_git_grep_line(&line) else {
            continue;
        };
        if !path.contains("Kconfig") || !seen_paths.insert((path.to_string(), line_no)) {
            continue;
        }
        if let Some(block) = read_context_block(repo, path, line_no, 80) {
            blocks.push(format!("{path}:{line_no}\n{block}"));
        }
        if blocks.len() >= 6 {
            break;
        }
    }
    if blocks.is_empty() {
        "(none found)".to_string()
    } else {
        join_capped_lines(blocks.join("\n\n").lines().map(ToOwned::to_owned))
    }
}

fn config_reference_context(repo: &Path, symbol: &str, vd: &VerboseDest) -> String {
    let lines = git_grep_lines(repo, &["grep", "-n", "-I", symbol], "config references", vd);
    join_capped_lines(lines.into_iter())
}

fn git_grep_lines(repo: &Path, args: &[&str], label: &str, vd: &VerboseDest) -> Vec<String> {
    let out = Command::new("git").current_dir(repo).args(args).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.trim().is_empty() {
                vd.line(format!(
                    "test picker: git grep {label} failed: {}",
                    stderr.trim()
                ));
            }
            Vec::new()
        }
        Err(e) => {
            vd.line(format!("test picker: git grep {label} failed: {e:#}"));
            Vec::new()
        }
    }
}

fn parse_git_grep_line(line: &str) -> Option<(&str, usize, &str)> {
    let mut parts = line.splitn(3, ':');
    let path = parts.next()?;
    let line_no = parts.next()?.parse().ok()?;
    let text = parts.next().unwrap_or("");
    Some((path, line_no, text))
}

fn read_context_block(
    repo: &Path,
    path: &str,
    start_line: usize,
    max_lines: usize,
) -> Option<String> {
    let text = std::fs::read_to_string(repo.join(path)).ok()?;
    let start = start_line.saturating_sub(1);
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate().skip(start) {
        if idx > start && is_kconfig_entry_start(line) {
            break;
        }
        out.push(line.to_string());
        if out.len() >= max_lines {
            out.push("[... truncated ...]".to_string());
            break;
        }
    }
    Some(out.join("\n"))
}

fn is_kconfig_entry_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("config ") || trimmed.starts_with("menuconfig ")
}

fn join_capped_lines(lines: impl IntoIterator<Item = String>) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut truncated = false;
    for line in lines {
        let extra = line.len() + usize::from(!out.is_empty());
        if count >= MAX_CONFIG_CONTEXT_LINES || out.len() + extra > MAX_CONFIG_CONTEXT_CHARS {
            truncated = true;
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
        count += 1;
    }
    if out.is_empty() {
        return "(none found)".to_string();
    }
    if truncated {
        out.push_str("\n[... truncated ...]");
    }
    out
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

fn parse_test_plan_json(raw: &str) -> Result<TestPlan> {
    let trimmed = api::strip_json_fences(raw);
    let v: Value = serde_json::from_str(trimmed.trim()).map_err(|e| anyhow::anyhow!("{e}"))?;
    if !v.is_object() {
        anyhow::bail!("test plan must be a JSON object");
    }

    let command = json_string(&v, "command")
        .and_then(|s| normalize_picker_command(&s))
        .or_else(|| json_string_array(&v, "commands").into_iter().next())
        .unwrap_or_else(|| "see steps below".to_string());
    let rationale = json_string(&v, "rationale").unwrap_or_default();
    let mut description = json_string(&v, "description").unwrap_or_default();
    let script = json_string(&v, "script").or_else(|| json_string(&v, "test_script"));
    let requirements = json_string_array(&v, "requirements");
    let steps = json_string_array(&v, "steps");
    let mut expected_results = json_string_array(&v, "expected_results");
    if expected_results.is_empty() {
        expected_results = json_string_array(&v, "expected");
    }
    if expected_results.is_empty() {
        expected_results = json_string_array(&v, "expected_signal");
    }

    if description.is_empty() {
        description = if rationale.is_empty() {
            format!("Run `{command}` as the primary test for this patch.")
        } else {
            rationale.clone()
        };
    }
    let rationale = if rationale.is_empty() {
        description.clone()
    } else {
        rationale
    };

    Ok(TestPlan {
        command,
        description,
        script,
        requirements,
        steps,
        expected_results,
        rationale,
    })
}

fn json_string(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn json_string_array(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Some(Value::String(s)) => s
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.trim_start_matches(|c: char| {
                    c == '-' || c == '*' || c == '+' || c == '.' || c.is_ascii_digit()
                })
                .trim()
                .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
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
    target: &TestTarget,
) -> String {
    let target_header = match target {
        TestTarget::Commit => String::new(),
        TestTarget::CommitRange { range, commits } => format!(
            "TEST_TARGET=range\nCOMMIT_RANGE={range}\nCOMMIT_COUNT={}\n",
            commits.len()
        ),
        TestTarget::Config(cfg) => format!(
            "TEST_TARGET=config\nCONFIG_OPTION={}\nCONFIG_FRAGMENT_LINE={}\n",
            cfg.symbol, cfg.config_line
        ),
    };
    let timeout_header = if out.timed_out {
        format!(
            "VNG_TIMED_OUT=true\nVNG_TIMEOUT_SECONDS={}\nNOTE=The system did not complete the chosen command within the budget; vng was killed and the captured output below is whatever the guest produced before the kill (it may be empty or end mid-line). Treat this as a Critical hang and report any panic / lockup / deadlock evidence visible in the partial output.\n",
            run_timeout.as_secs(),
        )
    } else {
        String::new()
    };
    format!(
        "{target_header}RAN_COMMAND={cmd}\nPICKER_RATIONALE={rationale}\n{timeout_header}VNG_EXIT_STATUS={exit}\nORIGINAL_LOG_CHARS={orig}\nKEPT_LOG_CHARS={kept}\n--- CAPTURED OUTPUT (trailing slice of vng combined stdout/stderr) ---\n{log}",
        cmd = command,
        rationale = rationale,
        target_header = target_header,
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
    fn config_target_defaults_to_enabled() {
        let target = TestTarget::from_config_arg("CONFIG_SCHED_CLASS_EXT").unwrap();
        assert_eq!(
            target,
            TestTarget::Config(ConfigTestTarget {
                symbol: "CONFIG_SCHED_CLASS_EXT".to_string(),
                config_line: "CONFIG_SCHED_CLASS_EXT=y".to_string(),
            })
        );
        assert_eq!(target.display_name("HEAD"), "CONFIG_SCHED_CLASS_EXT=y");
    }

    #[test]
    fn config_target_accepts_explicit_tristate_values() {
        let target = TestTarget::from_config_arg("CONFIG_BPF=m").unwrap();
        assert_eq!(
            target,
            TestTarget::Config(ConfigTestTarget {
                symbol: "CONFIG_BPF".to_string(),
                config_line: "CONFIG_BPF=m".to_string(),
            })
        );
    }

    #[test]
    fn config_target_accepts_numeric_values() {
        let target = TestTarget::from_config_arg("CONFIG_NR_CPUS=512").unwrap();
        assert_eq!(
            target,
            TestTarget::Config(ConfigTestTarget {
                symbol: "CONFIG_NR_CPUS".to_string(),
                config_line: "CONFIG_NR_CPUS=512".to_string(),
            })
        );
    }

    #[test]
    fn config_target_accepts_quoted_string_values() {
        let target =
            TestTarget::from_config_arg("CONFIG_CMDLINE=\"console=ttyS0 root=/dev/vda\"").unwrap();
        assert_eq!(
            target,
            TestTarget::Config(ConfigTestTarget {
                symbol: "CONFIG_CMDLINE".to_string(),
                config_line: "CONFIG_CMDLINE=\"console=ttyS0 root=/dev/vda\"".to_string(),
            })
        );
    }

    #[test]
    fn config_target_accepts_disabled_form() {
        let target = TestTarget::from_config_arg("# CONFIG_DEBUG_INFO is not set").unwrap();
        assert_eq!(
            target,
            TestTarget::Config(ConfigTestTarget {
                symbol: "CONFIG_DEBUG_INFO".to_string(),
                config_line: "# CONFIG_DEBUG_INFO is not set".to_string(),
            })
        );
    }

    #[test]
    fn config_target_rejects_empty_value() {
        assert!(TestTarget::from_config_arg("CONFIG_FOO=").is_err());
    }

    #[test]
    fn config_target_rejects_commit_refs() {
        assert!(TestTarget::from_config_arg("HEAD").is_err());
        assert!(TestTarget::from_config_arg("origin/master..HEAD").is_err());
    }

    #[test]
    fn range_target_fields_mark_synthetic_plan_entry() {
        let target = TestTarget::CommitRange {
            range: "HEAD~2..HEAD".to_string(),
            commits: vec!["a".repeat(40), "b".repeat(40)],
        };
        let obj = with_test_target_fields(
            json!({
                "sha": "placeholder",
                "plan": true,
                "findings": [],
            }),
            &target,
        );
        assert_eq!(obj["sha"], "range");
        assert_eq!(obj["test_target"], "range");
        assert_eq!(obj["range"], "HEAD~2..HEAD");
        assert_eq!(obj["range_commit_count"], 2);
        assert_eq!(obj["subject"], "HEAD~2..HEAD");
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
    fn plan_json_parses_detailed_shape() {
        let raw = r##"{
            "command": "make -C tools/testing/selftests run_tests TARGETS=\"net\"",
            "description": "Exercise the affected networking path with the matching selftests.",
            "script": "#!/bin/sh\nmake -C tools/testing/selftests run_tests TARGETS=\"net\" && echo OK || { echo FAIL: net selftests failed; exit 1; }",
            "requirements": ["CONFIG_NET=y", "two network namespaces"],
            "steps": ["build the patched kernel", "run the net selftests"],
            "expected_results": ["all selected tests pass", "dmesg has no WARN splats"],
            "rationale": "The patch touches net core behavior."
        }"##;
        let plan = parse_test_plan_json(raw).unwrap();
        assert!(plan.command.contains("selftests"));
        assert!(plan.script.as_deref().unwrap().contains("echo OK"));
        assert_eq!(plan.requirements.len(), 2);
        assert_eq!(plan.steps[1], "run the net selftests");
        assert!(plan.expected_results[1].contains("dmesg"));
    }

    #[test]
    fn plan_json_never_returns_null_command() {
        let raw = r#"{
            "command": null,
            "description": "Use lab hardware to exercise the changed path.",
            "steps": ["attach the device", "run the vendor stress suite"],
            "expected_signal": "no device reset or kernel warning"
        }"#;
        let plan = parse_test_plan_json(raw).unwrap();
        assert_eq!(plan.command, "see steps below");
        assert_eq!(
            plan.expected_results,
            vec!["no device reset or kernel warning"]
        );
    }

    #[test]
    fn plan_json_accepts_multiline_string_lists() {
        let raw = r#"{
            "command": "see steps below",
            "description": "Manual plan.",
            "requirements": "- target board\n- serial console",
            "steps": "1. boot patched kernel\n2. trigger suspend/resume"
        }"#;
        let plan = parse_test_plan_json(raw).unwrap();
        assert_eq!(plan.requirements, vec!["target board", "serial console"]);
        assert_eq!(
            plan.steps,
            vec!["boot patched kernel", "trigger suspend/resume"]
        );
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
