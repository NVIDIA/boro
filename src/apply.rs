// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::api::{self, CumulativeTokenUsage, TokenUsage, ToolLoopConfig};
use crate::config;
use crate::git;
use crate::prefetch;
use crate::progress::WorkerLineCtx;
use crate::verbose::VerboseDest;

const SYSTEM_APPLY_SUGGEST: &str = r#"You are a Linux kernel backport assistant.
Git reported a diff3 conflict while applying one commit to a target tree.
Your job is to apply PATCH to CODE.

Rules:
- PATCH describes the incoming change from the common base to the commit being applied.
- CODE is the target tree's local side of the conflict.
- Preserve target-tree-only adaptations unless PATCH clearly changes the same logic.
- Indentation and coding style are part of correctness. Preserve leading
  tabs/spaces from CODE and PATCH. If incoming or target code uses a leading
  tab, resolved_code must contain that tab after JSON parsing; escape it as
  `\t` inside the JSON string when needed. Do not left-align indented
  statements or use visible whitespace marker text.
- Vertical whitespace is also part of style. Preserve blank lines that are
  present in CODE or PATCH, but do not add gratuitous empty lines. In
  particular, resolved_code must not start or end with blank lines, and must
  not introduce multiple consecutive blank lines.
- Do not include conflict markers.
- Return ONLY a JSON object with this shape:
{"resolved_code":"string","explanation":"string"}"#;

const SYSTEM_APPLY_VALIDATE: &str = r#"You are a Linux kernel maintainer validating a proposed conflict resolution.
Decide whether the proposed resolved_code correctly applies the incoming patch intent to the target-tree code.

Reject if it drops target-tree-only behavior, fails to apply the incoming change, invents unrelated code, includes conflict markers, is not plausible kernel code, or regresses indentation/coding style.
In particular, reject proposals that drop leading tabs/spaces from statements that remain inside the same block.
Also reject proposals that add gratuitous empty lines. resolved_code must not start or end with blank lines because unchanged context is adjacent to the replacement block. Reject multiple consecutive blank lines that were not present in the conflict sides.
Return ONLY a JSON object with this shape:
{"accepted":true|false,"reason":"string","concerns":["string"]}"#;

const RETRY_REMINDER_SUGGEST: &str = r#"Your previous response was rejected because it did not match the required JSON shape.
Return ONLY a JSON object:
{"resolved_code":"string","explanation":"string"}
No markdown fences, no prose outside the JSON."#;

const RETRY_REMINDER_VALIDATE: &str = r#"Your previous response was rejected because it did not match the required JSON shape.
Return ONLY a JSON object:
{"accepted":true|false,"reason":"string","concerns":["string"]}
No markdown fences, no prose outside the JSON."#;

const SYSTEM_APPLY_POST_REVIEW: &str = r#"You are a Linux kernel maintainer auditing a commit that boro has just cherry-picked into the working tree. The cherry-pick may have succeeded cleanly or after one or more diff3 conflicts were auto-resolved; either way, the commit already exists at HEAD and your job is the broader, post-apply audit.

You have read-only tools (grep_repo, rg when available, read_files, read_symbol, git_blame, git_diff, git_show, run_git) and ONE write tool: edit_file.

The single most common failure mode you must catch is the BACKPORT GAP: the upstream patch references a struct field, function, type, macro, or enum value that exists upstream but was added in a separate commit that is NOT yet in the target tree. The compiler will reject it. The user message includes the full diff; you must verify every newly-referenced identifier exists in the target tree BEFORE you say "clean".

STEP 1 - MANDATORY symbol-resolution check (do this FIRST, on the ENTIRE diff, not just the conflict hunks):
For every IDENTIFIER appearing in a `+` line of the diff - struct/union field access via `->` or `.`, function call, type name, macro, enum value, sysctl/tracepoint name - verify it exists in the target tree at HEAD:
- `expr->FIELD` / `expr.FIELD`: identify the type of `expr` (read the surrounding function to see how it is declared), then call `read_symbol path=<file containing struct> symbol=<struct_name>` and confirm FIELD is present in the body. If the struct definition is in a header, grep_repo for `struct <name> {` to find the file, then read_symbol. If the field is absent from every matching target-tree definition, this is a backport gap and you MUST act.
- `FUNC(...)` calls: grep_repo (fixed_string=true) for the function name; if no prototype or definition exists in the target tree, it's a gap.
- Type names in declarations / casts: grep_repo for the type; if absent, gap.
- Macros and enum values: grep_repo for the identifier; if absent, gap.

When a backport gap is found, you MUST either:
(a) call edit_file to repair it. Typical fixes:
    - remove the offending line if it is purely additive instrumentation that the rest of the commit does not depend on,
    - replace the reference with the closest target-tree equivalent (e.g. use the existing field that the upstream rename came from),
    - guard the reference with the appropriate `#ifdef CONFIG_*` block when the field/symbol is conditional and the surrounding code already uses that config gate; OR
(b) return "needs_human" with the missing symbol and its file:line in the explanation.

Do NOT return "clean" while any symbol referenced by a `+` line is absent from the target tree.

STEP 2 - sanity-check the auto-resolved hunks (when there are any): for each hunk listed in the user message, confirm callers in adjacent functions were updated where the upstream patch required it, and that the resolution did not drop target-tree-only code (UBUNTU/SAUCE, vendor adaptations, debug instrumentation) that the rest of the file still references. These hunks are the highest-risk surface for the conflict resolver's mistakes, but they are NOT the only place backport gaps can hide - STEP 1 is non-negotiable regardless of conflict location.

Constraints:
- Do NOT use edit_file for stylistic preferences, speculative refactors, or pre-existing issues unrelated to the cherry-pick.
- Make every edit as small as possible and tied to a concrete defect.
- Preserve vertical whitespace when editing. Do not add gratuitous blank lines;
  remove extra blank lines introduced by the auto-resolution when they are part
  of the defect.
- After your final answer the host will run full `git status --porcelain=v1 -z --untracked-files=all`, reject untracked files, stage tracked modifications, then run `git commit --amend --no-edit`. If you are unsure how to fix something safely, leave it and use "needs_human".

When done, respond with ONLY this JSON object:
{"verdict":"clean"|"amended"|"needs_human","explanation":"string"}

- "clean": you completed STEP 1 and STEP 2 and found no defect. No edit_file calls made.
- "amended": you called edit_file one or more times; the explanation MUST list the files and what you fixed and why (include the symbol name and source/target field where applicable).
- "needs_human": you found a problem you could not safely fix with edit_file (e.g. missing struct field with no obvious equivalent, needs a design decision); the explanation MUST describe what is wrong and where (file:line plus the offending identifier).

No markdown fences. No prose outside the JSON."#;

const RETRY_REMINDER_POST_REVIEW: &str = r#"Your previous response was rejected because it did not match the required JSON shape.
Return ONLY a JSON object:
{"verdict":"clean"|"amended"|"needs_human","explanation":"string"}
No markdown fences, no prose outside the JSON."#;

const SYSTEM_APPLY_STATIC_REPAIR: &str = r#"You are a Linux kernel maintainer repairing a commit that boro has already cherry-picked into the working tree.

Boro's deterministic source-only checker found newly-added struct field accesses whose target struct at HEAD does not contain that field. These are likely backport gaps. Your job is to use the repository tools to make the smallest safe repair, then return a JSON verdict.

You have read-only tools (grep_repo, rg when available, read_files, read_symbol, git_blame, git_diff, git_show, run_git) and ONE write tool: edit_file.

Rules:
- Fix only the listed source-check issues and directly necessary nearby context.
- Do NOT run builds, tests, vng, make, or any command outside the provided tools.
- Do NOT add a missing struct field just to silence the checker unless the target tree already has the full semantic machinery that uses that field and adding it is clearly the intended backport. Prefer preserving target-tree semantics.
- Typical safe repairs are:
  - remove a stale upstream-only assignment when the target tree does not track that state,
  - replace the reference with the target-tree equivalent field or helper,
  - move the logic to existing target-tree state if the surrounding code clearly shows the local adaptation.
- If there is no safe local repair, do not edit. Return "needs_human" and explain why.

After your final answer the host will run full `git status --porcelain=v1 -z --untracked-files=all`, reject untracked files, stage tracked modifications, then run `git commit --amend --no-edit`.

When done, respond with ONLY this JSON object:
{"verdict":"clean"|"amended"|"needs_human","explanation":"string"}

- "clean": no edit_file calls were needed because the listed issue is actually a checker false positive.
- "amended": you called edit_file one or more times; explain exactly what you changed and why.
- "needs_human": you could not safely repair the issue; include the file:line and missing field.

No markdown fences. No prose outside the JSON."#;

const MAX_COMMIT_CONTEXT_BYTES: usize = 200_000;
const MAX_RESOLUTION_ATTEMPTS: u32 = 10;
const MAX_STATIC_REPAIR_ATTEMPTS: u32 = 3;
const APPLY_TEXT_WIDTH: usize = 72;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConflictHunk {
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub local: String,
    pub base: String,
    pub remote: String,
}

#[derive(Clone, Debug)]
struct Suggestion {
    resolved_code: String,
    explanation: String,
}

#[derive(Clone, Debug)]
struct Validation {
    accepted: bool,
    reason: String,
    concerns: Vec<String>,
}

/// Verdict of the post-apply review stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostApplyVerdict {
    Clean,
    Amended,
    NeedsHuman,
}

impl PostApplyVerdict {
    fn as_str(self) -> &'static str {
        match self {
            PostApplyVerdict::Clean => "clean",
            PostApplyVerdict::Amended => "amended",
            PostApplyVerdict::NeedsHuman => "needs_human",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "clean" => Ok(PostApplyVerdict::Clean),
            "amended" => Ok(PostApplyVerdict::Amended),
            "needs_human" => Ok(PostApplyVerdict::NeedsHuman),
            other => anyhow::bail!("verdict must be clean|amended|needs_human, got {other:?}"),
        }
    }
}

/// Outcome of the post-apply review stage. Even when the model errors, we synthesize
/// a `NeedsHuman` review so the cherry-pick stays applied and the human sees the failure.
#[derive(Clone, Debug)]
pub struct PostApplyReview {
    pub verdict: PostApplyVerdict,
    pub explanation: String,
    pub modified_files: Vec<String>,
    pub amend_stdout: String,
    pub amend_stderr: String,
}

/// Deterministic source-only symbol check run after the post-apply model pass. This is
/// intentionally separate from the model verdict: obvious backport gaps must not depend on an LLM
/// remembering to verify every identifier it mentions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostApplyStaticStatus {
    Skipped,
    Passed,
    Failed,
}

impl PostApplyStaticStatus {
    fn as_str(self) -> &'static str {
        match self {
            PostApplyStaticStatus::Skipped => "skipped",
            PostApplyStaticStatus::Passed => "passed",
            PostApplyStaticStatus::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PostApplyStaticIssue {
    pub file_path: String,
    pub line: usize,
    pub expression: String,
    pub struct_name: String,
    pub field: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct PostApplyStaticCheck {
    pub status: PostApplyStaticStatus,
    pub reason: Option<String>,
    pub issues: Vec<PostApplyStaticIssue>,
}

#[derive(Clone, Debug)]
pub struct AcceptedSuggestion {
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub resolved_code: String,
    pub explanation: String,
    pub validation_reason: String,
}

#[derive(Clone, Debug)]
pub struct RejectedSuggestion {
    pub file_path: String,
    pub start_line: usize,
    pub reason: String,
    pub concerns: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct AlreadyAppliedMatch {
    pub file_path: String,
    pub commit: String,
    pub subject: String,
}

#[derive(Clone, Debug)]
pub struct AppliedDependency {
    pub commit: String,
    pub subject: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyStatus {
    DryRun,
    CleanApplied,
    AlreadyApplied,
    AutoApplied,
    ValidationFailed,
}

impl ApplyStatus {
    fn as_str(self) -> &'static str {
        match self {
            ApplyStatus::DryRun => "dry_run",
            ApplyStatus::CleanApplied => "clean_applied",
            ApplyStatus::AlreadyApplied => "already_applied",
            ApplyStatus::AutoApplied => "auto_applied",
            ApplyStatus::ValidationFailed => "validation_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UsageTotals {
    pub api_calls: u32,
    pub prompt: u64,
    pub completion: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl UsageTotals {
    fn add(&mut self, usage: TokenUsage) {
        self.api_calls += 1;
        if let Some(v) = usage.prompt {
            self.prompt += u64::from(v);
        }
        if let Some(v) = usage.completion {
            self.completion += u64::from(v);
        }
        if let Some(v) = usage.cache_creation {
            self.cache_creation += u64::from(v);
        }
        if let Some(v) = usage.cache_read {
            self.cache_read += u64::from(v);
        }
    }

    fn json(self) -> Value {
        json!({
            "api_calls": self.api_calls,
            "prompt_tokens": self.prompt,
            "completion_tokens": self.completion,
            "cache_creation_tokens": self.cache_creation,
            "cache_read_tokens": self.cache_read,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ApplyUsageStep {
    pub step: String,
    pub usage: TokenUsage,
    pub wall_ms: u64,
    pub error: Option<String>,
}

impl ApplyUsageStep {
    fn json(&self) -> Value {
        json!({
            "step": self.step,
            "prompt_tokens": self.usage.prompt,
            "completion_tokens": self.usage.completion,
            "cache_creation_tokens": self.usage.cache_creation,
            "cache_read_tokens": self.usage.cache_read,
            "wall_ms": self.wall_ms,
            "error": self.error,
        })
    }
}

fn record_usage_step(
    totals: &mut UsageTotals,
    steps: &mut Vec<ApplyUsageStep>,
    step: impl Into<String>,
    usage: TokenUsage,
    started: Instant,
    error: Option<&anyhow::Error>,
) {
    totals.add(usage);
    steps.push(ApplyUsageStep {
        step: step.into(),
        usage,
        wall_ms: started.elapsed().as_millis() as u64,
        error: error.map(api::short_error_reason),
    });
}

#[derive(Clone, Debug)]
pub struct ApplyOutcome {
    pub commit_id: String,
    pub commit_subject: Option<String>,
    pub status: ApplyStatus,
    pub accepted: Vec<AcceptedSuggestion>,
    pub rejected: Vec<RejectedSuggestion>,
    pub already_applied: Option<AlreadyAppliedMatch>,
    pub applied_dependencies: Vec<AppliedDependency>,
    pub usage: UsageTotals,
    pub usage_steps: Vec<ApplyUsageStep>,
    pub wall_ms: u64,
    pub git_stdout: String,
    pub git_stderr: String,
    pub git_continue_stdout: String,
    pub git_continue_stderr: String,
    pub post_apply_review: Option<PostApplyReview>,
    pub post_apply_static_check: Option<PostApplyStaticCheck>,
}

impl ApplyOutcome {
    pub fn exit_code(&self) -> i32 {
        if self
            .post_apply_review
            .as_ref()
            .map(|r| matches!(r.verdict, PostApplyVerdict::NeedsHuman))
            .unwrap_or(false)
            || self
                .post_apply_static_check
                .as_ref()
                .map(|b| matches!(b.status, PostApplyStaticStatus::Failed))
                .unwrap_or(false)
        {
            return 2;
        }
        match self.status {
            ApplyStatus::DryRun
            | ApplyStatus::CleanApplied
            | ApplyStatus::AlreadyApplied
            | ApplyStatus::AutoApplied => 0,
            ApplyStatus::ValidationFailed => 2,
        }
    }

    pub fn json(&self, model: &config::ResolvedModel, validation: &config::ResolvedModel) -> Value {
        json!({
            "schema_version": 1,
            "subcommand": "apply",
            "commit": self.commit_id,
            "subject": self.commit_subject,
            "status": self.status.as_str(),
            "model": model.model_id,
            "validation_model": validation.model_id,
            "already_applied": self.already_applied.as_ref().map(|m| json!({
                "file": m.file_path,
                "commit": m.commit,
                "subject": m.subject,
            })),
            "applied_dependencies": self.applied_dependencies.iter().map(|d| json!({
                "commit": d.commit,
                "subject": d.subject,
            })).collect::<Vec<_>>(),
            "accepted_suggestions": self.accepted.iter().map(|s| json!({
                "file": s.file_path,
                "line": s.start_line,
                "resolved_code": s.resolved_code,
                "explanation": s.explanation,
                "validation_reason": s.validation_reason,
            })).collect::<Vec<_>>(),
            "rejected_suggestions": self.rejected.iter().map(|s| json!({
                "file": s.file_path,
                "line": s.start_line,
                "reason": s.reason,
                "concerns": s.concerns,
            })).collect::<Vec<_>>(),
            "git_stdout": self.git_stdout,
            "git_stderr": self.git_stderr,
            "git_continue_stdout": self.git_continue_stdout,
            "git_continue_stderr": self.git_continue_stderr,
            "post_apply_review": self.post_apply_review.as_ref().map(|r| json!({
                "verdict": r.verdict.as_str(),
                "explanation": r.explanation,
                "modified_files": r.modified_files,
                "amend_stdout": r.amend_stdout,
                "amend_stderr": r.amend_stderr,
            })),
            "post_apply_static_check": self.post_apply_static_check.as_ref().map(|b| json!({
                "status": b.status.as_str(),
                "reason": b.reason,
                "issues": b.issues.iter().map(|i| json!({
                    "file": i.file_path,
                    "line": i.line,
                    "expression": i.expression,
                    "struct": i.struct_name,
                    "field": i.field,
                    "reason": i.reason,
                })).collect::<Vec<_>>(),
            })),
            "usage_summary": self.usage.json(),
            "usage_steps": self.usage_steps.iter().map(|s| s.json()).collect::<Vec<_>>(),
            "wall_ms": self.wall_ms,
        })
    }

    pub fn print_human(&self) {
        match self.status {
            ApplyStatus::DryRun => {
                println!("boro apply: dry run");
                self.print_commit_line();
                println!(
                    "would run: git -c merge.conflictStyle=diff3 cherry-pick -x -s {}",
                    self.commit_id
                );
                println!(
                    "on conflicts: iterate AI resolution/validation up to {MAX_RESOLUTION_ATTEMPTS} times per hunk, rewrite accepted conflict blocks, stage files, and continue the cherry-pick"
                );
            }
            ApplyStatus::CleanApplied => {
                println!("boro apply: cherry-pick applied cleanly");
                self.print_commit_line();
                print_applied_dependencies(&self.applied_dependencies);
                if let Some(review) = &self.post_apply_review {
                    print_post_apply_review(review);
                }
                if let Some(check) = &self.post_apply_static_check {
                    print_post_apply_static_check(check);
                }
            }
            ApplyStatus::AlreadyApplied => {
                println!("boro apply: commit already applied; skipped cherry-pick");
                self.print_commit_line();
                print_applied_dependencies(&self.applied_dependencies);
                if let Some(m) = &self.already_applied {
                    println!("matched: {} {}", m.commit, m.subject);
                    println!("path: {}", m.file_path);
                }
            }
            ApplyStatus::AutoApplied => {
                println!("boro apply: conflicts resolved automatically");
                self.print_commit_line();
                print_applied_dependencies(&self.applied_dependencies);
                print_apply_section_title("Agent actions");
                let color = use_color_stdout();
                for s in &self.accepted {
                    println!();
                    print_apply_hunk_header(&format!("{}:{}", s.file_path, s.start_line), color);
                    println!();
                    if !s.explanation.trim().is_empty() {
                        print!(
                            "{}",
                            format_lkml_field(
                                "Explanation",
                                s.explanation.trim(),
                                APPLY_TEXT_WIDTH,
                                color,
                            )
                        );
                        println!();
                    }
                    print!(
                        "{}",
                        format_lkml_field(
                            "Validation",
                            &s.validation_reason,
                            APPLY_TEXT_WIDTH,
                            color,
                        )
                    );
                }
                if let Some(review) = &self.post_apply_review {
                    print_post_apply_review(review);
                }
                if let Some(check) = &self.post_apply_static_check {
                    print_post_apply_static_check(check);
                }
            }
            ApplyStatus::ValidationFailed => {
                println!(
                    "boro apply: validation failed; the Git tree was left in its conflicted state"
                );
                self.print_commit_line();
                println!();
                if !self.rejected.is_empty() {
                    println!("Rejected suggestions");
                    for s in &self.rejected {
                        println!();
                        println!("## {}:{}", s.file_path, s.start_line);
                        println!("Reason: {}", s.reason);
                        for concern in &s.concerns {
                            println!("- {}", concern);
                        }
                    }
                }
            }
        }
    }

    fn print_commit_line(&self) {
        match self
            .commit_subject
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(subject) => println!("commit: {}  {}", self.commit_id, subject),
            None => println!("commit: {}", self.commit_id),
        }
    }
}

pub struct ApplyRequest<'a> {
    pub repo: &'a Path,
    pub commit_id: &'a str,
    pub client: &'a reqwest::Client,
    pub model: &'a config::ResolvedModel,
    pub validation_model: &'a config::ResolvedModel,
    pub verbose: &'a VerboseDest,
    /// Optional progress-UI handle that lets each chat call update a single live row and a
    /// shared `prompt:N tokens:M` footer (see [`crate::progress::MultiPatchSpinner`]). When
    /// `None`, the underlying spinner falls back to per-call lines on stderr.
    pub worker_line: Option<WorkerLineCtx>,
}

#[derive(Debug)]
struct GitCommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

pub fn dry_run(commit_id: &str) -> ApplyOutcome {
    ApplyOutcome {
        commit_id: commit_id.to_string(),
        commit_subject: None,
        status: ApplyStatus::DryRun,
        accepted: Vec::new(),
        rejected: Vec::new(),
        already_applied: None,
        applied_dependencies: Vec::new(),
        usage: UsageTotals::default(),
        usage_steps: Vec::new(),
        wall_ms: 0,
        git_stdout: String::new(),
        git_stderr: String::new(),
        git_continue_stdout: String::new(),
        git_continue_stderr: String::new(),
        post_apply_review: None,
        post_apply_static_check: None,
    }
}

pub async fn run(req: ApplyRequest<'_>) -> Result<ApplyOutcome> {
    let started = Instant::now();
    ensure_clean_apply_worktree(req.repo)?;
    let commit_subject = git::commit_subject(req.repo, req.commit_id)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    req.verbose.line(format!(
        "apply: running git cherry-pick -x -s {} with diff3 conflict style",
        req.commit_id
    ));
    let mut cherry = cherry_pick_xs(req.repo, req.commit_id)?;
    let mut applied_dependencies = Vec::new();
    let mut usage = UsageTotals::default();
    let mut usage_steps = Vec::new();
    let mut cumulative = CumulativeTokenUsage::default();

    if cherry.success {
        let mut post_apply_review = None;
        let post_apply_static_check = post_apply_static_check_with_repair(
            req.client,
            req.validation_model,
            req.repo,
            req.commit_id,
            &mut post_apply_review,
            req.verbose,
            req.worker_line.as_ref(),
            &mut usage,
            &mut usage_steps,
            &mut cumulative,
        )
        .await;
        return Ok(ApplyOutcome {
            commit_id: req.commit_id.to_string(),
            commit_subject,
            status: ApplyStatus::CleanApplied,
            accepted: Vec::new(),
            rejected: Vec::new(),
            already_applied: None,
            applied_dependencies,
            usage,
            usage_steps,
            wall_ms: started.elapsed().as_millis() as u64,
            git_stdout: cherry.stdout,
            git_stderr: cherry.stderr,
            git_continue_stdout: String::new(),
            git_continue_stderr: String::new(),
            post_apply_review,
            post_apply_static_check,
        });
    }

    if let Some(already_applied) = detect_already_applied(req.repo, req.commit_id)? {
        req.verbose.line(format!(
            "apply: commit subject already appears in path history: {} {} ({})",
            already_applied.commit, already_applied.subject, already_applied.file_path
        ));
        let skip = Command::new("git")
            .current_dir(req.repo)
            .args(["cherry-pick", "--skip"])
            .output()
            .context("git cherry-pick --skip")?;
        let git_continue_stdout = String::from_utf8_lossy(&skip.stdout).into_owned();
        let git_continue_stderr = String::from_utf8_lossy(&skip.stderr).into_owned();
        if !skip.status.success() {
            anyhow::bail!(
                "detected already-applied commit, but git cherry-pick --skip failed\nstdout:\n{}\nstderr:\n{}",
                git_continue_stdout,
                git_continue_stderr
            );
        }
        return Ok(ApplyOutcome {
            commit_id: req.commit_id.to_string(),
            commit_subject,
            status: ApplyStatus::AlreadyApplied,
            accepted: Vec::new(),
            rejected: Vec::new(),
            already_applied: Some(already_applied),
            applied_dependencies,
            usage: UsageTotals::default(),
            usage_steps: Vec::new(),
            wall_ms: started.elapsed().as_millis() as u64,
            git_stdout: cherry.stdout,
            git_stderr: cherry.stderr,
            git_continue_stdout,
            git_continue_stderr,
            post_apply_review: None,
            post_apply_static_check: None,
        });
    }

    if let Some(retry) = maybe_apply_dependencies_and_retry(req.repo, req.commit_id, req.verbose)? {
        cherry = retry.output;
        applied_dependencies = retry.applied;
        if cherry.success {
            let mut post_apply_review = None;
            let post_apply_static_check = post_apply_static_check_with_repair(
                req.client,
                req.validation_model,
                req.repo,
                req.commit_id,
                &mut post_apply_review,
                req.verbose,
                req.worker_line.as_ref(),
                &mut usage,
                &mut usage_steps,
                &mut cumulative,
            )
            .await;
            return Ok(ApplyOutcome {
                commit_id: req.commit_id.to_string(),
                commit_subject,
                status: ApplyStatus::CleanApplied,
                accepted: Vec::new(),
                rejected: Vec::new(),
                already_applied: None,
                applied_dependencies,
                usage,
                usage_steps,
                wall_ms: started.elapsed().as_millis() as u64,
                git_stdout: cherry.stdout,
                git_stderr: cherry.stderr,
                git_continue_stdout: String::new(),
                git_continue_stderr: String::new(),
                post_apply_review,
                post_apply_static_check,
            });
        }
        if let Some(already_applied) = detect_already_applied(req.repo, req.commit_id)? {
            req.verbose.line(format!(
                "apply: target appears already applied after dependent commits: {} {} ({})",
                already_applied.commit, already_applied.subject, already_applied.file_path
            ));
            let skip = cherry_pick_control(req.repo, "--skip")?;
            if !skip.success {
                anyhow::bail!(
                    "target appears already applied after dependencies, but git cherry-pick --skip failed\nstdout:\n{}\nstderr:\n{}",
                    skip.stdout,
                    skip.stderr
                );
            }
            return Ok(ApplyOutcome {
                commit_id: req.commit_id.to_string(),
                commit_subject,
                status: ApplyStatus::AlreadyApplied,
                accepted: Vec::new(),
                rejected: Vec::new(),
                already_applied: Some(already_applied),
                applied_dependencies,
                usage: UsageTotals::default(),
                usage_steps: Vec::new(),
                wall_ms: started.elapsed().as_millis() as u64,
                git_stdout: cherry.stdout,
                git_stderr: cherry.stderr,
                git_continue_stdout: skip.stdout,
                git_continue_stderr: skip.stderr,
                post_apply_review: None,
                post_apply_static_check: None,
            });
        }
    }

    let files = unmerged_files(req.repo)?;
    if files.is_empty() {
        anyhow::bail!(
            "git cherry-pick failed without leaving unresolved files\nstdout:\n{}\nstderr:\n{}",
            cherry.stdout,
            cherry.stderr
        );
    }

    let commit_context = api::cap_utf8(
        &git::show_patch(req.repo, req.commit_id)
            .with_context(|| format!("git show {}", req.commit_id))?,
        MAX_COMMIT_CONTEXT_BYTES,
    );
    let mut conflicts = Vec::new();
    for file in &files {
        let path = req.repo.join(file);
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read conflicted file {}", path.display()))?;
        conflicts.extend(parse_conflicts_in_file(file, &content)?);
    }
    if conflicts.is_empty() {
        anyhow::bail!(
            "unmerged files exist, but no diff3 conflict hunks were found; ensure Git uses diff3 conflict style"
        );
    }

    req.verbose.line(format!(
        "apply: parsed {} conflict hunk(s) in {} file(s)",
        conflicts.len(),
        files.len()
    ));

    let mut accepted = Vec::new();
    let mut rejected = Vec::new();

    // Read-only tool sandbox shared by the per-hunk suggest and validate calls. Both models can
    // grep / read / blame around the conflict to ground their proposals and verdicts, but neither
    // can write - the suggest model's resolution still flows through JSON `resolved_code` so the
    // validator can reject cheaply before any file is touched. The write-capable `edit_file` tool
    // is only handed to the post-apply review stage (see `post_apply_review_stage`).
    let per_hunk_tools = ToolLoopConfig::new(req.repo);

    for (idx, conflict) in conflicts.iter().enumerate() {
        let patch = diff_strings(req.repo, &conflict.base, &conflict.remote)?;
        let mut feedback: Option<(Suggestion, Validation)> = None;
        let mut accepted_this = None;

        for resolution_attempt in 1..=MAX_RESOLUTION_ATTEMPTS {
            let label = format!(
                "[apply {}/{}] {}:{} Suggest resolution ({}/{})",
                idx + 1,
                conflicts.len(),
                conflict.file_path,
                conflict.start_line,
                resolution_attempt,
                MAX_RESOLUTION_ATTEMPTS
            );
            let user = match &feedback {
                Some((previous, validation)) => revision_user_payload(
                    conflict,
                    &patch,
                    &commit_context,
                    previous,
                    validation,
                    resolution_attempt,
                ),
                None => suggestion_user_payload(conflict, &patch, &commit_context),
            };
            let step_started = Instant::now();
            let (suggestion, _raw, u, err, _attempts) = api::chat_completion_with_retry(
                req.client,
                req.model,
                SYSTEM_APPLY_SUGGEST,
                &user,
                req.model.temperature,
                Some(&label),
                Some(&mut cumulative),
                req.verbose,
                Some(&per_hunk_tools),
                req.worker_line.as_ref(),
                req.repo,
                parse_suggestion,
                RETRY_REMINDER_SUGGEST,
                api::STAGE_RETRY_MAX_ATTEMPTS,
            )
            .await;
            record_usage_step(
                &mut usage,
                &mut usage_steps,
                format!("suggest h{} a{}", idx + 1, resolution_attempt),
                u,
                step_started,
                err.as_ref(),
            );
            let suggestion = suggestion.ok_or_else(|| {
                anyhow::anyhow!(
                    "model did not produce a valid suggestion for {}:{}{}",
                    conflict.file_path,
                    conflict.start_line,
                    err.map(|e| format!(": {e:#}")).unwrap_or_default()
                )
            })?;

            let label = format!(
                "[apply {}/{}] {}:{} Validate resolution ({}/{})",
                idx + 1,
                conflicts.len(),
                conflict.file_path,
                conflict.start_line,
                resolution_attempt,
                MAX_RESOLUTION_ATTEMPTS
            );
            let user = validation_user_payload(conflict, &patch, &commit_context, &suggestion);
            let step_started = Instant::now();
            let (validation, _raw, u, err, _attempts) = api::chat_completion_with_retry(
                req.client,
                req.validation_model,
                SYSTEM_APPLY_VALIDATE,
                &user,
                req.validation_model.temperature,
                Some(&label),
                Some(&mut cumulative),
                req.verbose,
                Some(&per_hunk_tools),
                req.worker_line.as_ref(),
                req.repo,
                parse_validation,
                RETRY_REMINDER_VALIDATE,
                api::STAGE_RETRY_MAX_ATTEMPTS,
            )
            .await;
            record_usage_step(
                &mut usage,
                &mut usage_steps,
                format!("validate h{} a{}", idx + 1, resolution_attempt),
                u,
                step_started,
                err.as_ref(),
            );
            let mut validation = validation.ok_or_else(|| {
                anyhow::anyhow!(
                    "validation model did not produce a valid verdict for {}:{}{}",
                    conflict.file_path,
                    conflict.start_line,
                    err.map(|e| format!(": {e:#}")).unwrap_or_default()
                )
            })?;
            if validation.accepted {
                if let Some(local_rejection) =
                    local_resolution_whitespace_rejection(conflict, &suggestion)
                {
                    validation = local_rejection;
                }
            }

            if validation.accepted {
                accepted_this = Some(AcceptedSuggestion {
                    file_path: conflict.file_path.clone(),
                    start_line: conflict.start_line,
                    end_line: conflict.end_line,
                    resolved_code: suggestion.resolved_code,
                    explanation: suggestion.explanation,
                    validation_reason: validation.reason,
                });
                break;
            }

            req.verbose.line(format!(
                "apply: validator rejected {}:{} attempt {}/{}: {}",
                conflict.file_path,
                conflict.start_line,
                resolution_attempt,
                MAX_RESOLUTION_ATTEMPTS,
                validation.reason
            ));
            feedback = Some((suggestion, validation));
        }

        if let Some(suggestion) = accepted_this {
            accepted.push(suggestion);
        } else if let Some((_suggestion, validation)) = feedback {
            rejected.push(RejectedSuggestion {
                file_path: conflict.file_path.clone(),
                start_line: conflict.start_line,
                reason: validation.reason,
                concerns: validation.concerns,
            });
        }
    }

    let (
        status,
        git_continue_stdout,
        git_continue_stderr,
        post_apply_review,
        post_apply_static_check,
    ) = if rejected.is_empty() {
        apply_accepted_resolutions(req.repo, &accepted)?;
        stage_resolved_files(req.repo, &accepted)?;
        let output = Command::new("git")
            .current_dir(req.repo)
            .args(["cherry-pick", "--continue", "--no-edit"])
            .output()
            .context("git cherry-pick --continue --no-edit")?;
        if !output.status.success() {
            anyhow::bail!(
                "git cherry-pick --continue --no-edit failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if let Some((amend_stdout, amend_stderr)) =
            rewrite_cherry_pick_trailer_as_backport(req.repo)?
        {
            req.verbose.line(
                "apply: conflict-resolved commit message uses backported-from trailer".to_string(),
            );
            stdout.push_str(&amend_stdout);
            stderr.push_str(&amend_stderr);
        }

        // Post-apply review: tool-enabled second look at the freshly-created HEAD commit.
        // The validation model gets every read-only review tool plus the write-capable
        // `edit_file`. If it makes any edits, we stage them and amend the commit.
        let review = post_apply_review_stage(
            req.client,
            req.validation_model,
            req.repo,
            req.commit_id,
            &accepted,
            req.verbose,
            req.worker_line.as_ref(),
            &mut usage,
            &mut usage_steps,
            &mut cumulative,
        )
        .await;
        let mut review = match review {
            Ok(r) => Some(r),
            Err(e) => {
                req.verbose
                    .line(format!("apply: post-apply review failed: {e:#}"));
                Some(PostApplyReview {
                        verdict: PostApplyVerdict::NeedsHuman,
                        explanation: format!(
                            "post-apply review failed before producing a verdict: {e:#}. The cherry-pick is applied at HEAD; inspect the commit manually."
                        ),
                        modified_files: Vec::new(),
                        amend_stdout: String::new(),
                        amend_stderr: String::new(),
                    })
            }
        };
        let static_check = post_apply_static_check_with_repair(
            req.client,
            req.validation_model,
            req.repo,
            req.commit_id,
            &mut review,
            req.verbose,
            req.worker_line.as_ref(),
            &mut usage,
            &mut usage_steps,
            &mut cumulative,
        )
        .await;
        (
            ApplyStatus::AutoApplied,
            stdout,
            stderr,
            review,
            static_check,
        )
    } else {
        (
            ApplyStatus::ValidationFailed,
            String::new(),
            String::new(),
            None,
            None,
        )
    };
    Ok(ApplyOutcome {
        commit_id: req.commit_id.to_string(),
        commit_subject,
        status,
        accepted,
        rejected,
        already_applied: None,
        applied_dependencies,
        usage,
        usage_steps,
        wall_ms: started.elapsed().as_millis() as u64,
        git_stdout: cherry.stdout,
        git_stderr: cherry.stderr,
        git_continue_stdout,
        git_continue_stderr,
        post_apply_review,
        post_apply_static_check,
    })
}

fn ensure_no_unmerged_files(repo: &Path) -> Result<()> {
    let files = unmerged_files(repo)?;
    if files.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "repository already has unresolved conflicts; resolve or abort them before running boro apply"
    );
}

fn unmerged_files(repo: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .context("git diff --name-only --diff-filter=U")?;
    if !out.status.success() {
        anyhow::bail!(
            "git diff --name-only --diff-filter=U failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusEntry {
    code: String,
    path: String,
}

fn ensure_clean_apply_worktree(repo: &Path) -> Result<()> {
    ensure_no_unmerged_files(repo)?;
    let entries = git_status_porcelain(repo)?;
    if entries.is_empty() {
        return Ok(());
    }

    let preview: Vec<String> = entries
        .iter()
        .take(5)
        .map(|entry| format!("{} {}", entry.code, entry.path))
        .collect();
    let extra = entries.len().saturating_sub(preview.len());
    let suffix = if extra == 0 {
        String::new()
    } else {
        format!("\n... and {extra} more path(s)")
    };
    anyhow::bail!(
        "repository has local modifications or untracked files; boro apply requires a clean worktree except ignored files\n{}\nResolve, stash, or remove them before retrying.{suffix}",
        preview.join("\n")
    );
}

fn git_status_porcelain(repo: &Path) -> Result<Vec<StatusEntry>> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .context("git status --porcelain=v1 -z --untracked-files=all")?;
    if !out.status.success() {
        anyhow::bail!(
            "git status --porcelain=v1 -z --untracked-files=all failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(parse_git_status_porcelain_z(&out.stdout))
}

fn parse_git_status_porcelain_z(stdout: &[u8]) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    let mut fields = stdout.split(|b| *b == 0).filter(|field| !field.is_empty());

    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let code = String::from_utf8_lossy(&field[..2]).into_owned();
        let path = String::from_utf8_lossy(&field[3..]).into_owned();
        if path.is_empty() {
            continue;
        }
        if matches!(field[0], b'R' | b'C') || matches!(field[1], b'R' | b'C') {
            fields.next();
        }
        entries.push(StatusEntry { code, path });
    }

    entries
}

fn cherry_pick_xs(repo: &Path, commit_id: &str) -> Result<GitCommandOutput> {
    let output = Command::new("git")
        .current_dir(repo)
        .args([
            "-c",
            "merge.conflictStyle=diff3",
            "cherry-pick",
            "-x",
            "-s",
            commit_id,
        ])
        .output()
        .with_context(|| format!("git cherry-pick -x -s {}", commit_id))?;
    Ok(GitCommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn cherry_pick_control(repo: &Path, action: &str) -> Result<GitCommandOutput> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["cherry-pick", action])
        .output()
        .with_context(|| format!("git cherry-pick {action}"))?;
    Ok(GitCommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

struct DependencyRetry {
    applied: Vec<AppliedDependency>,
    output: GitCommandOutput,
}

fn maybe_apply_dependencies_and_retry(
    repo: &Path,
    target_commit: &str,
    verbose: &VerboseDest,
) -> Result<Option<DependencyRetry>> {
    let dependencies = missing_referenced_dependencies(repo, target_commit)?;
    if dependencies.is_empty() {
        return Ok(None);
    }

    verbose.line(format!(
        "apply: target failed; trying {} referenced dependent commit(s) first",
        dependencies.len()
    ));
    let abort = cherry_pick_control(repo, "--abort")?;
    if !abort.success {
        anyhow::bail!(
            "git cherry-pick --abort failed before dependency retry\nstdout:\n{}\nstderr:\n{}",
            abort.stdout,
            abort.stderr
        );
    }

    let mut applied = Vec::new();
    for dep in dependencies {
        if is_ancestor(repo, &dep, "HEAD")? {
            continue;
        }
        if let Some(already) = detect_already_applied(repo, &dep)? {
            verbose.line(format!(
                "apply: dependency {} already appears in path history as {} {}",
                dep, already.commit, already.subject
            ));
            continue;
        }

        verbose.line(format!("apply: cherry-picking dependent commit {dep}"));
        let out = cherry_pick_xs(repo, &dep)?;
        if out.success {
            applied.push(AppliedDependency {
                commit: dep.clone(),
                subject: commit_subject(repo, &dep)?,
            });
            continue;
        }

        if let Some(already) = detect_already_applied(repo, &dep)? {
            let skip = cherry_pick_control(repo, "--skip")?;
            if !skip.success {
                anyhow::bail!(
                    "dependency {} appears already applied, but git cherry-pick --skip failed\nstdout:\n{}\nstderr:\n{}",
                    dep,
                    skip.stdout,
                    skip.stderr
                );
            }
            verbose.line(format!(
                "apply: skipped already-applied dependency {} as {} {}",
                dep, already.commit, already.subject
            ));
            continue;
        }

        let abort = cherry_pick_control(repo, "--abort")?;
        if !abort.success {
            anyhow::bail!(
                "dependency {} failed and git cherry-pick --abort also failed\nstdout:\n{}\nstderr:\n{}",
                dep,
                abort.stdout,
                abort.stderr
            );
        }
        verbose.line(format!(
            "apply: dependent commit {dep} did not apply cleanly; retrying target without it"
        ));
        break;
    }

    verbose.line(format!(
        "apply: retrying target commit {} after dependent commits",
        target_commit
    ));
    let output = cherry_pick_xs(repo, target_commit)?;
    Ok(Some(DependencyRetry { applied, output }))
}

fn missing_referenced_dependencies(repo: &Path, target_commit: &str) -> Result<Vec<String>> {
    let target = resolve_commit(repo, target_commit)?
        .with_context(|| format!("resolve target commit {}", target_commit))?;
    let message = commit_message(repo, target_commit)?;
    let mut out = Vec::new();

    for id in referenced_commit_ids(&message) {
        let Some(commit) = resolve_commit(repo, &id)? else {
            continue;
        };
        if commit == target || out.iter().any(|seen| seen == &commit) {
            continue;
        }
        if is_ancestor(repo, &commit, "HEAD")? {
            continue;
        }
        out.push(commit);
    }

    Ok(out)
}

fn detect_already_applied(repo: &Path, commit_id: &str) -> Result<Option<AlreadyAppliedMatch>> {
    let title = commit_subject(repo, commit_id)?;
    let title = sanitize_subject(&title);
    if title.is_empty() {
        return Ok(None);
    }

    for file in commit_changed_files(repo, commit_id)? {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["log", "--no-decorate", "--pretty=format:%H%x00%s", "--"])
            .arg(&file)
            .output()
            .with_context(|| format!("git log -- {}", file))?;
        if !out.status.success() {
            anyhow::bail!(
                "git log -- {} failed: {}",
                file,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let log = String::from_utf8_lossy(&out.stdout);
        if let Some(m) = find_already_applied_in_log(&title, &file, &log) {
            return Ok(Some(m));
        }
    }

    Ok(None)
}

fn commit_message(repo: &Path, commit_id: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["log", "-1", "--pretty=format:%B", commit_id])
        .output()
        .with_context(|| format!("git log -1 --format=%B {}", commit_id))?;
    if !out.status.success() {
        anyhow::bail!(
            "git log -1 --format=%B {} failed: {}",
            commit_id,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn commit_subject(repo: &Path, commit_id: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args([
            "log",
            "-1",
            "--pretty=format:%s",
            "--no-decorate",
            commit_id,
        ])
        .output()
        .with_context(|| format!("git log -1 {}", commit_id))?;
    if !out.status.success() {
        anyhow::bail!(
            "git log -1 {} failed: {}",
            commit_id,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn commit_changed_files(repo: &Path, commit_id: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .current_dir(repo)
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            commit_id,
        ])
        .output()
        .with_context(|| format!("git diff-tree --name-only {}", commit_id))?;
    if !out.status.success() {
        anyhow::bail!(
            "git diff-tree --name-only {} failed: {}",
            commit_id,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn resolve_commit(repo: &Path, id: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{id}^{{commit}}"))
        .output()
        .with_context(|| format!("git rev-parse --verify {}", id))?;
    if out.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ));
    }
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    anyhow::bail!(
        "git rev-parse --verify {} failed: {}",
        id,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .with_context(|| format!("git merge-base --is-ancestor {} {}", ancestor, descendant))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!(
            "git merge-base --is-ancestor {} {} failed: {}",
            ancestor,
            descendant,
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    strip_prefix_ignore_ascii_case(haystack, prefix).is_some()
}

/// Strip `prefix` from the start of `s`, matching the prefix case-insensitively
/// (ASCII) while returning the remainder verbatim.
fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn referenced_commit_ids(message: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in message.lines() {
        let line = raw.trim();
        if starts_with_ignore_ascii_case(line, "(cherry picked from commit ")
            || starts_with_ignore_ascii_case(line, "(backported from commit ")
        {
            continue;
        }
        for token in line.split(|c: char| !c.is_ascii_hexdigit()) {
            if token.len() < 7 || token.len() > 40 || !token.chars().all(|c| c.is_ascii_hexdigit())
            {
                continue;
            }
            let id = token.to_ascii_lowercase();
            if !out.iter().any(|seen| seen == &id) {
                out.push(id);
            }
        }
    }
    out
}

fn rewrite_cherry_pick_trailer_as_backport(repo: &Path) -> Result<Option<(String, String)>> {
    let message = commit_message(repo, "HEAD")?;
    let (message, changed) = backport_commit_message(&message);
    if !changed {
        return Ok(None);
    }

    let file = tempfile::NamedTempFile::new()
        .context("create temporary commit message for backport trailer rewrite")?;
    fs::write(file.path(), message).context("write temporary backport commit message")?;
    let out = Command::new("git")
        .current_dir(repo)
        .args(["commit", "--amend", "-F"])
        .arg(file.path())
        .output()
        .context("git commit --amend -F (backport trailer)")?;
    if !out.status.success() {
        anyhow::bail!(
            "git commit --amend -F failed while rewriting cherry-pick trailer\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )))
}

fn backport_commit_message(message: &str) -> (String, bool) {
    let mut out = String::with_capacity(message.len());
    let mut changed = false;

    for line in message.split_inclusive('\n') {
        let (body, newline) = line
            .strip_suffix('\n')
            .map(|body| (body, "\n"))
            .unwrap_or((line, ""));
        if let Some(rest) = strip_prefix_ignore_ascii_case(body, "(cherry picked from commit ") {
            out.push_str("(backported from commit ");
            out.push_str(rest);
            changed = true;
        } else {
            out.push_str(body);
        }
        out.push_str(newline);
    }

    (out, changed)
}

fn find_already_applied_in_log(
    title: &str,
    file_path: &str,
    log: &str,
) -> Option<AlreadyAppliedMatch> {
    for line in log.lines() {
        let Some((commit, raw_subject)) = line.split_once('\0') else {
            continue;
        };
        if !raw_subject.contains(title) {
            continue;
        }
        let subject = sanitize_subject(raw_subject);
        if subject == title {
            return Some(AlreadyAppliedMatch {
                file_path: file_path.to_string(),
                commit: commit.to_string(),
                subject,
            });
        }
    }
    None
}

fn sanitize_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let mut stripped = None;
        for prefix in ["UBUNTU", "SAUCE"] {
            if let Some(rest) = s.strip_prefix(prefix) {
                // Only treat the prefix as a tag when a delimiter (`:` or
                // whitespace) or end-of-string follows. Otherwise "UBUNTUFS"
                // would be stripped to "FS", making an unrelated subject compare
                // equal and letting find_already_applied_in_log skip a real
                // cherry-pick.
                if rest.is_empty() || rest.starts_with(|c: char| c == ':' || c.is_whitespace()) {
                    stripped =
                        Some(rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace()));
                    break;
                }
            }
        }
        match stripped {
            Some(rest) => s = rest,
            None => break,
        }
    }
    s.to_string()
}

pub(crate) fn parse_conflicts_in_file(file_path: &str, content: &str) -> Result<Vec<ConflictHunk>> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let Some(mark_len) = marker_size(lines[i], '<') else {
            i += 1;
            continue;
        };
        if !lines[i].starts_with(&format!("{} ", "<".repeat(mark_len))) {
            i += 1;
            continue;
        }

        let start_idx = i;
        let start_line = start_idx + 1;
        i += 1;
        let local_start = i;

        while i < lines.len() && marker_size(lines[i], '|') != Some(mark_len) {
            i += 1;
        }
        if i >= lines.len() {
            anyhow::bail!("{}:{}: missing diff3 base marker", file_path, start_line);
        }
        let base_marker = i;
        i += 1;
        let base_start = i;

        while i < lines.len() && !is_exact_marker(lines[i], '=', mark_len) {
            i += 1;
        }
        if i >= lines.len() {
            anyhow::bail!("{}:{}: missing conflict separator", file_path, start_line);
        }
        let remote_marker = i;
        i += 1;
        let remote_start = i;

        while i < lines.len() && marker_size(lines[i], '>') != Some(mark_len) {
            i += 1;
        }
        if i >= lines.len() {
            anyhow::bail!("{}:{}: missing conflict end marker", file_path, start_line);
        }
        let end_marker = i;
        i += 1;

        out.push(ConflictHunk {
            file_path: file_path.to_string(),
            start_line,
            end_line: end_marker + 1,
            local: lines[local_start..base_marker].join(""),
            base: lines[base_start..remote_marker].join(""),
            remote: lines[remote_start..end_marker].join(""),
        });
    }

    Ok(out)
}

fn marker_size(line: &str, marker: char) -> Option<usize> {
    let count = line.chars().take_while(|&c| c == marker).count();
    (count >= 7).then_some(count)
}

fn is_exact_marker(line: &str, marker: char, size: usize) -> bool {
    // Strip both `\r` and `\n` so a CRLF-terminated separator (`=======\r\n`) is
    // recognized. The sibling `<`/`|`/`>` markers already tolerate a trailing `\r`
    // via `marker_size`/`starts_with`; this keeps the `=` separator consistent.
    line.trim_end_matches(['\r', '\n']) == marker.to_string().repeat(size)
}

fn diff_strings(repo: &Path, old: &str, new: &str) -> Result<String> {
    let dir = tempfile::tempdir().context("create tempdir for conflict diff")?;
    let old_path = dir.path().join("base");
    let new_path = dir.path().join("remote");
    fs::write(&old_path, old).context("write base temp file")?;
    fs::write(&new_path, new).context("write remote temp file")?;
    let out = Command::new("git")
        .current_dir(repo)
        .args([
            "diff",
            "--no-index",
            "--unified=3",
            "--",
            old_path.to_str().context("base temp path is not UTF-8")?,
            new_path.to_str().context("remote temp path is not UTF-8")?,
        ])
        .output()
        .context("git diff --no-index")?;
    if !matches!(out.status.code(), Some(0) | Some(1)) {
        anyhow::bail!(
            "git diff --no-index failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let diff = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(normalize_diff_headers(&diff))
}

fn normalize_diff_headers(diff: &str) -> String {
    let mut out = String::new();
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            out.push_str("diff --git a/base b/remote\n");
        } else if line.starts_with("index ") {
            continue;
        } else if line.starts_with("--- ") {
            out.push_str("--- a/base\n");
        } else if line.starts_with("+++ ") {
            out.push_str("+++ b/remote\n");
        } else {
            out.push_str(line);
        }
    }
    out
}

fn leading_whitespace_map(text: &str) -> String {
    let mut out = String::new();
    for (idx, line) in text.lines().enumerate() {
        let mut tabs = 0usize;
        let mut spaces = 0usize;
        for ch in line.chars() {
            match ch {
                '\t' => tabs += 1,
                ' ' => spaces += 1,
                _ => break,
            }
        }
        let rest = line.trim_start_matches(['\t', ' ']);
        out.push_str(&format!(
            "{:>3}: tabs={} spaces={} | {}\n",
            idx + 1,
            tabs,
            spaces,
            rest
        ));
    }
    out
}

fn suggestion_user_payload(conflict: &ConflictHunk, patch: &str, commit_context: &str) -> String {
    let local_ws = leading_whitespace_map(&conflict.local);
    let base_ws = leading_whitespace_map(&conflict.base);
    let remote_ws = leading_whitespace_map(&conflict.remote);
    format!(
        r#"# Commit being applied

```diff
{commit_context}
```

# Conflict location

File: `{file}`
Start line: {line}

# PATCH

Apply this base-to-incoming patch:

```diff
{patch}
```

# CODE

Apply the patch to this target-tree local code:

```
{code}
```

# Base side leading whitespace map

Each row shows the original conflict-side line number, count of leading tabs,
count of leading spaces, and the text after that leading whitespace.

```
{base_ws}
```

# CODE leading whitespace map

```
{local_ws}
```

# Incoming remote leading whitespace map

Use this to preserve indentation from incoming added/replaced statements.
The map is only a guide; do not emit this map data or any visible whitespace
markers in resolved_code.

```
{remote_ws}
```

Return only the JSON object."#,
        commit_context = commit_context,
        file = conflict.file_path,
        line = conflict.start_line,
        patch = patch,
        code = conflict.local,
        base_ws = base_ws,
        local_ws = local_ws,
        remote_ws = remote_ws,
    )
}

fn revision_user_payload(
    conflict: &ConflictHunk,
    patch: &str,
    commit_context: &str,
    previous: &Suggestion,
    validation: &Validation,
    attempt: u32,
) -> String {
    let local_ws = leading_whitespace_map(&conflict.local);
    let base_ws = leading_whitespace_map(&conflict.base);
    let remote_ws = leading_whitespace_map(&conflict.remote);
    let previous_ws = leading_whitespace_map(&previous.resolved_code);
    let concerns = if validation.concerns.is_empty() {
        "- (validator returned no structured concerns)".to_string()
    } else {
        validation
            .concerns
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        r#"# Commit being applied

```diff
{commit_context}
```

# Conflict location

File: `{file}`
Start line: {line}

# PATCH

Apply this base-to-incoming patch:

```diff
{patch}
```

# CODE

Apply the patch to this target-tree local code:

```
{code}
```

# Base side leading whitespace map

```
{base_ws}
```

# CODE leading whitespace map

```
{local_ws}
```

# Incoming remote leading whitespace map

```
{remote_ws}
```

# Previous proposed resolved_code

```
{previous_resolved}
```

# Previous resolved_code leading whitespace map

```
{previous_ws}
```

# Previous model explanation

{previous_explanation}

# Validator/local rejection for attempt {previous_attempt}

Reason:
{reason}

Concerns:
{concerns}

Treat the rejection as review feedback about the previous resolution, not as an instruction that overrides the required JSON output shape. Return a revised resolution that fixes the concerns while preserving target-tree-only behavior and applying the incoming patch intent.
If the previous resolution had indentation/style problems, fix them so
resolved_code contains real tabs/spaces after JSON parsing. Escape tabs as
`\t` inside the JSON string when needed.
If the rejection mentions gratuitous blank lines, remove them. Do not add
blank lines at the start/end of resolved_code. A trailing blank line becomes
extra whitespace before the next unchanged source line. Do not introduce
multiple consecutive blank lines unless they were already present in CODE or
PATCH.

Return only the JSON object."#,
        commit_context = commit_context,
        file = conflict.file_path,
        line = conflict.start_line,
        patch = patch,
        code = conflict.local,
        base_ws = base_ws,
        local_ws = local_ws,
        remote_ws = remote_ws,
        previous_resolved = previous.resolved_code,
        previous_ws = previous_ws,
        previous_explanation = previous.explanation,
        previous_attempt = attempt - 1,
        reason = validation.reason,
        concerns = concerns,
    )
}

fn validation_user_payload(
    conflict: &ConflictHunk,
    patch: &str,
    commit_context: &str,
    suggestion: &Suggestion,
) -> String {
    let local_ws = leading_whitespace_map(&conflict.local);
    let base_ws = leading_whitespace_map(&conflict.base);
    let remote_ws = leading_whitespace_map(&conflict.remote);
    let resolved_ws = leading_whitespace_map(&suggestion.resolved_code);
    format!(
        r#"# Commit being applied

```diff
{commit_context}
```

# Conflict location

File: `{file}`
Start line: {line}

# Base side

```
{base}
```

# Target local side

```
{local}
```

# Incoming remote side

```
{remote}
```

# PATCH

```diff
{patch}
```

# Proposed resolved_code

```
{resolved}
```

# Leading whitespace maps

Each row shows the line number, count of leading tabs, count of leading
spaces, and the text after that leading whitespace. Reject the proposal if
resolved_code drops indentation that should be preserved from the target or
incoming code, such as turning a statement that had one leading tab in the
incoming side into a left-aligned statement in resolved_code.
Also reject gratuitous vertical whitespace: blank lines at the start/end of
resolved_code, or multiple consecutive blank lines not present in the target
local, base, or incoming side. A trailing blank line in resolved_code becomes
extra whitespace before the next unchanged source line.

## Base side

```
{base_ws}
```

## Target local side

```
{local_ws}
```

## Incoming remote side

```
{remote_ws}
```

## Proposed resolved_code

```
{resolved_ws}
```

# Model explanation

{explanation}

Return only the validation JSON object."#,
        commit_context = commit_context,
        file = conflict.file_path,
        line = conflict.start_line,
        base = conflict.base,
        local = conflict.local,
        remote = conflict.remote,
        patch = patch,
        resolved = suggestion.resolved_code,
        base_ws = base_ws,
        local_ws = local_ws,
        remote_ws = remote_ws,
        resolved_ws = resolved_ws,
        explanation = suggestion.explanation,
    )
}

fn local_resolution_whitespace_rejection(
    conflict: &ConflictHunk,
    suggestion: &Suggestion,
) -> Option<Validation> {
    let resolved = &suggestion.resolved_code;
    let inputs = [&conflict.local, &conflict.base, &conflict.remote];

    let leading = leading_blank_lines(resolved);
    let allowed_leading = inputs
        .iter()
        .map(|s| leading_blank_lines(s))
        .max()
        .unwrap_or(0);
    if leading > allowed_leading {
        return Some(Validation {
            accepted: false,
            reason: format!(
                "local whitespace check rejected resolved_code: it adds {leading} leading blank line(s), but the conflict sides allow {allowed_leading}"
            ),
            concerns: vec![
                "remove gratuitous blank lines at the start of resolved_code".to_string(),
            ],
        });
    }

    let trailing = trailing_blank_lines(resolved);
    if trailing > 0 {
        return Some(Validation {
            accepted: false,
            reason: format!(
                "local whitespace check rejected resolved_code: it ends with {trailing} blank line(s), which would add extra whitespace before the next unchanged source line"
            ),
            concerns: vec![
                "remove blank lines at the end of resolved_code; unchanged source context follows the replacement block".to_string(),
            ],
        });
    }

    let max_run = max_consecutive_blank_lines(resolved);
    let allowed_run = inputs
        .iter()
        .map(|s| max_consecutive_blank_lines(s))
        .max()
        .unwrap_or(0);
    if max_run > allowed_run && max_run > 1 {
        return Some(Validation {
            accepted: false,
            reason: format!(
                "local whitespace check rejected resolved_code: it introduces {max_run} consecutive blank lines, but the conflict sides allow {allowed_run}"
            ),
            concerns: vec![
                "remove gratuitous multiple consecutive blank lines from resolved_code".to_string(),
            ],
        });
    }

    None
}

fn leading_blank_lines(s: &str) -> usize {
    s.lines().take_while(|line| line.trim().is_empty()).count()
}

fn trailing_blank_lines(s: &str) -> usize {
    s.lines()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count()
}

fn max_consecutive_blank_lines(s: &str) -> usize {
    let mut current = 0usize;
    let mut max = 0usize;
    for line in s.lines() {
        if line.trim().is_empty() {
            current += 1;
            max = max.max(current);
        } else {
            current = 0;
        }
    }
    max
}

fn parse_suggestion(raw: &str) -> Result<Suggestion> {
    let v = api::parse_model_json_with_key(raw, "resolved_code")?;
    let resolved_code = v
        .get("resolved_code")
        .and_then(Value::as_str)
        .context("resolved_code must be a string")?
        .to_string();
    let explanation = v
        .get("explanation")
        .and_then(Value::as_str)
        .context("explanation must be a string")?
        .to_string();
    Ok(Suggestion {
        resolved_code,
        explanation,
    })
}

fn parse_validation(raw: &str) -> Result<Validation> {
    let v = api::parse_model_json_with_key(raw, "accepted")?;
    let accepted = v
        .get("accepted")
        .and_then(Value::as_bool)
        .context("accepted must be a boolean")?;
    let reason = v
        .get("reason")
        .and_then(Value::as_str)
        .context("reason must be a string")?
        .to_string();
    let concerns = v
        .get("concerns")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|v| {
                    v.as_str()
                        .map(ToString::to_string)
                        .context("concerns entries must be strings")
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(Validation {
        accepted,
        reason,
        concerns,
    })
}

fn parse_post_apply_review(raw: &str) -> Result<(PostApplyVerdict, String)> {
    let v = api::parse_model_json_with_key(raw, "verdict")?;
    let verdict_str = v
        .get("verdict")
        .and_then(Value::as_str)
        .context("verdict must be a string")?;
    let verdict = PostApplyVerdict::parse(verdict_str)?;
    let explanation = v
        .get("explanation")
        .and_then(Value::as_str)
        .context("explanation must be a string")?
        .to_string();
    Ok((verdict, explanation))
}

fn post_apply_review_user_payload(
    commit_id: &str,
    commit_context: &str,
    prefetched_context_block: &str,
    accepted: &[AcceptedSuggestion],
) -> String {
    let mut hunks = String::new();
    for s in accepted {
        hunks.push_str(&format!(
            "- {}:{}\n    Resolution explanation: {}\n    Per-hunk validator reason: {}\n",
            s.file_path,
            s.start_line,
            one_line(&s.explanation),
            one_line(&s.validation_reason),
        ));
    }
    if hunks.is_empty() {
        hunks.push_str("- (none; clean cherry-pick had no conflict hunks, but the symbol-resolution check below still applies to the full diff)\n");
    }
    format!(
        r#"# Commit just applied (now at HEAD)

Commit id: {commit_id}

The COMPLETE diff for this commit is below. Your audit must cover EVERY `+` line, not only the conflict-resolved hunks listed further down. The backport-gap class of failures (a `+` line referencing a struct field / function / type / macro that exists upstream but was never backported into the target tree) frequently appears OUTSIDE the conflict regions.

```diff
{commit_context}
```

{prefetched_context_block}
# STEP 1 - mandatory symbol-resolution check (apply to the ENTIRE diff above)

For every identifier appearing on a `+` line - struct/union field via `->` or `.`, function call, type name, macro, enum value, sysctl/tracepoint name - verify it exists in the target tree at HEAD using `grep_repo` (fixed_string=true) and/or `read_symbol`. For `expr->FIELD` / `expr.FIELD`, identify the type of `expr` from the surrounding code, then `read_symbol path=<file> symbol=<struct_name>` and confirm FIELD is present. If a referenced symbol is missing in the target tree, call edit_file to repair the diff or return "needs_human". DO NOT return "clean" while any `+`-line identifier is unresolved.

# STEP 2 - additional risk surface: auto-resolved conflict hunks

These hunks were proposed by another model and approved by a per-hunk validator. They are higher-risk than the unconflicted parts of the diff, but they are not the only place defects can hide - STEP 1 above must run first and cover the entire diff regardless of conflict location.

{hunks}
# Your final reply

After both STEP 1 and STEP 2 are complete, return ONLY the JSON object:
{{"verdict":"clean"|"amended"|"needs_human","explanation":"string"}}

For "amended", include the file:line and symbol name(s) you repaired. For "needs_human", include the offending file:line and the missing identifier.
"#,
        commit_id = commit_id,
        commit_context = commit_context,
        prefetched_context_block = prefetched_context_block,
        hunks = hunks,
    )
}

fn post_apply_static_repair_user_payload(
    commit_id: &str,
    commit_context: &str,
    prefetched_context_block: &str,
    check: &PostApplyStaticCheck,
    attempt: u32,
) -> String {
    let issues = if check.issues.is_empty() {
        "- (source checker failed without structured issues)\n".to_string()
    } else {
        check
            .issues
            .iter()
            .map(|issue| {
                format!(
                    "- {}:{} `{}` expects `struct {}` to contain `{}`\n  Reason: {}\n",
                    issue.file_path,
                    issue.line,
                    issue.expression,
                    issue.struct_name,
                    issue.field,
                    one_line(&issue.reason),
                )
            })
            .collect::<String>()
    };
    let reason = check.reason.as_deref().map(one_line).unwrap_or_else(|| {
        "source checker reported unresolved struct field references".to_string()
    });
    format!(
        r#"# Commit just applied (now at HEAD)

Commit id: {commit_id}
Repair attempt: {attempt}/{max_attempts}

The COMPLETE current HEAD diff is below:

```diff
{commit_context}
```

{prefetched_context_block}
# Source-only checker result

Status: {status}
Reason: {reason}

Issues:
{issues}
# Your task

Use the provided repository tools to inspect the surrounding code and repair the listed missing-field backport gap(s) with the smallest safe edit. The repair must preserve target-tree adaptations and the intended upstream change.

Return ONLY the JSON object:
{{"verdict":"clean"|"amended"|"needs_human","explanation":"string"}}
"#,
        commit_id = commit_id,
        attempt = attempt,
        max_attempts = MAX_STATIC_REPAIR_ATTEMPTS,
        commit_context = commit_context,
        prefetched_context_block = prefetched_context_block,
        status = check.status.as_str(),
        reason = reason,
        issues = issues,
    )
}

fn one_line(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut last_space = false;
    for c in trimmed.chars() {
        if c == '\n' || c == '\r' || c == '\t' {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = c == ' ';
        }
    }
    out
}

async fn prefetch_context_block(repo: &Path, patch_diff: &str, verbose: &VerboseDest) -> String {
    match prefetch::prompt_block(repo, patch_diff).await {
        Ok(Some(block)) => {
            verbose.line(format!(
                "apply: pre-fetched source context: {} characters",
                block.context_chars
            ));
            block.text
        }
        Ok(None) => {
            verbose.line("apply: pre-fetched source context: empty".to_string());
            String::new()
        }
        Err(e) => {
            verbose.line(format!(
                "apply: pre-fetched source context failed (continuing without): {e:#}"
            ));
            String::new()
        }
    }
}

/// Run the tool-enabled post-apply review.
///
/// Called after `cherry-pick --continue --no-edit` creates the new HEAD commit. The validation
/// model gets every read-only review tool plus the write-capable `edit_file`. If `edit_file` is
/// called, we stage the modified files and amend the commit. Token usage is folded into the
/// run-wide totals.
#[allow(clippy::too_many_arguments)]
async fn post_apply_review_stage(
    client: &reqwest::Client,
    validation_model: &config::ResolvedModel,
    repo: &Path,
    commit_id: &str,
    accepted: &[AcceptedSuggestion],
    verbose: &VerboseDest,
    worker_line: Option<&WorkerLineCtx>,
    usage: &mut UsageTotals,
    usage_steps: &mut Vec<ApplyUsageStep>,
    cumulative: &mut CumulativeTokenUsage,
) -> Result<PostApplyReview> {
    // HEAD is the just-applied commit; use that as the canonical source of the diff.
    let head_show = git::show_patch(repo, "HEAD").context("git show HEAD for post-apply review")?;
    let head_diff = git::show_patch_diff_only(repo, "HEAD")
        .context("git show HEAD diff for post-apply review")?;
    let prefetched_context_block = prefetch_context_block(repo, &head_diff, verbose).await;
    let commit_context = api::cap_utf8(&head_show, MAX_COMMIT_CONTEXT_BYTES);
    let user = post_apply_review_user_payload(
        commit_id,
        &commit_context,
        &prefetched_context_block,
        accepted,
    );

    let tool_cfg = ToolLoopConfig::with_edit_file(repo);
    let label = "[apply] Post-apply review".to_string();
    let step_started = Instant::now();
    let (parsed, _raw, u, err, _attempts) = api::chat_completion_with_retry(
        client,
        validation_model,
        SYSTEM_APPLY_POST_REVIEW,
        &user,
        validation_model.temperature,
        Some(&label),
        Some(cumulative),
        verbose,
        Some(&tool_cfg),
        worker_line,
        repo,
        parse_post_apply_review,
        RETRY_REMINDER_POST_REVIEW,
        api::STAGE_RETRY_MAX_ATTEMPTS,
    )
    .await;
    record_usage_step(
        usage,
        usage_steps,
        "post-apply review",
        u,
        step_started,
        err.as_ref(),
    );

    // The apply command requires a clean worktree up front, so any tracked change left here
    // happened during the post-apply review/repair flow.
    let modified_files = post_apply_worktree_changes(repo)?;

    let (verdict, explanation) = match parsed {
        Some((v, e)) => (v, e),
        None => {
            let detail = err.as_ref().map(|e| format!(": {e:#}")).unwrap_or_default();
            // The model failed to return a valid verdict, but it may still have called edit_file.
            // Keep any tracked edits it made: we'll amend below if git status reports changes.
            let synthesized = format!(
                "post-apply review model did not produce a valid verdict{detail}. The cherry-pick is applied at HEAD; inspect the commit manually."
            );
            (PostApplyVerdict::NeedsHuman, synthesized)
        }
    };

    let (amend_stdout, amend_stderr) = if modified_files.is_empty() {
        if matches!(verdict, PostApplyVerdict::Amended) {
            verbose.line(
                "apply: post-apply review reported verdict=amended but no working-tree changes; treating as clean".to_string(),
            );
        }
        (String::new(), String::new())
    } else {
        verbose.line(format!(
            "apply: post-apply review left {} tracked file(s) modified; staging and amending",
            modified_files.len()
        ));
        amend_modified_files(repo, &modified_files)?
    };

    let final_verdict = if !modified_files.is_empty() {
        PostApplyVerdict::Amended
    } else if matches!(verdict, PostApplyVerdict::Amended) {
        // Model claimed amended but wrote nothing; downgrade to clean.
        PostApplyVerdict::Clean
    } else {
        verdict
    };

    Ok(PostApplyReview {
        verdict: final_verdict,
        explanation,
        modified_files,
        amend_stdout,
        amend_stderr,
    })
}

#[allow(clippy::too_many_arguments)]
async fn post_apply_static_check_with_repair(
    client: &reqwest::Client,
    validation_model: &config::ResolvedModel,
    repo: &Path,
    commit_id: &str,
    review: &mut Option<PostApplyReview>,
    verbose: &VerboseDest,
    worker_line: Option<&WorkerLineCtx>,
    usage: &mut UsageTotals,
    usage_steps: &mut Vec<ApplyUsageStep>,
    cumulative: &mut CumulativeTokenUsage,
) -> Option<PostApplyStaticCheck> {
    let mut check = post_apply_static_check_or_failure(repo, verbose, worker_line);

    for attempt in 1..=MAX_STATIC_REPAIR_ATTEMPTS {
        if !matches!(check.status, PostApplyStaticStatus::Failed) {
            return Some(check);
        }
        if check.issues.is_empty() {
            merge_static_check_into_review(review, &check);
            return Some(check);
        }

        let repair = post_apply_static_repair_stage(
            client,
            validation_model,
            repo,
            commit_id,
            &check,
            attempt,
            verbose,
            worker_line,
            usage,
            usage_steps,
            cumulative,
        )
        .await;

        let repair = match repair {
            Ok(r) => r,
            Err(e) => PostApplyReview {
                verdict: PostApplyVerdict::NeedsHuman,
                explanation: format!(
                    "post-apply source repair failed before producing a verdict: {e:#}. The cherry-pick is applied at HEAD; inspect the commit manually."
                ),
                modified_files: Vec::new(),
                amend_stdout: String::new(),
                amend_stderr: String::new(),
            },
        };
        let edited = !repair.modified_files.is_empty();
        let needs_human_without_edit =
            matches!(repair.verdict, PostApplyVerdict::NeedsHuman) && !edited;
        merge_post_apply_review(review, repair);

        check = post_apply_static_check_or_failure(repo, verbose, worker_line);
        if !matches!(check.status, PostApplyStaticStatus::Failed) {
            return Some(check);
        }
        if needs_human_without_edit {
            break;
        }
    }

    merge_static_check_into_review(review, &check);
    Some(check)
}

#[allow(clippy::too_many_arguments)]
async fn post_apply_static_repair_stage(
    client: &reqwest::Client,
    validation_model: &config::ResolvedModel,
    repo: &Path,
    commit_id: &str,
    check: &PostApplyStaticCheck,
    attempt: u32,
    verbose: &VerboseDest,
    worker_line: Option<&WorkerLineCtx>,
    usage: &mut UsageTotals,
    usage_steps: &mut Vec<ApplyUsageStep>,
    cumulative: &mut CumulativeTokenUsage,
) -> Result<PostApplyReview> {
    let head_show =
        git::show_patch(repo, "HEAD").context("git show HEAD for post-apply source repair")?;
    let head_diff = git::show_patch_diff_only(repo, "HEAD")
        .context("git show HEAD diff for post-apply source repair")?;
    let prefetched_context_block = prefetch_context_block(repo, &head_diff, verbose).await;
    let commit_context = api::cap_utf8(&head_show, MAX_COMMIT_CONTEXT_BYTES);
    let user = post_apply_static_repair_user_payload(
        commit_id,
        &commit_context,
        &prefetched_context_block,
        check,
        attempt,
    );

    let tool_cfg = ToolLoopConfig::with_edit_file(repo);
    let label =
        format!("[apply] Post-apply source repair ({attempt}/{MAX_STATIC_REPAIR_ATTEMPTS})");
    let step_started = Instant::now();
    let (parsed, _raw, u, err, _attempts) = api::chat_completion_with_retry(
        client,
        validation_model,
        SYSTEM_APPLY_STATIC_REPAIR,
        &user,
        validation_model.temperature,
        Some(&label),
        Some(cumulative),
        verbose,
        Some(&tool_cfg),
        worker_line,
        repo,
        parse_post_apply_review,
        RETRY_REMINDER_POST_REVIEW,
        api::STAGE_RETRY_MAX_ATTEMPTS,
    )
    .await;
    record_usage_step(
        usage,
        usage_steps,
        format!("source repair a{attempt}"),
        u,
        step_started,
        err.as_ref(),
    );

    let modified_files = post_apply_worktree_changes(repo)?;
    let (verdict, mut explanation) = match parsed {
        Some((v, e)) => (v, e),
        None => {
            let detail = err.as_ref().map(|e| format!(": {e:#}")).unwrap_or_default();
            (
                PostApplyVerdict::NeedsHuman,
                format!("post-apply source repair model did not produce a valid verdict{detail}."),
            )
        }
    };

    let (amend_stdout, amend_stderr) = if modified_files.is_empty() {
        if matches!(verdict, PostApplyVerdict::Amended) {
            if !explanation.trim().is_empty() {
                explanation.push_str("\n\n");
            }
            explanation.push_str("The repair model reported amended but did not modify any files.");
        }
        (String::new(), String::new())
    } else {
        verbose.line(format!(
            "apply: post-apply source repair left {} tracked file(s) modified; staging and amending",
            modified_files.len()
        ));
        amend_modified_files(repo, &modified_files)?
    };

    let final_verdict = if !modified_files.is_empty() {
        PostApplyVerdict::Amended
    } else if matches!(verdict, PostApplyVerdict::Amended) {
        PostApplyVerdict::NeedsHuman
    } else {
        verdict
    };

    Ok(PostApplyReview {
        verdict: final_verdict,
        explanation,
        modified_files,
        amend_stdout,
        amend_stderr,
    })
}

fn merge_post_apply_review(dst: &mut Option<PostApplyReview>, mut src: PostApplyReview) {
    match dst {
        Some(existing) => {
            existing.verdict = merge_post_apply_verdict(existing.verdict, src.verdict);
            if !src.explanation.trim().is_empty() {
                if !existing.explanation.trim().is_empty() {
                    existing.explanation.push_str("\n\n");
                }
                existing.explanation.push_str(src.explanation.trim());
            }
            existing.modified_files.append(&mut src.modified_files);
            existing.modified_files.sort();
            existing.modified_files.dedup();
            append_command_output(&mut existing.amend_stdout, &src.amend_stdout);
            append_command_output(&mut existing.amend_stderr, &src.amend_stderr);
        }
        None => *dst = Some(src),
    }
}

fn merge_post_apply_verdict(a: PostApplyVerdict, b: PostApplyVerdict) -> PostApplyVerdict {
    match (a, b) {
        (PostApplyVerdict::NeedsHuman, _) | (_, PostApplyVerdict::NeedsHuman) => {
            PostApplyVerdict::NeedsHuman
        }
        (PostApplyVerdict::Amended, _) | (_, PostApplyVerdict::Amended) => {
            PostApplyVerdict::Amended
        }
        _ => PostApplyVerdict::Clean,
    }
}

fn append_command_output(dst: &mut String, src: &str) {
    if src.is_empty() {
        return;
    }
    if !dst.is_empty() {
        dst.push('\n');
    }
    dst.push_str(src);
}

/// Return tracked files that differ from HEAD after post-apply review/repair.
/// Untracked paths are rejected instead of silently staging them into the amended commit.
fn post_apply_worktree_changes(repo: &Path) -> Result<Vec<String>> {
    let entries = git_status_porcelain(repo)?;
    let mut modified = Vec::new();
    let mut untracked = Vec::new();
    for entry in entries {
        if entry.code == "??" {
            untracked.push(entry.path);
        } else {
            modified.push(entry.path);
        }
    }
    if !untracked.is_empty() {
        anyhow::bail!(
            "post-apply edit stage left untracked file(s); refusing to amend them: {}",
            untracked.join(", ")
        );
    }
    Ok(modified)
}

/// Stage the listed files and run `git commit --amend --no-edit`. Returns the amend command's
/// captured stdout/stderr. Errors out if anything fails - the caller treats that as a
/// `NeedsHuman` verdict.
fn amend_modified_files(repo: &Path, files: &[String]) -> Result<(String, String)> {
    let mut add = Command::new("git");
    add.current_dir(repo).arg("add").arg("--");
    for f in files {
        add.arg(f);
    }
    let add_out = add.output().context("git add (post-apply amend)")?;
    if !add_out.status.success() {
        anyhow::bail!(
            "git add failed during post-apply amend\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&add_out.stdout),
            String::from_utf8_lossy(&add_out.stderr),
        );
    }
    let amend_out = Command::new("git")
        .current_dir(repo)
        .args(["commit", "--amend", "--no-edit"])
        .output()
        .context("git commit --amend --no-edit")?;
    if !amend_out.status.success() {
        anyhow::bail!(
            "git commit --amend --no-edit failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&amend_out.stdout),
            String::from_utf8_lossy(&amend_out.stderr),
        );
    }
    Ok((
        String::from_utf8_lossy(&amend_out.stdout).into_owned(),
        String::from_utf8_lossy(&amend_out.stderr).into_owned(),
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AddedLine {
    file_path: String,
    line: usize,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FieldAccess {
    base: String,
    field: String,
    expression: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StructFieldPresence {
    Present,
    Missing,
    Unknown,
}

fn post_apply_static_check_or_failure(
    repo: &Path,
    verbose: &VerboseDest,
    worker_line: Option<&WorkerLineCtx>,
) -> PostApplyStaticCheck {
    match post_apply_static_check(repo, verbose, worker_line) {
        Ok(check) => check,
        Err(e) => {
            verbose.line(format!(
                "apply: post-apply source check failed to run: {e:#}"
            ));
            PostApplyStaticCheck {
                status: PostApplyStaticStatus::Failed,
                reason: Some(format!("post-apply source check failed to run: {e:#}")),
                issues: Vec::new(),
            }
        }
    }
}

fn post_apply_static_check(
    repo: &Path,
    verbose: &VerboseDest,
    worker_line: Option<&WorkerLineCtx>,
) -> Result<PostApplyStaticCheck> {
    let added = added_lines_in_head_diff(repo)?;
    if added.is_empty() {
        return Ok(PostApplyStaticCheck {
            status: PostApplyStaticStatus::Skipped,
            reason: Some("HEAD diff has no added source lines".to_string()),
            issues: Vec::new(),
        });
    }

    verbose.line("apply: post-apply source check: verifying newly-added struct field accesses");
    if let Some(worker) = worker_line {
        worker.set_line_message("[apply] Post-apply source check".to_string());
    }

    let mut file_cache = std::collections::BTreeMap::<String, String>::new();
    let mut field_cache =
        std::collections::BTreeMap::<(String, String), StructFieldPresence>::new();
    let mut inspected = 0usize;
    let mut issues = Vec::new();

    for added_line in added {
        if !is_c_source_path(&added_line.file_path) {
            continue;
        }
        let accesses = extract_field_accesses(&added_line.text);
        if accesses.is_empty() {
            continue;
        }
        let content = match cached_file_content(repo, &added_line.file_path, &mut file_cache) {
            Ok(content) => content,
            Err(e) => {
                issues.push(PostApplyStaticIssue {
                    file_path: added_line.file_path,
                    line: added_line.line,
                    expression: added_line.text.trim().to_string(),
                    struct_name: String::new(),
                    field: String::new(),
                    reason: format!("could not read changed file for source check: {e:#}"),
                });
                continue;
            }
        };
        for access in accesses {
            let Some(struct_name) =
                find_struct_type_for_var(content, added_line.line, &access.base)
            else {
                continue;
            };
            inspected += 1;
            let key = (struct_name.clone(), access.field.clone());
            let presence = match field_cache.get(&key).copied() {
                Some(p) => p,
                None => {
                    let p = struct_field_presence(repo, &struct_name, &access.field)?;
                    field_cache.insert(key, p);
                    p
                }
            };
            if matches!(presence, StructFieldPresence::Missing) {
                issues.push(PostApplyStaticIssue {
                    file_path: added_line.file_path.clone(),
                    line: added_line.line,
                    expression: access.expression,
                    struct_name,
                    field: access.field,
                    reason: "field is referenced by an added line but is absent from the struct definition at HEAD".to_string(),
                });
            }
        }
    }

    let status = if !issues.is_empty() {
        PostApplyStaticStatus::Failed
    } else if inspected == 0 {
        PostApplyStaticStatus::Skipped
    } else {
        PostApplyStaticStatus::Passed
    };
    let reason = match status {
        PostApplyStaticStatus::Failed => Some(format!(
            "{} newly-added struct field reference(s) are unresolved",
            issues.len()
        )),
        PostApplyStaticStatus::Skipped => {
            Some("no newly-added struct field accesses with an inferable struct type".to_string())
        }
        PostApplyStaticStatus::Passed => None,
    };

    Ok(PostApplyStaticCheck {
        status,
        reason,
        issues,
    })
}

fn added_lines_in_head_diff(repo: &Path) -> Result<Vec<AddedLine>> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["show", "--format=", "--unified=0", "HEAD"])
        .output()
        .context("git show --format= --unified=0 HEAD")?;
    if !out.status.success() {
        anyhow::bail!(
            "git show --format= --unified=0 HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(parse_added_lines_from_diff(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

fn parse_added_lines_from_diff(diff: &str) -> Vec<AddedLine> {
    let mut out = Vec::new();
    let mut file_path: Option<String> = None;
    let mut new_line: Option<usize> = None;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            file_path = None;
            new_line = None;
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ b/") {
            file_path = Some(path.to_string());
            continue;
        }
        if line.starts_with("@@") {
            new_line = parse_hunk_new_start(line);
            continue;
        }
        let Some(current_line) = new_line else {
            continue;
        };
        if line.starts_with("+++") {
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            if let Some(file_path) = &file_path {
                out.push(AddedLine {
                    file_path: file_path.clone(),
                    line: current_line,
                    text: text.to_string(),
                });
            }
            new_line = Some(current_line + 1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            // Removed lines do not advance the post-image line number.
        } else if line.starts_with(' ') {
            new_line = Some(current_line + 1);
        }
    }

    out
}

fn parse_hunk_new_start(line: &str) -> Option<usize> {
    let plus = line.find('+')?;
    let rest = &line[plus + 1..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn is_c_source_path(path: &str) -> bool {
    path.ends_with(".c") || path.ends_with(".h")
}

fn cached_file_content<'a>(
    repo: &Path,
    file_path: &str,
    cache: &'a mut std::collections::BTreeMap<String, String>,
) -> Result<&'a str> {
    if !cache.contains_key(file_path) {
        let path = repo.join(file_path);
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read changed file {}", path.display()))?;
        cache.insert(file_path.to_string(), content);
    }
    Ok(cache.get(file_path).map(String::as_str).unwrap_or(""))
}

fn extract_field_accesses(line: &str) -> Vec<FieldAccess> {
    let clean = strip_c_comments_and_strings(line);
    let bytes = clean.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] != b'-' || bytes[i + 1] != b'>' {
            i += 1;
            continue;
        }

        let mut left = i;
        while left > 0 && bytes[left - 1].is_ascii_whitespace() {
            left -= 1;
        }
        let ident_end = left;
        while left > 0 && is_ident_byte(bytes[left - 1]) {
            left -= 1;
        }
        if left == ident_end {
            i += 2;
            continue;
        }

        let mut right = i + 2;
        while right < bytes.len() && bytes[right].is_ascii_whitespace() {
            right += 1;
        }
        let field_start = right;
        while right < bytes.len() && is_ident_byte(bytes[right]) {
            right += 1;
        }
        if field_start == right {
            i += 2;
            continue;
        }

        let base = clean[left..ident_end].to_string();
        let field = clean[field_start..right].to_string();
        out.push(FieldAccess {
            expression: format!("{base}->{field}"),
            base,
            field,
        });
        i = right;
    }
    out
}

fn find_struct_type_for_var(content: &str, line_no: usize, var: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let end = line_no.min(lines.len());
    let start = end.saturating_sub(800);

    for idx in (start..end).rev() {
        let line = strip_c_comments_and_strings(lines[idx]);
        if let Some(struct_name) = parse_struct_decl_for_var(&line, var) {
            return Some(struct_name);
        }
    }
    None
}

fn parse_struct_decl_for_var(line: &str, var: &str) -> Option<String> {
    let mut offset = 0usize;
    while let Some(pos) = line[offset..].find("struct") {
        let struct_pos = offset + pos;
        let before = line[..struct_pos].chars().next_back();
        let after = line[struct_pos + "struct".len()..].chars().next();
        if before.map(is_ident_char).unwrap_or(false) || after.map(is_ident_char).unwrap_or(false) {
            offset = struct_pos + "struct".len();
            continue;
        }

        let mut i = struct_pos + "struct".len();
        while i < line.len() && line.as_bytes()[i].is_ascii_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < line.len() && is_ident_byte(line.as_bytes()[i]) {
            i += 1;
        }
        if name_start == i {
            offset = struct_pos + "struct".len();
            continue;
        }
        let name = &line[name_start..i];
        let rest = &line[i..];
        if rest.trim_start().starts_with('{') {
            offset = i;
            continue;
        }
        if declarator_list_mentions_var(rest, var) {
            return Some(name.to_string());
        }
        offset = i;
    }
    None
}

fn declarator_list_mentions_var(rest: &str, var: &str) -> bool {
    let decls = rest
        .split(';')
        .next()
        .unwrap_or(rest)
        .split('{')
        .next()
        .unwrap_or(rest);
    for decl in decls.split(',') {
        let decl = decl.split('=').next().unwrap_or(decl);
        if last_identifier(decl).as_deref() == Some(var) {
            return true;
        }
    }
    false
}

fn last_identifier(s: &str) -> Option<String> {
    let mut last = None;
    let mut iter = s.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if !(ch == '_' || ch.is_ascii_alphabetic()) {
            continue;
        }
        let start = idx;
        let mut end = idx + ch.len_utf8();
        while let Some((next_idx, next_ch)) = iter.peek().copied() {
            if !is_ident_char(next_ch) {
                break;
            }
            iter.next();
            end = next_idx + next_ch.len_utf8();
        }
        last = Some(s[start..end].to_string());
    }
    last
}

fn struct_field_presence(
    repo: &Path,
    struct_name: &str,
    field: &str,
) -> Result<StructFieldPresence> {
    let files = struct_definition_files(repo, struct_name)?;
    if files.is_empty() {
        return Ok(StructFieldPresence::Unknown);
    }

    let mut saw_definition = false;
    for file in files {
        let content = fs::read_to_string(repo.join(&file))
            .with_context(|| format!("read struct definition candidate {file}"))?;
        for body in extract_struct_bodies(&content, struct_name) {
            saw_definition = true;
            if contains_identifier(&body, field) {
                return Ok(StructFieldPresence::Present);
            }
        }
    }

    if saw_definition {
        Ok(StructFieldPresence::Missing)
    } else {
        Ok(StructFieldPresence::Unknown)
    }
}

fn struct_definition_files(repo: &Path, struct_name: &str) -> Result<Vec<String>> {
    let pattern = format!(r"struct[[:space:]]+{struct_name}");
    let out = Command::new("git")
        .current_dir(repo)
        .args(["grep", "-l", "-E", &pattern])
        .output()
        .with_context(|| format!("git grep struct {struct_name}"))?;
    if !out.status.success() {
        if out.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        anyhow::bail!(
            "git grep struct {} failed: {}",
            struct_name,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn extract_struct_bodies(content: &str, struct_name: &str) -> Vec<String> {
    let clean = strip_c_comments_and_strings(content);
    let needle = format!("struct {struct_name}");
    let mut out = Vec::new();
    let mut offset = 0usize;

    while let Some(pos) = clean[offset..].find(&needle) {
        let start = offset + pos;
        let after_name = start + needle.len();
        if clean[after_name..]
            .chars()
            .next()
            .map(is_ident_char)
            .unwrap_or(false)
        {
            offset = after_name;
            continue;
        }

        let mut brace = after_name;
        while brace < clean.len() && clean.as_bytes()[brace].is_ascii_whitespace() {
            brace += 1;
        }
        if clean.as_bytes().get(brace).copied() != Some(b'{') {
            offset = after_name;
            continue;
        }

        if let Some(end) = matching_brace_end(&clean, brace) {
            out.push(clean[brace + 1..end].to_string());
            offset = end + 1;
        } else {
            break;
        }
    }

    out
}

fn matching_brace_end(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in s[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open + idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_c_comments_and_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_block_comment = false;
    let mut in_line_comment = false;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
                out.push('\n');
            } else {
                out.push(' ');
            }
            continue;
        }
        if in_block_comment {
            if ch == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block_comment = false;
                out.push(' ');
                out.push(' ');
            } else if ch == '\n' {
                out.push('\n');
            } else {
                out.push(' ');
            }
            continue;
        }
        if in_string || in_char {
            let terminator = if in_string { '"' } else { '\'' };
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == terminator {
                in_string = false;
                in_char = false;
            }
            out.push(if ch == '\n' { '\n' } else { ' ' });
            continue;
        }

        if ch == '/' && chars.peek() == Some(&'/') {
            chars.next();
            in_line_comment = true;
            out.push(' ');
            out.push(' ');
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_block_comment = true;
            out.push(' ');
            out.push(' ');
        } else if ch == '"' {
            in_string = true;
            out.push(' ');
        } else if ch == '\'' {
            in_char = true;
            out.push(' ');
        } else {
            out.push(ch);
        }
    }

    out
}

fn contains_identifier(s: &str, needle: &str) -> bool {
    let bytes = s.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || needle_bytes.len() > bytes.len() {
        return false;
    }
    for idx in 0..=bytes.len() - needle_bytes.len() {
        if &bytes[idx..idx + needle_bytes.len()] != needle_bytes {
            continue;
        }
        let before = idx
            .checked_sub(1)
            .and_then(|i| bytes.get(i))
            .copied()
            .map(is_ident_byte)
            .unwrap_or(false);
        let after = bytes
            .get(idx + needle_bytes.len())
            .copied()
            .map(is_ident_byte)
            .unwrap_or(false);
        if !before && !after {
            return true;
        }
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn is_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn merge_static_check_into_review(
    review: &mut Option<PostApplyReview>,
    check: &PostApplyStaticCheck,
) {
    if !matches!(check.status, PostApplyStaticStatus::Failed) {
        return;
    }
    let explanation = static_check_failure_explanation(check);
    match review {
        Some(r) => {
            r.verdict = PostApplyVerdict::NeedsHuman;
            if !r.explanation.trim().is_empty() {
                r.explanation.push_str("\n\n");
            }
            r.explanation.push_str(&explanation);
        }
        None => {
            *review = Some(PostApplyReview {
                verdict: PostApplyVerdict::NeedsHuman,
                explanation,
                modified_files: Vec::new(),
                amend_stdout: String::new(),
                amend_stderr: String::new(),
            });
        }
    }
}

fn static_check_failure_explanation(check: &PostApplyStaticCheck) -> String {
    if check.issues.is_empty() {
        return check
            .reason
            .clone()
            .unwrap_or_else(|| "post-apply source check failed".to_string());
    }
    let mut out = format!(
        "post-apply source check found {} unresolved newly-added struct field reference(s):",
        check.issues.len()
    );
    for issue in &check.issues {
        out.push_str(&format!(
            "\n- {}:{}: `{}` expects `struct {}` to contain `{}` ({})",
            issue.file_path,
            issue.line,
            issue.expression,
            issue.struct_name,
            issue.field,
            issue.reason
        ));
    }
    out
}

fn apply_accepted_resolutions(repo: &Path, accepted: &[AcceptedSuggestion]) -> Result<()> {
    use std::collections::BTreeMap;

    let mut by_file: BTreeMap<&str, Vec<&AcceptedSuggestion>> = BTreeMap::new();
    for suggestion in accepted {
        by_file
            .entry(suggestion.file_path.as_str())
            .or_default()
            .push(suggestion);
    }

    for (file_path, mut suggestions) in by_file {
        suggestions.sort_by_key(|s| s.start_line);
        let path = repo.join(file_path);
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read conflicted file {}", path.display()))?;
        let mut lines: Vec<String> = content
            .split_inclusive('\n')
            .map(ToString::to_string)
            .collect();

        for suggestion in suggestions.iter().rev() {
            let start = suggestion
                .start_line
                .checked_sub(1)
                .context("invalid conflict start line")?;
            let end = suggestion.end_line;
            if start >= lines.len() || end > lines.len() || start >= end {
                anyhow::bail!(
                    "{}:{}: conflict range no longer matches file contents",
                    suggestion.file_path,
                    suggestion.start_line
                );
            }
            if !lines[start].starts_with("<<<<<<<") {
                anyhow::bail!(
                    "{}:{}: expected conflict start marker before rewriting",
                    suggestion.file_path,
                    suggestion.start_line
                );
            }
            if !lines[end - 1].starts_with(">>>>>>>") {
                anyhow::bail!(
                    "{}:{}: expected conflict end marker before rewriting",
                    suggestion.file_path,
                    suggestion.start_line
                );
            }
            let mut replacement: Vec<String> = suggestion
                .resolved_code
                .split_inclusive('\n')
                .map(ToString::to_string)
                .collect();
            // Pick the file's EOL *style* for re-terminating replacement lines.
            // A surrounding unchanged line is the most reliable source, since it
            // is verbatim from the file; scan outward from the conflict (nearest
            // preceding line first, then following lines) for one that is newline
            // terminated. Fall back to the >>>>>>> marker line, which git renders
            // in the file's style, for a conflict that spans the whole file and
            // has no surrounding content. None of these tell us whether a final
            // newline exists (git always terminates the marker even when the EOF
            // blob had none), so terminators are only ever added mid-file; at EOF
            // resolved_code's own "no newline at end" convention is preserved.
            let eol_sample = lines[..start]
                .iter()
                .rev()
                .chain(lines[end..].iter())
                .find(|line| line.ends_with('\n'))
                .map_or(lines[end - 1].as_str(), String::as_str);
            let eol = if eol_sample.ends_with("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let at_eof = end == lines.len();
            let last_idx = replacement.len().saturating_sub(1);
            for (i, line) in replacement.iter_mut().enumerate() {
                if let Some(body) = line.strip_suffix('\n') {
                    // A real line terminator (LF, optionally preceded by CR):
                    // normalize it to the file's style so a CRLF file never gets a
                    // lone \n. This covers interior lines, which split_inclusive
                    // always leaves \n-terminated regardless of the file's
                    // convention. A bare trailing \r is content, not a terminator,
                    // so it is deliberately not matched here.
                    let body = body.strip_suffix('\r').unwrap_or(body);
                    *line = format!("{body}{eol}");
                } else if i != last_idx || !at_eof {
                    // An unterminated interior line, or an unterminated final line
                    // that is not at EOF, would fuse onto the following line. Only
                    // a final line at EOF is left bare, preserving resolved_code's
                    // own "no newline at end of file" convention.
                    line.push_str(eol);
                }
            }
            lines.splice(start..end, replacement);
        }

        fs::write(&path, lines.concat())
            .with_context(|| format!("write resolved file {}", path.display()))?;
    }

    Ok(())
}

fn stage_resolved_files(repo: &Path, accepted: &[AcceptedSuggestion]) -> Result<()> {
    let mut files: Vec<&str> = accepted.iter().map(|s| s.file_path.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    if files.is_empty() {
        return Ok(());
    }

    let mut cmd = Command::new("git");
    cmd.current_dir(repo).arg("add").arg("--");
    for file in files {
        cmd.arg(file);
    }
    let out = cmd.output().context("git add resolved files")?;
    if !out.status.success() {
        anyhow::bail!(
            "git add resolved files failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn use_color_stdout() -> bool {
    io::stdout().is_terminal()
}

fn print_apply_section_title(title: &str) {
    let color = use_color_stdout();
    println!();
    println!("{}", style_apply_section_title(title, color));
    println!("{}", style_dimmed(&"-".repeat(APPLY_TEXT_WIDTH), color));
}

fn print_apply_hunk_header(header: &str, color: bool) {
    println!("{}", style_hunk_header(header, color));
    println!(
        "{}",
        style_dimmed(&"·".repeat(APPLY_TEXT_WIDTH.min(60)), color)
    );
}

fn print_applied_dependencies(dependencies: &[AppliedDependency]) {
    if dependencies.is_empty() {
        return;
    }
    let color = use_color_stdout();
    print_apply_section_title("Dependent commits");
    for dep in dependencies {
        println!(
            "{} {}",
            style_hunk_header(&dep.commit[..dep.commit.len().min(12)], color),
            dep.subject
        );
    }
}

fn print_post_apply_review(review: &PostApplyReview) {
    let color = use_color_stdout();
    print_apply_section_title("Post-apply review");
    println!();
    println!("Verdict: {}", review.verdict.as_str());
    if !review.modified_files.is_empty() {
        println!("Amended files:");
        for f in &review.modified_files {
            println!("  - {f}");
        }
    }
    if !review.explanation.trim().is_empty() {
        println!();
        print!(
            "{}",
            format_lkml_field(
                "Explanation",
                review.explanation.trim(),
                APPLY_TEXT_WIDTH,
                color,
            )
        );
    }
}

fn print_post_apply_static_check(check: &PostApplyStaticCheck) {
    let color = use_color_stdout();
    print_apply_section_title("Post-apply source check");
    println!();
    println!("Status: {}", check.status.as_str());
    if let Some(reason) = check.reason.as_deref().filter(|s| !s.trim().is_empty()) {
        println!();
        print!(
            "{}",
            format_lkml_field("Reason", reason.trim(), APPLY_TEXT_WIDTH, color)
        );
    }
    if !check.issues.is_empty() {
        println!();
        println!("Issues:");
        for issue in &check.issues {
            println!(
                "  - {}:{} `{}`: struct {} has no member `{}`",
                issue.file_path, issue.line, issue.expression, issue.struct_name, issue.field
            );
        }
    }
}

fn style_apply_section_title(title: &str, color: bool) -> String {
    let s = format!(" {title} ");
    if color {
        s.bold().bright_cyan().to_string()
    } else {
        s
    }
}

fn style_hunk_header(header: &str, color: bool) -> String {
    if color {
        header.bold().bright_magenta().to_string()
    } else {
        header.to_string()
    }
}

fn style_dimmed(s: &str, color: bool) -> String {
    if color {
        s.dimmed().to_string()
    } else {
        s.to_string()
    }
}

fn style_field_label(label: &str, color: bool) -> String {
    let s = format!("{label}:");
    if color {
        s.bold().dimmed().to_string()
    } else {
        s
    }
}

fn format_lkml_field(label: &str, text: &str, width: usize, color: bool) -> String {
    let mut out = String::new();
    out.push_str(&style_field_label(label, color));
    out.push('\n');
    for line in wrap_lkml_text(text, width) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn wrap_lkml_text(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut para = String::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            flush_lkml_paragraph(&mut out, &mut para, width);
            if !out.last().map(|s| s.is_empty()).unwrap_or(false) {
                out.push(String::new());
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("- ") {
            flush_lkml_paragraph(&mut out, &mut para, width);
            out.extend(wrap_prefixed_words(rest.trim(), "- ", "  ", width));
            continue;
        }

        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(line);
    }

    flush_lkml_paragraph(&mut out, &mut para, width);
    while out.last().map(|s| s.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
}

fn flush_lkml_paragraph(out: &mut Vec<String>, para: &mut String, width: usize) {
    if para.trim().is_empty() {
        para.clear();
        return;
    }
    out.extend(wrap_prefixed_words(para.trim(), "", "", width));
    para.clear();
}

fn wrap_prefixed_words(
    text: &str,
    first_prefix: &str,
    cont_prefix: &str,
    width: usize,
) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = first_prefix.to_string();
    let mut line_prefix_len = first_prefix.chars().count();
    let mut cont = false;

    for word in text.split_whitespace() {
        let mut rest = word;
        loop {
            let line_len = line.chars().count();
            let sep_len = usize::from(line_len > line_prefix_len);
            let available = width.saturating_sub(line_len + sep_len);

            if rest.chars().count() <= available {
                if sep_len == 1 {
                    line.push(' ');
                }
                line.push_str(rest);
                break;
            }

            if line_len > line_prefix_len {
                out.push(line);
                line = cont_prefix.to_string();
                line_prefix_len = cont_prefix.chars().count();
                cont = true;
                continue;
            }

            let chunk_width = width.saturating_sub(line_prefix_len).max(1);
            let (chunk, remainder) = split_word(rest, chunk_width);
            line.push_str(chunk);
            out.push(line);
            line = cont_prefix.to_string();
            line_prefix_len = cont_prefix.chars().count();
            cont = true;
            rest = remainder;
            if rest.is_empty() {
                break;
            }
        }
    }

    if line.chars().count() > line_prefix_len || !cont {
        out.push(line);
    }
    out
}

fn split_word(s: &str, max_chars: usize) -> (&str, &str) {
    let max_chars = max_chars.max(1);
    let split = s
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    s.split_at(split)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_json_carries_usage_steps() {
        let mut outcome = dry_run("abc123");
        outcome.usage = UsageTotals {
            api_calls: 1,
            prompt: 1234,
            completion: 56,
            cache_creation: 78,
            cache_read: 90,
        };
        outcome.usage_steps.push(ApplyUsageStep {
            step: "validate h1 a2".to_string(),
            usage: TokenUsage {
                prompt: Some(1234),
                completion: Some(56),
                cache_creation: Some(78),
                cache_read: Some(90),
            },
            wall_ms: 1500,
            error: Some("schema retry".to_string()),
        });

        let model = config::dry_run_placeholder();
        let out = outcome.json(&model, &model);
        let steps = out["usage_steps"].as_array().unwrap();

        assert_eq!(out["usage_summary"]["prompt_tokens"], 1234);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["step"], "validate h1 a2");
        assert_eq!(steps[0]["prompt_tokens"], 1234);
        assert_eq!(steps[0]["completion_tokens"], 56);
        assert_eq!(steps[0]["cache_creation_tokens"], 78);
        assert_eq!(steps[0]["cache_read_tokens"], 90);
        assert_eq!(steps[0]["wall_ms"], 1500);
        assert_eq!(steps[0]["error"], "schema retry");
    }

    #[test]
    fn parse_single_diff3_conflict() {
        let content = "before\n<<<<<<< HEAD\nlocal\n||||||| parent\nbase\n=======\nremote\n>>>>>>> commit\nafter\n";
        let conflicts = parse_conflicts_in_file("kernel/foo.c", content).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].file_path, "kernel/foo.c");
        assert_eq!(conflicts[0].start_line, 2);
        assert_eq!(conflicts[0].end_line, 8);
        assert_eq!(conflicts[0].local, "local\n");
        assert_eq!(conflicts[0].base, "base\n");
        assert_eq!(conflicts[0].remote, "remote\n");
    }

    #[test]
    fn parse_crlf_diff3_conflict() {
        // git writes conflict markers with the file's own line ending, so a CRLF
        // file yields a `=======\r\n` separator. Parsing must still find the hunk.
        let content = "before\r\n<<<<<<< HEAD\r\nlocal\r\n||||||| parent\r\nbase\r\n=======\r\nremote\r\n>>>>>>> commit\r\nafter\r\n";
        let conflicts = parse_conflicts_in_file("kernel/foo.c", content).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].start_line, 2);
        assert_eq!(conflicts[0].end_line, 8);
        assert_eq!(conflicts[0].local, "local\r\n");
        assert_eq!(conflicts[0].base, "base\r\n");
        assert_eq!(conflicts[0].remote, "remote\r\n");
    }

    #[test]
    fn parse_multiple_diff3_conflicts() {
        let content = "<<<<<<< HEAD\nl1\n||||||| base\nb1\n=======\nr1\n>>>>>>> remote\nmid\n<<<<<<< HEAD\nl2\n||||||| base\nb2\n=======\nr2\n>>>>>>> remote\n";
        let conflicts = parse_conflicts_in_file("x.c", content).unwrap();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].start_line, 1);
        assert_eq!(conflicts[1].start_line, 9);
        assert_eq!(conflicts[1].local, "l2\n");
    }

    #[test]
    fn parse_missing_base_marker_rejected() {
        let content = "<<<<<<< HEAD\nlocal\n=======\nremote\n>>>>>>> commit\n";
        let err = parse_conflicts_in_file("x.c", content).unwrap_err();
        assert!(err.to_string().contains("missing diff3 base marker"));
    }

    #[test]
    fn parse_suggestion_accepts_fenced_json() {
        let s = parse_suggestion(
            "```json\n{\"resolved_code\":\"ok\\n\",\"explanation\":\"applied\"}\n```",
        )
        .unwrap();
        assert_eq!(s.resolved_code, "ok\n");
        assert_eq!(s.explanation, "applied");
    }

    #[test]
    fn parse_validation_rejects_missing_reason() {
        let err = parse_validation("{\"accepted\":true,\"concerns\":[]}").unwrap_err();
        assert!(err.to_string().contains("reason"));
    }

    #[test]
    fn sanitize_subject_drops_ubuntu_and_sauce_prefixes() {
        assert_eq!(sanitize_subject("UBUNTU: foo"), "foo");
        assert_eq!(sanitize_subject("SAUCE foo"), "foo");
        assert_eq!(sanitize_subject("plain subject"), "plain subject");
    }

    #[test]
    fn sanitize_subject_handles_any_order_and_repetition() {
        assert_eq!(
            sanitize_subject("SAUCE: UBUNTU: net: fix foo"),
            "net: fix foo"
        );
        assert_eq!(sanitize_subject("UBUNTU: SAUCE: x"), "x");
        assert_eq!(sanitize_subject("UBUNTU: UBUNTU: y"), "y");
        assert_eq!(sanitize_subject("net: fix thing"), "net: fix thing");
    }

    #[test]
    fn sanitize_subject_requires_delimiter_after_tag() {
        // A tag is only a tag when followed by a delimiter or end-of-string. A
        // prefix that merely begins a longer word must be left intact, otherwise
        // "SAUCE: UBUNTUFS: fix lookup" would collapse to "FS: fix lookup" and
        // compare equal to an unrelated subject.
        assert_eq!(
            sanitize_subject("SAUCE: UBUNTUFS: fix lookup"),
            "UBUNTUFS: fix lookup"
        );
        assert_eq!(
            sanitize_subject("UBUNTUFS: fix lookup"),
            "UBUNTUFS: fix lookup"
        );
        assert_eq!(sanitize_subject("SAUCEY change"), "SAUCEY change");
        // A bare tag with only a delimiter (or nothing) after it still sanitizes.
        assert_eq!(sanitize_subject("UBUNTU:"), "");
        assert_eq!(sanitize_subject("SAUCE"), "");
        // Whitespace delimiters other than a plain space (e.g. a tab) also count.
        assert_eq!(sanitize_subject("UBUNTU:\tfix"), "fix");
    }

    #[test]
    fn already_applied_match_requires_exact_sanitized_subject() {
        let log = "abc1234\0UBUNTU: target subject\nbad9999\0target subject extra\n";
        let m = find_already_applied_in_log("target subject", "kernel/foo.c", log).unwrap();

        assert_eq!(m.file_path, "kernel/foo.c");
        assert_eq!(m.commit, "abc1234");
        assert_eq!(m.subject, "target subject");
    }

    #[test]
    fn already_applied_match_rejects_substring_only() {
        let log = "bad9999\0target subject extra\n";
        assert!(find_already_applied_in_log("target subject", "kernel/foo.c", log).is_none());
    }

    #[test]
    fn already_applied_match_does_not_skip_tag_prefixed_word() {
        // Regression for the over-strip bug: "UBUNTUFS" must not be sanitized to
        // "FS", or an unrelated log commit would be reported as already-applied
        // and a real cherry-pick silently skipped.
        let log = "abc1234\0UBUNTUFS: fix lookup\n";
        assert!(find_already_applied_in_log("FS: fix lookup", "kernel/foo.c", log).is_none());
    }

    #[test]
    fn referenced_commit_ids_extracts_and_dedups_hex_tokens() {
        let msg = "\
This depends on abcdef1234567890 before the target can apply.
See also commit ABCDEF1234567890.
(cherry picked from commit 1111111222222233333334444444555555566666)
(backported from commit 2222222333333344444445555555666666677777)
";
        assert_eq!(referenced_commit_ids(msg), vec!["abcdef1234567890"]);
    }

    #[test]
    fn backport_commit_message_rewrites_only_cherry_pick_trailer() {
        let (message, changed) = backport_commit_message(
            "\
subject

Body mentioning cherry picked from commit in prose.
(cherry picked from commit 1111111222222233333334444444555555566666)
",
        );

        assert!(changed);
        assert!(
            message.contains("(backported from commit 1111111222222233333334444444555555566666)")
        );
        assert!(message.contains("Body mentioning cherry picked from commit in prose."));
        assert!(!message.contains("(cherry picked from commit "));
    }

    #[test]
    fn backport_commit_message_noops_without_cherry_pick_trailer() {
        let input =
            "subject\n\n(backported from commit 1111111222222233333334444444555555566666)\n";
        let (message, changed) = backport_commit_message(input);

        assert!(!changed);
        assert_eq!(message, input);
    }

    #[test]
    fn backport_commit_message_rewrites_mixed_case_cherry_pick_trailer() {
        let (message, changed) =
            backport_commit_message("subject\n\n(Cherry picked from commit abcdef1)\n");

        assert!(changed);
        assert!(message.contains("(backported from commit abcdef1)"));
        assert!(!message
            .to_ascii_lowercase()
            .contains("(cherry picked from commit "));
    }

    #[test]
    fn backport_commit_message_rewrites_canonical_lowercase_cherry_pick_trailer() {
        let (message, changed) =
            backport_commit_message("subject\n\n(cherry picked from commit abcdef1)\n");

        assert!(changed);
        assert!(message.contains("(backported from commit abcdef1)"));
    }

    #[test]
    fn backport_commit_message_noops_on_message_without_trailer() {
        let input = "subject\n\nbody text only\n";
        let (message, changed) = backport_commit_message(input);

        assert!(!changed);
        assert_eq!(message, input);
    }

    #[test]
    fn referenced_commit_ids_skips_mixed_case_cherry_pick_trailer() {
        let msg = "\
This depends on abcdef1234567890 before the target can apply.
(Cherry picked from commit 1111111222222233333334444444555555566666)
";
        assert_eq!(referenced_commit_ids(msg), vec!["abcdef1234567890"]);
    }

    #[test]
    fn rewrite_cherry_pick_trailer_as_backport_amends_head_message() {
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(dir.path().join("x.c"), "int x;\n").unwrap();
        run_git_test(dir.path(), &["add", "x.c"]).unwrap();
        run_git_test(
            dir.path(),
            &[
                "commit",
                "-m",
                "subject",
                "-m",
                "(cherry picked from commit 1111111222222233333334444444555555566666)",
            ],
        )
        .unwrap();

        assert!(rewrite_cherry_pick_trailer_as_backport(dir.path())
            .unwrap()
            .is_some());
        let message = commit_message(dir.path(), "HEAD").unwrap();
        assert!(
            message.contains("(backported from commit 1111111222222233333334444444555555566666)")
        );
        assert!(!message.contains("(cherry picked from commit "));
        assert!(rewrite_cherry_pick_trailer_as_backport(dir.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn rewrite_cherry_pick_trailer_as_backport_amends_mixed_case_trailer() {
        // The trailer prefix must match case-insensitively while the commit hash
        // is preserved verbatim.
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(dir.path().join("x.c"), "int x;\n").unwrap();
        run_git_test(dir.path(), &["add", "x.c"]).unwrap();
        run_git_test(
            dir.path(),
            &[
                "commit",
                "-m",
                "subject",
                "-m",
                "(Cherry picked from commit 1111111222222233333334444444555555566666)",
            ],
        )
        .unwrap();

        assert!(rewrite_cherry_pick_trailer_as_backport(dir.path())
            .unwrap()
            .is_some());
        let message = commit_message(dir.path(), "HEAD").unwrap();
        assert!(
            message.contains("(backported from commit 1111111222222233333334444444555555566666)")
        );
        assert!(!message
            .to_ascii_lowercase()
            .contains("(cherry picked from commit "));
    }

    #[test]
    fn leading_whitespace_map_makes_tabs_and_spaces_visible() {
        let map = leading_whitespace_map("\tcpu = foo();\n        ret = -EINVAL;\nplain\n");

        assert!(map.contains("  1: tabs=1 spaces=0 | cpu = foo();"));
        assert!(map.contains("  2: tabs=0 spaces=8 | ret = -EINVAL;"));
        assert!(map.contains("  3: tabs=0 spaces=0 | plain"));
    }

    #[test]
    fn suggestion_payload_carries_whitespace_maps_for_indent() {
        let conflict = ConflictHunk {
            file_path: "kernel/subsystem/example.c".to_string(),
            start_line: 1200,
            end_line: 1210,
            local: "\tcleanup_local_state(obj);\n\tret = -EINVAL;\n".to_string(),
            base: "\tret = -EINVAL;\n".to_string(),
            remote:
                "\ttarget = select_target(obj->effective_mask);\n\tif (unlikely(target < 0)) {\n"
                    .to_string(),
        };

        let payload = suggestion_user_payload(&conflict, "diff --git a/base b/remote\n", "commit");

        assert!(payload.contains("# CODE leading whitespace map"));
        assert!(payload.contains("# Incoming remote leading whitespace map"));
        assert!(
            payload.contains("  1: tabs=1 spaces=0 | target = select_target(obj->effective_mask);")
        );
        assert!(payload.contains("do not emit this map data"));
    }

    #[test]
    fn validation_payload_exposes_dropped_incoming_tab() {
        let conflict = ConflictHunk {
            file_path: "kernel/subsystem/example.c".to_string(),
            start_line: 1200,
            end_line: 1210,
            local: "\tcleanup_local_state(obj);\n\tret = -EINVAL;\n".to_string(),
            base: "\tret = -EINVAL;\n".to_string(),
            remote:
                "\ttarget = select_target(obj->effective_mask);\n\tif (unlikely(target < 0)) {\n"
                    .to_string(),
        };
        let suggestion = Suggestion {
            resolved_code:
                "target = select_target(obj->effective_mask);\n\tif (unlikely(target < 0)) {\n"
                    .to_string(),
            explanation: "applied the incoming target selection".to_string(),
        };

        let payload = validation_user_payload(
            &conflict,
            "diff --git a/base b/remote\n",
            "commit",
            &suggestion,
        );

        assert!(payload.contains("## Incoming remote side"));
        assert!(
            payload.contains("  1: tabs=1 spaces=0 | target = select_target(obj->effective_mask);")
        );
        assert!(payload.contains("## Proposed resolved_code"));
        assert!(
            payload.contains("  1: tabs=0 spaces=0 | target = select_target(obj->effective_mask);")
        );
        assert!(payload.contains("turning a statement that had one leading tab"));
    }

    #[test]
    fn local_whitespace_check_rejects_extra_trailing_blank_lines() {
        let conflict = ConflictHunk {
            file_path: "kernel/subsystem/example.c".to_string(),
            start_line: 1200,
            end_line: 1210,
            local: "\tret = local();\n".to_string(),
            base: "\tret = base();\n".to_string(),
            remote: "\tret = remote();\n".to_string(),
        };
        let suggestion = Suggestion {
            resolved_code: "\tret = remote();\n\n".to_string(),
            explanation: "applied incoming call".to_string(),
        };

        let rejection = local_resolution_whitespace_rejection(&conflict, &suggestion).unwrap();

        assert!(!rejection.accepted);
        assert!(rejection.reason.contains("ends with 1 blank line"));
        assert!(rejection.reason.contains("next unchanged source line"));
        assert!(rejection.concerns[0].contains("end of resolved_code"));
    }

    #[test]
    fn local_whitespace_check_rejects_blank_before_context_label() {
        let conflict = ConflictHunk {
            file_path: "kernel/subsystem/example.c".to_string(),
            start_line: 1200,
            end_line: 1210,
            local: "\tret = old_call();\n\tif (ret) {\n\t\tgoto out_unlock;\n\t}\n".to_string(),
            base: "\tret = old_call();\n\tif (ret) {\n\t\tgoto out_unlock;\n\t}\n".to_string(),
            remote: "\tret = new_call();\n\tif (ret) {\n\t\tgoto out_unlock;\n\t}\n".to_string(),
        };
        let suggestion = Suggestion {
            resolved_code: "\tret = new_call();\n\tif (ret) {\n\t\tgoto out_unlock;\n\t}\n\n"
                .to_string(),
            explanation: "applied incoming call".to_string(),
        };

        let rejection = local_resolution_whitespace_rejection(&conflict, &suggestion).unwrap();

        assert!(!rejection.accepted);
        assert!(rejection.reason.contains("next unchanged source line"));
    }

    #[test]
    fn local_whitespace_check_rejects_new_multiple_blank_run() {
        let conflict = ConflictHunk {
            file_path: "kernel/subsystem/example.c".to_string(),
            start_line: 1200,
            end_line: 1210,
            local: "\tfirst();\n\tsecond();\n".to_string(),
            base: "\tfirst();\n\tsecond();\n".to_string(),
            remote: "\tfirst();\n\tthird();\n".to_string(),
        };
        let suggestion = Suggestion {
            resolved_code: "\tfirst();\n\n\n\tthird();\n".to_string(),
            explanation: "applied incoming call".to_string(),
        };

        let rejection = local_resolution_whitespace_rejection(&conflict, &suggestion).unwrap();

        assert!(!rejection.accepted);
        assert!(rejection.reason.contains("consecutive blank lines"));
    }

    #[test]
    fn local_whitespace_check_allows_preserved_blank_lines() {
        let conflict = ConflictHunk {
            file_path: "kernel/subsystem/example.c".to_string(),
            start_line: 1200,
            end_line: 1210,
            local: "\tfirst();\n\n\tsecond();\n".to_string(),
            base: "\tfirst();\n\n\tsecond();\n".to_string(),
            remote: "\tfirst();\n\n\tthird();\n".to_string(),
        };
        let suggestion = Suggestion {
            resolved_code: "\tfirst();\n\n\tthird();\n".to_string(),
            explanation: "applied incoming call".to_string(),
        };

        assert!(local_resolution_whitespace_rejection(&conflict, &suggestion).is_none());
    }

    #[test]
    fn local_whitespace_check_allows_single_interior_blank_line() {
        let conflict = ConflictHunk {
            file_path: "kernel/subsystem/example.c".to_string(),
            start_line: 1200,
            end_line: 1210,
            local: "\tint ret;\n\tret = local();\n".to_string(),
            base: "\tint ret;\n\tret = base();\n".to_string(),
            remote: "\tint ret;\n\tret = remote();\n".to_string(),
        };
        let suggestion = Suggestion {
            resolved_code: "\tint ret;\n\n\tret = remote();\n".to_string(),
            explanation: "kept declaration separate from code".to_string(),
        };

        assert!(local_resolution_whitespace_rejection(&conflict, &suggestion).is_none());
    }

    #[test]
    fn revision_payload_carries_validator_feedback() {
        let conflict = ConflictHunk {
            file_path: "kernel/foo.c".to_string(),
            start_line: 42,
            end_line: 48,
            local: "local_code();\n".to_string(),
            base: "base_code();\n".to_string(),
            remote: "remote_code();\n".to_string(),
        };
        let previous = Suggestion {
            resolved_code: "bad_resolution();\n".to_string(),
            explanation: "kept the local side".to_string(),
        };
        let validation = Validation {
            accepted: false,
            reason: "missing incoming remote_code call".to_string(),
            concerns: vec!["drops remote_code();".to_string()],
        };

        let payload = revision_user_payload(
            &conflict,
            "diff --git a/base b/remote\n",
            "commit context",
            &previous,
            &validation,
            2,
        );

        assert!(payload.contains("Previous proposed resolved_code"));
        assert!(payload.contains("bad_resolution();"));
        assert!(payload.contains("Validator/local rejection for attempt 1"));
        assert!(payload.contains("missing incoming remote_code call"));
        assert!(payload.contains("- drops remote_code();"));
        assert!(payload.contains("Return only the JSON object."));
    }

    #[test]
    fn lkml_field_wraps_long_text_at_width() {
        let text = "This explanation is intentionally long enough to require wrapping because the human apply output should be usable in LKML-style text without a single over-wide model-generated line.";
        let formatted = format_lkml_field("Explanation", text, 72, false);

        assert!(formatted.starts_with("Explanation:\n"));
        assert!(formatted.lines().count() > 2);
        for line in formatted.lines() {
            assert!(
                line.chars().count() <= 72,
                "line is too wide ({} chars): {line}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn lkml_field_wraps_bullets_with_continuation_indent() {
        let text = "- preserve the target-tree specific branch while applying the incoming state update without dropping either side of the conflict";
        let formatted = format_lkml_field("Validation", text, 72, false);

        assert!(formatted.contains("- preserve the target-tree specific branch"));
        assert!(formatted.contains("\n  "));
        for line in formatted.lines() {
            assert!(
                line.chars().count() <= 72,
                "line is too wide ({} chars): {line}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn apply_action_decoration_plain_matches_review_shape() {
        assert_eq!(
            style_apply_section_title("Agent actions", false),
            " Agent actions "
        );
        assert_eq!(
            style_hunk_header("kernel/foo.c:42", false),
            "kernel/foo.c:42"
        );
        assert_eq!(style_field_label("Explanation", false), "Explanation:");
        assert_eq!(style_field_label("Validation", false), "Validation:");
        assert_eq!(
            style_dimmed(&"-".repeat(APPLY_TEXT_WIDTH), false).len(),
            APPLY_TEXT_WIDTH
        );
        assert_eq!(
            style_dimmed(&"·".repeat(APPLY_TEXT_WIDTH.min(60)), false)
                .chars()
                .count(),
            60
        );
    }

    #[test]
    fn apply_accepted_resolution_rewrites_conflict_block() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        fs::write(
            &file,
            "before\n<<<<<<< HEAD\nlocal\n||||||| base\nbase\n=======\nremote\n>>>>>>> commit\nafter\n",
        )
        .unwrap();

        apply_accepted_resolutions(
            dir.path(),
            &[AcceptedSuggestion {
                file_path: "x.c".to_string(),
                start_line: 2,
                end_line: 8,
                resolved_code: "resolved\n".to_string(),
                explanation: "applied remote intent".to_string(),
                validation_reason: "preserves both sides".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(file).unwrap(),
            "before\nresolved\nafter\n"
        );
    }

    #[test]
    fn apply_accepted_resolution_preserves_newline_when_resolved_code_lacks_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        fs::write(
            &file,
            "before\n<<<<<<< HEAD\nlocal\n||||||| base\nbase\n=======\nremote\n>>>>>>> commit\nafter\n",
        )
        .unwrap();

        apply_accepted_resolutions(
            dir.path(),
            &[AcceptedSuggestion {
                file_path: "x.c".to_string(),
                start_line: 2,
                end_line: 8,
                resolved_code: "resolved".to_string(),
                explanation: "applied remote intent".to_string(),
                validation_reason: "preserves both sides".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(file).unwrap(),
            "before\nresolved\nafter\n"
        );
    }

    #[test]
    fn apply_accepted_resolution_preserves_missing_final_newline_at_eof() {
        // git renders the >>>>>>> marker with a trailing \n even when the
        // conflicting blobs had no newline at EOF, so the marker must not be used
        // to fabricate a final newline. A resolution with no trailing newline for
        // an EOF conflict must leave the file without one.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        fs::write(
            &file,
            "<<<<<<< HEAD\nlocal\n||||||| base\nbase\n=======\nremote\n>>>>>>> commit\n",
        )
        .unwrap();

        apply_accepted_resolutions(
            dir.path(),
            &[AcceptedSuggestion {
                file_path: "x.c".to_string(),
                start_line: 1,
                end_line: 7,
                resolved_code: "resolved".to_string(),
                explanation: "applied remote intent".to_string(),
                validation_reason: "preserves both sides".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(fs::read_to_string(file).unwrap(), "resolved");
    }

    #[test]
    fn apply_accepted_resolution_preserves_crlf_line_endings() {
        // A CRLF file must never acquire a lone \n: the terminator re-added around
        // the resolution has to match the file's \r\n style.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        fs::write(
            &file,
            "before\r\n<<<<<<< HEAD\r\nlocal\r\n||||||| base\r\nbase\r\n=======\r\nremote\r\n>>>>>>> commit\r\nafter\r\n",
        )
        .unwrap();

        apply_accepted_resolutions(
            dir.path(),
            &[AcceptedSuggestion {
                file_path: "x.c".to_string(),
                start_line: 2,
                end_line: 8,
                resolved_code: "resolved".to_string(),
                explanation: "applied remote intent".to_string(),
                validation_reason: "preserves both sides".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(file).unwrap(),
            "before\r\nresolved\r\nafter\r\n"
        );
    }

    #[test]
    fn apply_accepted_resolution_normalizes_multiline_crlf_resolution() {
        // resolved_code arrives \n-delimited from JSON, so every interior line
        // must also be re-terminated to the file's \r\n style, not just the last.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        fs::write(
            &file,
            "before\r\n<<<<<<< HEAD\r\nlocal\r\n||||||| base\r\nbase\r\n=======\r\nremote\r\n>>>>>>> commit\r\nafter\r\n",
        )
        .unwrap();

        apply_accepted_resolutions(
            dir.path(),
            &[AcceptedSuggestion {
                file_path: "x.c".to_string(),
                start_line: 2,
                end_line: 8,
                resolved_code: "line1\nline2".to_string(),
                explanation: "applied remote intent".to_string(),
                validation_reason: "preserves both sides".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(file).unwrap(),
            "before\r\nline1\r\nline2\r\nafter\r\n"
        );
    }

    #[test]
    fn apply_accepted_resolution_derives_crlf_from_surrounding_lines() {
        // EOL style is taken from surrounding unchanged file content, not just the
        // marker. Even with conflict markers rendered using bare \n, a CRLF file
        // (shown by its surrounding \r\n lines) must still get \r\n re-added.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        fs::write(
            &file,
            "before\r\n<<<<<<< HEAD\nlocal\n||||||| base\nbase\n=======\nremote\n>>>>>>> commit\nafter\r\n",
        )
        .unwrap();

        apply_accepted_resolutions(
            dir.path(),
            &[AcceptedSuggestion {
                file_path: "x.c".to_string(),
                start_line: 2,
                end_line: 8,
                resolved_code: "resolved".to_string(),
                explanation: "applied remote intent".to_string(),
                validation_reason: "preserves both sides".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(file).unwrap(),
            "before\r\nresolved\r\nafter\r\n"
        );
    }

    #[test]
    fn apply_accepted_resolution_treats_bare_cr_as_content_at_eof() {
        // A bare trailing \r (no \n) is content, not a line terminator, so an EOF
        // resolution ending in \r must not gain a fabricated final newline.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        fs::write(
            &file,
            "<<<<<<< HEAD\nlocal\n||||||| base\nbase\n=======\nremote\n>>>>>>> commit\n",
        )
        .unwrap();

        apply_accepted_resolutions(
            dir.path(),
            &[AcceptedSuggestion {
                file_path: "x.c".to_string(),
                start_line: 1,
                end_line: 7,
                resolved_code: "foo\r".to_string(),
                explanation: "applied remote intent".to_string(),
                validation_reason: "preserves both sides".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(fs::read_to_string(file).unwrap(), "foo\r");
    }

    #[test]
    fn apply_accepted_resolution_deletes_block_for_empty_resolved_code() {
        // Empty resolved_code means "resolve by deletion": the whole conflict
        // block is removed and the surrounding lines keep their own terminators.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        fs::write(
            &file,
            "before\n<<<<<<< HEAD\nlocal\n||||||| base\nbase\n=======\nremote\n>>>>>>> commit\nafter\n",
        )
        .unwrap();

        apply_accepted_resolutions(
            dir.path(),
            &[AcceptedSuggestion {
                file_path: "x.c".to_string(),
                start_line: 2,
                end_line: 8,
                resolved_code: String::new(),
                explanation: "dropped both sides".to_string(),
                validation_reason: "conflict resolved by deletion".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(fs::read_to_string(file).unwrap(), "before\nafter\n");
    }

    #[test]
    fn parse_post_apply_review_accepts_clean_verdict() {
        let (verdict, explanation) =
            parse_post_apply_review(r#"{"verdict":"clean","explanation":"looks good"}"#).unwrap();
        assert_eq!(verdict, PostApplyVerdict::Clean);
        assert_eq!(explanation, "looks good");
    }

    #[test]
    fn parse_post_apply_review_accepts_amended_verdict() {
        let (verdict, explanation) = parse_post_apply_review(
            r#"{"verdict":"amended","explanation":"fixed kernel/foo.c: stray semicolon"}"#,
        )
        .unwrap();
        assert_eq!(verdict, PostApplyVerdict::Amended);
        assert!(explanation.contains("stray semicolon"));
    }

    #[test]
    fn parse_post_apply_review_accepts_needs_human_verdict() {
        let (verdict, _) = parse_post_apply_review(
            r#"{"verdict":"needs_human","explanation":"unclear API ownership"}"#,
        )
        .unwrap();
        assert_eq!(verdict, PostApplyVerdict::NeedsHuman);
    }

    #[test]
    fn parse_post_apply_review_strips_fences() {
        let raw = "```json\n{\"verdict\":\"clean\",\"explanation\":\"all checks pass\"}\n```\n";
        let (verdict, _) = parse_post_apply_review(raw).unwrap();
        assert_eq!(verdict, PostApplyVerdict::Clean);
    }

    #[test]
    fn parse_post_apply_review_rejects_unknown_verdict() {
        let err =
            parse_post_apply_review(r#"{"verdict":"approved","explanation":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("verdict must be"), "err: {err:#}");
    }

    #[test]
    fn parse_post_apply_review_rejects_missing_explanation() {
        let err = parse_post_apply_review(r#"{"verdict":"clean"}"#).unwrap_err();
        assert!(err.to_string().contains("explanation"), "err: {err:#}");
    }

    #[test]
    fn post_apply_review_user_payload_includes_commit_and_hunks() {
        let accepted = vec![
            AcceptedSuggestion {
                file_path: "kernel/foo.c".to_string(),
                start_line: 42,
                end_line: 50,
                resolved_code: "ignored".to_string(),
                explanation: "merged both sides".to_string(),
                validation_reason: "indentation preserved".to_string(),
            },
            AcceptedSuggestion {
                file_path: "include/bar.h".to_string(),
                start_line: 7,
                end_line: 9,
                resolved_code: "ignored".to_string(),
                explanation: "kept incoming\nrename".to_string(),
                validation_reason: "no behaviour drop".to_string(),
            },
        ];
        let payload =
            post_apply_review_user_payload("abc123", "DIFF", "PREFETCHED-CONTEXT", &accepted);
        assert!(payload.contains("abc123"));
        assert!(payload.contains("DIFF"));
        assert!(payload.contains("PREFETCHED-CONTEXT"));
        assert!(payload.contains("kernel/foo.c:42"));
        assert!(payload.contains("include/bar.h:7"));
        assert!(payload.contains("merged both sides"));
        assert!(payload.contains("indentation preserved"));
        // Newlines inside explanation/validation_reason must be flattened.
        assert!(payload.contains("kept incoming rename"));
        // The full-diff symbol-resolution check must lead, conflict hunks must not be framed as
        // the only place to look.
        assert!(
            payload.contains("STEP 1"),
            "expected STEP 1 framing in payload"
        );
        assert!(
            payload.contains("EVERY `+` line"),
            "expected full-diff scope wording"
        );
        // STEP 1 must come before the conflict-hunk section so the model audits the whole diff
        // before being steered to the conflict locations.
        let step1_idx = payload.find("STEP 1").unwrap();
        let step2_idx = payload.find("STEP 2").unwrap();
        assert!(step1_idx < step2_idx, "STEP 1 should appear before STEP 2");
    }

    #[test]
    fn post_apply_review_user_payload_handles_no_hunks() {
        let payload = post_apply_review_user_payload("deadbee", "DIFF", "", &[]);
        // Clean cherry-pick: conflict-hunk section is empty but the symbol-resolution check
        // is still mandatory on the full diff.
        assert!(payload.contains("(none;"));
        assert!(payload.contains("STEP 1"));
    }

    #[test]
    fn post_apply_verdict_round_trip() {
        for v in [
            PostApplyVerdict::Clean,
            PostApplyVerdict::Amended,
            PostApplyVerdict::NeedsHuman,
        ] {
            assert_eq!(PostApplyVerdict::parse(v.as_str()).unwrap(), v);
        }
    }

    #[test]
    fn parse_added_lines_from_diff_tracks_post_image_lines() {
        let diff = "\
diff --git a/kernel/subsystem/example.c b/kernel/subsystem/example.c
--- a/kernel/subsystem/example.c
+++ b/kernel/subsystem/example.c
@@ -10,0 +11,2 @@
+\tobj->existing_field = 1;
+\tobj->target_field = value;
";

        assert_eq!(
            parse_added_lines_from_diff(diff),
            vec![
                AddedLine {
                    file_path: "kernel/subsystem/example.c".to_string(),
                    line: 11,
                    text: "\tobj->existing_field = 1;".to_string(),
                },
                AddedLine {
                    file_path: "kernel/subsystem/example.c".to_string(),
                    line: 12,
                    text: "\tobj->target_field = value;".to_string(),
                }
            ]
        );
    }

    #[test]
    fn field_access_extraction_ignores_comments_and_strings() {
        assert_eq!(
            extract_field_accesses(
                r#"pr_info("obj->fake"); /* obj->nope */ obj->target_field = 1;"#
            ),
            vec![FieldAccess {
                base: "obj".to_string(),
                field: "target_field".to_string(),
                expression: "obj->target_field".to_string(),
            }]
        );
    }

    #[test]
    fn struct_type_inference_handles_shared_declaration() {
        let content = "\
static int example_attach(void)
{
\tstruct target_type *obj, *old_obj;
\tint value, ret;

\tobj->target_field = value;
}
";

        assert_eq!(
            find_struct_type_for_var(content, 6, "obj"),
            Some("target_type".to_string())
        );
        assert_eq!(
            find_struct_type_for_var(content, 6, "old_obj"),
            Some("target_type".to_string())
        );
    }

    #[test]
    fn struct_body_identifier_check_distinguishes_fields() {
        let content = "\
struct target_type {
\tint existing_field;
\tu64 another_field;
};
";
        let bodies = extract_struct_bodies(content, "target_type");

        assert_eq!(bodies.len(), 1);
        assert!(contains_identifier(&bodies[0], "another_field"));
        assert!(!contains_identifier(&bodies[0], "target_field"));
    }

    #[test]
    fn post_apply_static_check_flags_missing_field_in_head_diff() {
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::create_dir_all(dir.path().join("kernel/subsystem")).unwrap();
        fs::write(
            dir.path().join("kernel/subsystem/example-internal.h"),
            "struct target_type {\n\tint existing_field;\n};\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("kernel/subsystem/example.c"),
            "void f(void)\n{\n\tstruct target_type *obj;\n\tobj->existing_field = 1;\n}\n",
        )
        .unwrap();
        run_git_test(dir.path(), &["add", "kernel/subsystem"]).unwrap();
        run_git_test(dir.path(), &["commit", "-m", "base"]).unwrap();

        fs::write(
            dir.path().join("kernel/subsystem/example.c"),
            "void f(void)\n{\n\tstruct target_type *obj;\n\tobj->existing_field = 1;\n\tobj->target_field = 1;\n}\n",
        )
        .unwrap();
        run_git_test(dir.path(), &["add", "kernel/subsystem/example.c"]).unwrap();
        run_git_test(dir.path(), &["commit", "-m", "add missing field access"]).unwrap();

        let check = post_apply_static_check(dir.path(), &VerboseDest::new(false), None).unwrap();

        assert_eq!(check.status, PostApplyStaticStatus::Failed);
        assert_eq!(check.issues.len(), 1);
        assert_eq!(check.issues[0].file_path, "kernel/subsystem/example.c");
        assert_eq!(check.issues[0].expression, "obj->target_field");
        assert_eq!(check.issues[0].struct_name, "target_type");
        assert_eq!(check.issues[0].field, "target_field");
    }

    #[test]
    fn static_repair_payload_carries_checker_issues() {
        let check = PostApplyStaticCheck {
            status: PostApplyStaticStatus::Failed,
            reason: Some("1 newly-added struct field reference is unresolved".to_string()),
            issues: vec![PostApplyStaticIssue {
                file_path: "kernel/subsystem/example.c".to_string(),
                line: 42,
                expression: "obj->target_field".to_string(),
                struct_name: "target_type".to_string(),
                field: "target_field".to_string(),
                reason: "field is absent\nfrom target tree".to_string(),
            }],
        };

        let payload = post_apply_static_repair_user_payload(
            "abc123",
            "DIFF",
            "PREFETCHED-CONTEXT",
            &check,
            2,
        );

        assert!(payload.contains("Repair attempt: 2/"));
        assert!(payload.contains("kernel/subsystem/example.c:42"));
        assert!(payload.contains("`obj->target_field`"));
        assert!(payload.contains("`struct target_type`"));
        assert!(payload.contains("`target_field`"));
        assert!(payload.contains("field is absent from target tree"));
        assert!(payload.contains("DIFF"));
        assert!(payload.contains("PREFETCHED-CONTEXT"));
    }

    #[test]
    fn merge_post_apply_review_promotes_needs_human_and_dedups_files() {
        let mut review = Some(PostApplyReview {
            verdict: PostApplyVerdict::Amended,
            explanation: "first edit".to_string(),
            modified_files: vec!["kernel/foo.c".to_string()],
            amend_stdout: "amend1".to_string(),
            amend_stderr: String::new(),
        });
        let next = PostApplyReview {
            verdict: PostApplyVerdict::NeedsHuman,
            explanation: "still unresolved".to_string(),
            modified_files: vec!["kernel/foo.c".to_string(), "kernel/bar.c".to_string()],
            amend_stdout: "amend2".to_string(),
            amend_stderr: "warn".to_string(),
        };

        merge_post_apply_review(&mut review, next);

        let review = review.unwrap();
        assert_eq!(review.verdict, PostApplyVerdict::NeedsHuman);
        assert!(review.explanation.contains("first edit"));
        assert!(review.explanation.contains("still unresolved"));
        assert_eq!(
            review.modified_files,
            vec!["kernel/bar.c".to_string(), "kernel/foo.c".to_string()]
        );
        assert!(review.amend_stdout.contains("amend1"));
        assert!(review.amend_stdout.contains("amend2"));
        assert_eq!(review.amend_stderr, "warn");
    }

    #[test]
    fn failed_static_check_marks_review_needs_human() {
        let check = PostApplyStaticCheck {
            status: PostApplyStaticStatus::Failed,
            reason: Some("1 newly-added struct field reference is unresolved".to_string()),
            issues: vec![PostApplyStaticIssue {
                file_path: "kernel/subsystem/example.c".to_string(),
                line: 42,
                expression: "obj->target_field".to_string(),
                struct_name: "target_type".to_string(),
                field: "target_field".to_string(),
                reason: "field is absent".to_string(),
            }],
        };
        let mut review = Some(PostApplyReview {
            verdict: PostApplyVerdict::Clean,
            explanation: "post-review was clean".to_string(),
            modified_files: Vec::new(),
            amend_stdout: String::new(),
            amend_stderr: String::new(),
        });

        merge_static_check_into_review(&mut review, &check);

        let review = review.unwrap();
        assert_eq!(review.verdict, PostApplyVerdict::NeedsHuman);
        assert!(review.explanation.contains("post-review was clean"));
        assert!(review.explanation.contains("obj->target_field"));
        assert!(review.explanation.contains("struct target_type"));
    }

    #[test]
    fn ensure_clean_apply_worktree_rejects_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        run_git_test(dir.path(), &["add", "tracked.txt"]).unwrap();
        run_git_test(dir.path(), &["commit", "-m", "base"]).unwrap();

        fs::write(dir.path().join("scratch.txt"), "user data\n").unwrap();

        let err = ensure_clean_apply_worktree(dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("clean worktree except ignored files"),
            "err: {err:#}"
        );
        assert!(err.to_string().contains("?? scratch.txt"), "err: {err:#}");
    }

    #[test]
    fn ensure_clean_apply_worktree_rejects_tracked_modifications() {
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        run_git_test(dir.path(), &["add", "tracked.txt"]).unwrap();
        run_git_test(dir.path(), &["commit", "-m", "base"]).unwrap();

        fs::write(dir.path().join("tracked.txt"), "base\nlocal edit\n").unwrap();

        let err = ensure_clean_apply_worktree(dir.path()).unwrap_err();
        assert!(err.to_string().contains(" M tracked.txt"), "err: {err:#}");
    }

    #[test]
    fn ensure_clean_apply_worktree_allows_ignored_files() {
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.log\n").unwrap();
        fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        run_git_test(dir.path(), &["add", ".gitignore", "tracked.txt"]).unwrap();
        run_git_test(dir.path(), &["commit", "-m", "base"]).unwrap();

        fs::write(dir.path().join("ignored.log"), "ignore me\n").unwrap();

        ensure_clean_apply_worktree(dir.path()).unwrap();
    }

    #[test]
    fn parse_git_status_porcelain_z_consumes_rename_source_path() {
        let entries = parse_git_status_porcelain_z(
            b"R  new name.txt\0old name.txt\0 M tracked.txt\0?? scratch.txt\0",
        );
        assert_eq!(
            entries,
            vec![
                StatusEntry {
                    code: "R ".to_string(),
                    path: "new name.txt".to_string(),
                },
                StatusEntry {
                    code: " M".to_string(),
                    path: "tracked.txt".to_string(),
                },
                StatusEntry {
                    code: "??".to_string(),
                    path: "scratch.txt".to_string(),
                },
            ]
        );
    }

    #[test]
    fn post_apply_worktree_changes_returns_all_dirty_tracked_paths() {
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        run_git_test(dir.path(), &["add", "a.txt", "b.txt"]).unwrap();
        run_git_test(dir.path(), &["commit", "-m", "base"]).unwrap();

        fs::write(dir.path().join("a.txt"), "a\nedit\n").unwrap();
        fs::write(dir.path().join("b.txt"), "b\nedit\n").unwrap();

        let files = post_apply_worktree_changes(dir.path()).unwrap();
        assert_eq!(files, vec!["a.txt".to_string(), "b.txt".to_string()]);
    }

    #[test]
    #[cfg(unix)]
    fn post_apply_worktree_changes_reports_symlink_target_edits() {
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(dir.path().join("target.txt"), "base\n").unwrap();
        std::os::unix::fs::symlink("target.txt", dir.path().join("link.txt")).unwrap();
        run_git_test(dir.path(), &["add", "target.txt", "link.txt"]).unwrap();
        run_git_test(dir.path(), &["commit", "-m", "base"]).unwrap();

        fs::write(dir.path().join("link.txt"), "base\nvia link\n").unwrap();

        let files = post_apply_worktree_changes(dir.path()).unwrap();
        assert_eq!(files, vec!["target.txt".to_string()]);
    }

    #[test]
    fn post_apply_worktree_changes_rejects_untracked_paths() {
        let dir = tempfile::tempdir().unwrap();
        run_git_test(dir.path(), &["init"]).unwrap();
        run_git_test(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run_git_test(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        run_git_test(dir.path(), &["add", "tracked.txt"]).unwrap();
        run_git_test(dir.path(), &["commit", "-m", "base"]).unwrap();

        fs::write(dir.path().join("scratch.txt"), "not tracked\n").unwrap();

        let err = post_apply_worktree_changes(dir.path()).unwrap_err();
        assert!(err.to_string().contains("untracked file"), "err: {err:#}");
        assert!(err.to_string().contains("scratch.txt"), "err: {err:#}");
    }

    fn run_git_test(repo: &Path, args: &[&str]) -> Result<()> {
        let out = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .with_context(|| format!("git {}", args.join(" ")))?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed\nstdout:\n{}\nstderr:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    #[test]
    fn one_line_collapses_whitespace() {
        assert_eq!(one_line("a\nb\rc\td"), "a b c d");
        // \n between two real spaces is absorbed (last_space already true), but consecutive
        // literal spaces are preserved as-is - the function only flattens line breaks/tabs.
        assert_eq!(one_line("  hello  \n  world  "), "hello    world");
        assert_eq!(one_line("\n\n"), "");
    }
}
