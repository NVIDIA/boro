// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod api;
mod apply;
mod claude_cli;
mod cluster;
mod codex_cli;
mod config;
mod diff_index;
mod git;
mod http;
mod kconfig;
mod lore;
mod model_timeout;
mod opencode;
mod output;
mod prefetch;
mod progress;
mod prompts;
mod snapshot;
mod stages;
mod target;
mod test_boot;
mod test_build;
mod tools;
mod verbose;
mod vng;
mod worktree;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use api::{rough_token_hint, StageUsage, TokenUsage};
use progress::{phase_tag, MultiPatchSpinner, WorkerLineCtx};
use snapshot::{snapshot_to_value, CommitSnapshot, SnapshotPublisher};
use verbose::VerboseDest;

/// CLI surface for `--backend`. Maps to [`config::Backend`] before reaching the rest of the code.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum BackendArg {
    /// OpenAI-compatible chat/completions endpoint.
    Openai,
    /// Shell out to the `claude` CLI.
    Claude,
    /// Shell out to the `opencode` CLI.
    Opencode,
    /// Shell out to the `codex` CLI.
    Codex,
}

impl BackendArg {
    fn to_config(self) -> config::Backend {
        match self {
            BackendArg::Openai => config::Backend::OpenAi,
            BackendArg::Claude => config::Backend::Claude,
            BackendArg::Opencode => config::Backend::Opencode,
            BackendArg::Codex => config::Backend::Codex,
        }
    }
}

/// CLI surface for `--target` (review only). Maps to [`config::ReviewTarget`].
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum TargetArg {
    /// Linux kernel (default).
    #[default]
    Kernel,
    /// QEMU.
    Qemu,
}

impl TargetArg {
    fn to_config(self) -> config::ReviewTarget {
        match self {
            TargetArg::Kernel => config::ReviewTarget::Kernel,
            TargetArg::Qemu => config::ReviewTarget::Qemu,
        }
    }
}

/// CLI surface for `--validation-mode`. Selects whether the global
/// findings-validation stage runs and whether the per-commit LKML
/// pass renders survivors after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ValidationMode {
    /// Skip the validation stage entirely; render raw findings + raw LKML.
    Off,
    /// Validate findings, then render per-commit LKML from survivors only.
    Filter,
    /// Validate findings, do not render the per-commit LKML prose.
    Findings,
}

impl ValidationMode {
    fn label(self) -> &'static str {
        match self {
            ValidationMode::Off => "off",
            ValidationMode::Filter => "filter",
            ValidationMode::Findings => "findings",
        }
    }
}

struct CommitReviewResult {
    findings_val: Value,
    usage_commit: Value,
    usage_steps: Option<Value>,
    phase0_selected_prompts: Option<Vec<String>>,
    validation_context: String,
}

struct UpstreamFollowupResult {
    summary: String,
    master_fixes: Vec<lore::MasterFix>,
}

#[derive(Parser, Debug)]
#[command(name = "boro")]
#[command(version)]
#[command(about = "Linux patch review and testing CLI with a multi-stage agentic workflow")]
struct Cli {
    #[command(flatten)]
    global: GlobalOpts,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, Clone)]
struct GlobalOpts {
    /// Linux git tree to operate on (default is current working directory).
    #[arg(short, long, value_name = "DIR", global = true)]
    source: Option<PathBuf>,

    /// Max concurrent commit workers. Defaults: 8 for `review`, 1 for `build`/`test`.
    #[arg(short = 'j', long, value_name = "N", global = true)]
    max_workers: Option<usize>,

    /// Transport boro uses to talk to the model.
    #[arg(short = 'b', long = "backend", value_enum,
          default_value_t = BackendArg::Openai, global = true)]
    backend: BackendArg,

    /// Print context sizes (review) or planned actions (apply/build/test) and exit.
    #[arg(short = 'd', long, global = true)]
    dry_run: bool,

    /// Stream model responses to stderr while they are generated.
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    /// Emit a single pretty-printed JSON object on stdout (machine-readable);
    /// suppresses the human report. Stderr is unchanged (verbose still works).
    #[arg(long = "json", global = true)]
    json: bool,

    /// Disable Anthropic-style `cache_control` markers on outgoing requests. Caching is on by
    /// default; pass this when running against an endpoint you know rejects the markers and want to
    /// skip the one-time fallback round-trip.
    #[arg(long = "no-prompt-caching", global = true)]
    no_prompt_caching: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// LLM code review of each commit in COMMIT_RANGE (multi-stage agentic pipeline).
    Review(ReviewArgs),
    /// Apply COMMIT_ID with git cherry-pick -x -s; on conflicts, auto-resolve validated hunks.
    Apply(ApplyArgs),
    /// Build each commit with `vng -b`; the model reviews the build log.
    #[command(name = "build")]
    TestBuild(RangeArgs),
    /// Build, then boot under virtme-ng and run a model-picked quick test.
    #[command(name = "test")]
    TestBoot(TestArgs),
}

#[derive(Args, Debug, Clone)]
struct ReviewArgs {
    /// Per-stage model timeout in seconds (default 600).
    #[arg(long, value_name = "SECONDS", default_value_t = 600)]
    timeout: u64,

    /// Skip concerns + consolidation; one review call + LKML pass per commit.
    #[arg(short = 'x', long)]
    fast: bool,

    /// Disable tools execution for even faster/cheaper reviews.
    #[arg(short = 't', long)]
    no_tools: bool,

    /// Codebase being reviewed: selects the prompt corpus and reviewer persona.
    #[arg(short = 'T', long, value_enum, default_value_t = TargetArg::Kernel)]
    target: TargetArg,

    /// Max characters for bundled reference markdown (patch excluded).
    #[arg(short = 'm', long, default_value_t = 200_000)]
    max_context_size: usize,

    /// Output of the final review-validation stage.
    #[arg(long = "validation-mode", value_enum, default_value_t = ValidationMode::Filter)]
    validation_mode: ValidationMode,

    /// Git URI whose selected branch is checked for follow-up fixes.
    #[arg(
        long = "upstream-repo",
        value_name = "URI",
        default_value = lore::UPSTREAM_MASTER_BRANCH_URL
    )]
    upstream: String,

    /// Branch in --upstream-repo checked for follow-up fixes.
    #[arg(long, value_name = "BRANCH", default_value = "master")]
    upstream_branch: String,

    /// Git revision range, e.g. HEAD~4..HEAD. A single commit means COMMIT^..COMMIT.
    #[arg(value_name = "COMMIT_RANGE")]
    range: String,
}

#[derive(Args, Debug, Clone)]
struct ApplyArgs {
    /// Commit to apply with git cherry-pick -x -s
    #[arg(value_name = "COMMIT_ID")]
    commit_id: String,
}

#[derive(Args, Debug, Clone)]
struct RangeArgs {
    /// Git revision range, e.g. HEAD~4..HEAD. A single commit means COMMIT^..COMMIT.
    #[arg(value_name = "COMMIT_RANGE")]
    range: String,
}

#[derive(Args, Debug, Clone)]
struct TestArgs {
    /// Wall-clock budget (seconds) for the in-VM run.
    #[arg(short = 't', long, value_name = "SECONDS", default_value_t = 300)]
    timeout: u64,

    /// Generate a detailed test plan without building or booting.
    #[arg(long)]
    plan: bool,

    /// Kconfig option to test at HEAD, e.g. CONFIG_FOO or CONFIG_NR_CPUS=512.
    #[arg(long, value_name = "CONFIG", conflicts_with = "range")]
    config: Option<String>,

    /// Git revision range, e.g. HEAD~4..HEAD. A single commit means COMMIT^..COMMIT.
    #[arg(value_name = "COMMIT_RANGE", required_unless_present = "config")]
    range: Option<String>,
}

/// Per-commit dispatch carrying the action-specific knobs.
#[derive(Clone)]
enum CommitAction {
    Review {
        target: config::ReviewTarget,
        fast: bool,
        no_tools: bool,
        max_context_size: usize,
        upstream: String,
        upstream_branch: String,
        /// Model config for the per-commit second-opinion call. Carries the resolved
        /// `BORO_VALIDATION_*` config (which falls back to the main model when those
        /// env vars are unset).
        second_opinion: Option<config::ResolvedModel>,
    },
    TestBuild,
    TestBoot {
        /// Wall-clock cap for the in-VM run (seconds). The build itself is unbounded.
        timeout_secs: u64,
        /// Ask the picker what would run, then stop before `vng -b` / `vng -r`.
        plan_only: bool,
        /// What the test picker should target: a commit patch or a CONFIG_ option.
        target: test_boot::TestTarget,
    },
}

impl CommitAction {
    fn label(&self) -> &'static str {
        match self {
            CommitAction::Review { .. } => "review",
            CommitAction::TestBuild => "build",
            CommitAction::TestBoot { .. } => "test",
        }
    }

    /// `--no-tools` only affects the review pipeline; build/test don't use tools.
    fn review_no_tools(&self) -> bool {
        matches!(self, CommitAction::Review { no_tools: true, .. })
    }

    fn is_review(&self) -> bool {
        matches!(self, CommitAction::Review { .. })
    }

    fn uses_commit_metadata(&self) -> bool {
        match self {
            CommitAction::TestBoot { target, .. } => target.uses_commit_metadata(),
            _ => true,
        }
    }
}

#[derive(Default)]
struct RunTotals {
    api_calls: u32,
    prompt: u64,
    completion: u64,
    cache_creation: u64,
    cache_read: u64,
}

impl RunTotals {
    fn add_usage(&mut self, u: TokenUsage) {
        self.api_calls += 1;
        if let Some(p) = u.prompt {
            self.prompt += u64::from(p);
        }
        if let Some(c) = u.completion {
            self.completion += u64::from(c);
        }
        if let Some(cw) = u.cache_creation {
            self.cache_creation += u64::from(cw);
        }
        if let Some(cr) = u.cache_read {
            self.cache_read += u64::from(cr);
        }
    }

    fn merge_from(&mut self, other: RunTotals) {
        self.api_calls += other.api_calls;
        self.prompt += other.prompt;
        self.completion += other.completion;
        self.cache_creation += other.cache_creation;
        self.cache_read += other.cache_read;
    }

    fn json(&self) -> Value {
        json!({
            "api_calls": self.api_calls,
            "prompt_tokens": self.prompt,
            "completion_tokens": self.completion,
            "cache_creation_tokens": self.cache_creation,
            "cache_read_tokens": self.cache_read,
        })
    }
}

fn v(vd: &VerboseDest, msg: impl std::fmt::Display) {
    vd.line(msg);
}

/// `[PATCH n/N]` prefix for stderr spinner lines (SHA is shown separately).
fn patch_series_tag(one_based: usize, total: usize) -> String {
    format!("[PATCH {one_based}/{total}]")
}

fn short_sha10(sha: &str) -> String {
    sha.chars().take(10).collect()
}

/// Prefix for every verbose / log line while reviewing one commit in a series.
fn verbose_worker_line_prefix(one_based: usize, total: usize, sha: &str) -> String {
    format!("[PATCH {one_based}/{total}] {}]", short_sha10(sha))
}

fn stage_progress_line(
    patch_tag: &str,
    sha_short: &str,
    step: u32,
    total: u32,
    description: &str,
) -> String {
    format!("{patch_tag} {sha_short} [step {step}/{total}] {description}")
}

/// One commit: git + reference bundle + single-pass or full multi-pass review.
/// Per-commit consolidation (concerns -> findings) and LKML generation run inside this task.
///
/// `repo` is the main repo root (used for SHA-based `git show`/`diff-tree` lookups, which work
/// the same from any path inside the repo). `effective_repo` is the working directory that tools
/// and subprocess backends see - the per-commit worktree when one is in use, otherwise the same
/// as `repo`.
#[allow(clippy::too_many_arguments)]
async fn commit_review_inner(
    idx: usize,
    sha: &str,
    num_commits: usize,
    range: &str,
    repo: &Path,
    effective_repo: &Path,
    client: &reqwest::Client,
    model: &config::ResolvedModel,
    target: config::ReviewTarget,
    fast: bool,
    no_tools: bool,
    max_context_size: usize,
    second_opinion: Option<&config::ResolvedModel>,
    vd: &VerboseDest,
    dry_run: bool,
    totals: &mut RunTotals,
    worker_ctx: Option<&WorkerLineCtx>,
    publisher: &SnapshotPublisher,
    master_repo: Option<&lore::MasterRepo>,
) -> Result<Value> {
    v(vd, format!("running git show {sha} ..."));
    let patch = git::show_patch(repo, sha)?;
    v(vd, format!("patch text: {} characters", patch.len()));

    v(vd, "listing changed paths (git diff-tree) ...");
    let changed = git::changed_paths(repo, sha)?;
    v(
        vd,
        format!("{} file(s) touched: {}", changed.len(), changed.join(", ")),
    );

    v(
        vd,
        format!(
            "building reference bundle (max {} chars) ...",
            max_context_size
        ),
    );
    let reference =
        prompts::build_reference_context(target, &changed, max_context_size, None, None)?;
    let ref_hint =
        rough_token_hint(crate::target::reviewer_system_prompt(target).len() + reference.len());
    let patch_hint = rough_token_hint(patch.len());
    v(
        vd,
        format!(
            "reference text: {} chars (~{} tokens est. for ref+system without instructions suffix)",
            reference.len(),
            ref_hint
        ),
    );
    v(
        vd,
        format!(
            "(rough ~tokens for patch section alone: ~{}, full request larger due to JSON instructions)",
            patch_hint
        ),
    );
    if dry_run {
        if !vd.stderr {
            eprintln!(
                "commit {sha}: reference_chars={} patch_chars={}",
                reference.len(),
                patch.len()
            );
        }
        return Ok(json!({
            "sha": sha,
            "dry_run": true,
            "reference_chars": reference.len(),
            "patch_chars": patch.len(),
        }));
    }

    let commit_headers = git::show_commit_headers(repo, sha)?;
    let patch_diff = git::show_patch_diff_only(repo, sha)?;

    let series_for_consolidation = if fast {
        String::new()
    } else {
        series_context_for_consolidation(repo, range, idx, num_commits)
    };
    v(
        vd,
        format!(
            "series context for consolidation (pass 2): {} characters",
            series_for_consolidation.len()
        ),
    );

    let patch_tag = patch_series_tag(idx + 1, num_commits);
    let sha_short = short_sha10(sha);
    let tool_cfg = (!no_tools).then(|| api::ToolLoopConfig::new(effective_repo));
    let review = if fast {
        let prefetch_block = prefetch_context_block(effective_repo, &patch_diff, vd).await;
        let reference_with_prefetch = if prefetch_block.is_empty() {
            reference.clone()
        } else {
            format!("{reference}{prefetch_block}")
        };
        run_single_pass(
            client,
            model,
            target,
            &reference_with_prefetch,
            &prefetch_block,
            &commit_headers,
            &patch_diff,
            vd,
            &patch_tag,
            &sha_short,
            tool_cfg.as_ref(),
            totals,
            worker_ctx,
            publisher,
            effective_repo,
            second_opinion,
        )
        .await?
    } else {
        run_two_pass(
            client,
            model,
            target,
            repo,
            sha,
            &patch_tag,
            &sha_short,
            &patch,
            &commit_headers,
            &patch_diff,
            &changed,
            max_context_size,
            max_context_size / 2,
            &series_for_consolidation,
            vd,
            tool_cfg.as_ref(),
            totals,
            worker_ctx,
            publisher,
            effective_repo,
            second_opinion,
            master_repo,
        )
        .await?
    };

    let mut findings_val = review.findings_val.clone();
    let repaired = cleanup_repair_and_validate_findings(
        &mut findings_val,
        &commit_headers,
        &patch_diff,
        &changed,
        vd,
    );
    if repaired.relocated > 0 || repaired.dropped > 0 {
        v(
            vd,
            format!(
                "final source repair: relocated {} patch-text finding(s), dropped {} ambiguous finding(s)",
                repaired.relocated, repaired.dropped
            ),
        );
    }
    let mut commit_obj = json!({
        "sha": sha,
        "findings": findings_val.get("findings").cloned().unwrap_or(json!([])),
        "usage": review.usage_commit,
    });
    if !review.validation_context.is_empty() {
        // Internal hand-off to the global validation stage. Removed before output.
        commit_obj["_validation_context"] = json!(review.validation_context);
    }
    if let Some(st) = &review.usage_steps {
        commit_obj["usage_steps"] = st.clone();
    }
    if let Some(p0) = &review.phase0_selected_prompts {
        if !p0.is_empty() {
            commit_obj["phase0_selected_prompts"] = json!(p0);
        }
    }
    Ok(commit_obj)
}

#[allow(clippy::too_many_arguments)]
async fn execute_commit_task(
    idx: usize,
    sha: String,
    num_commits: usize,
    range: String,
    repo: PathBuf,
    use_worktree: bool,
    client: reqwest::Client,
    model: config::ResolvedModel,
    action: CommitAction,
    vd: VerboseDest,
    dry_run: bool,
    worker_line: Option<WorkerLineCtx>,
    publisher: SnapshotPublisher,
    master_repo: Option<Arc<lore::MasterRepo>>,
) -> (Value, RunTotals) {
    let mut totals = RunTotals::default();
    let task_start = Instant::now();
    let sha_ref = sha.as_str();
    v(
        &vd,
        format!(
            "--- commit {}/{} ({}): {sha_ref} ---",
            idx + 1,
            num_commits,
            action.label()
        ),
    );

    // Pre-stage worker-row banner: anything before the first chat_completion (worktree
    // create, reference bundle build, etc.) leaves the row idle otherwise. Any later
    // set_line_message - from the first stage label - overwrites this naturally.
    let patch_tag = patch_series_tag(idx + 1, num_commits);
    let sha_short = short_sha10(sha_ref);
    if let Some(w) = worker_line.as_ref() {
        w.set_line_message(format!(
            "{patch_tag} {sha_short} {}",
            phase_tag("initializing worktree...")
        ));
    }

    // Per-commit metadata + diff captured up front against the main repo so every
    // subcommand (review/build/test) gets identical fields in the per-commit JSON
    // and any partial-run Ctrl-C dump carries them too. Best-effort: a failure here
    // is recorded into the snapshot but does not abort the commit task — the inner
    // subcommand handler may still produce useful output.
    let (commit_meta, patch_diff_full, changed_paths_full) = if action.uses_commit_metadata() {
        let commit_meta = match git::commit_metadata(repo.as_path(), sha_ref) {
            Ok(m) => Some(m),
            Err(e) => {
                v(
                    &vd,
                    format!("commit {sha_ref}: git metadata lookup failed: {e:#}"),
                );
                None
            }
        };
        let patch_diff_full = match git::show_patch_diff_only(repo.as_path(), sha_ref) {
            Ok(p) => Some(p),
            Err(e) => {
                v(&vd, format!("commit {sha_ref}: patch fetch failed: {e:#}"));
                None
            }
        };
        let changed_paths_full = match git::changed_paths(repo.as_path(), sha_ref) {
            Ok(c) => Some(c),
            Err(e) => {
                v(
                    &vd,
                    format!("commit {sha_ref}: changed_paths failed: {e:#}"),
                );
                None
            }
        };
        (commit_meta, patch_diff_full, changed_paths_full)
    } else {
        v(
            &vd,
            "config test target: skipping commit patch metadata injection",
        );
        (None, None, None)
    };
    if let (Some(m), Some(p), Some(c)) = (
        commit_meta.as_ref(),
        patch_diff_full.as_ref(),
        changed_paths_full.as_ref(),
    ) {
        publisher.set_metadata(m.clone(), p.clone(), c.clone());
    }

    // RAII: dropped at end of this function (including on tokio task abort).
    let worktree = if use_worktree {
        match worktree::Worktree::create(repo.as_path(), action.label(), sha_ref, &vd) {
            Ok(w) => Some(w),
            Err(e) => {
                let msg = format!("worktree setup failed: {e:#}");
                publisher.set_error(msg.clone());
                eprintln!("[boro] commit {sha_ref}: {msg}");
                if let Some(w) = worker_line {
                    w.finish_commit_line();
                }
                let mut val = inject_commit_metadata(
                    json!({
                        "sha": sha_ref,
                        "findings": [],
                        "error": msg,
                    }),
                    commit_meta.as_ref(),
                    patch_diff_full.as_deref(),
                    changed_paths_full.as_deref(),
                );
                val["wall_ms"] = json!(task_start.elapsed().as_millis() as u64);
                return (val, totals);
            }
        }
    } else {
        None
    };
    let effective_repo: PathBuf = worktree
        .as_ref()
        .map(|w| w.path().to_path_buf())
        .unwrap_or_else(|| repo.clone());

    let commit_obj_result = match &action {
        CommitAction::Review {
            target,
            fast,
            no_tools,
            max_context_size,
            second_opinion,
            ..
        } => {
            commit_review_inner(
                idx,
                sha_ref,
                num_commits,
                range.as_str(),
                repo.as_path(),
                effective_repo.as_path(),
                &client,
                &model,
                *target,
                *fast,
                *no_tools,
                *max_context_size,
                second_opinion.as_ref(),
                &vd,
                dry_run,
                &mut totals,
                worker_line.as_ref(),
                &publisher,
                master_repo.as_deref(),
            )
            .await
        }
        CommitAction::TestBuild => {
            test_build::commit_test_build(
                sha_ref,
                effective_repo.as_path(),
                &client,
                &model,
                &vd,
                dry_run,
                worker_line.as_ref(),
                &publisher,
            )
            .await
        }
        CommitAction::TestBoot {
            timeout_secs,
            plan_only,
            target,
        } => {
            test_boot::commit_test_boot(
                sha_ref,
                effective_repo.as_path(),
                &client,
                &model,
                target,
                &vd,
                dry_run,
                *plan_only,
                Duration::from_secs(*timeout_secs),
                worker_line.as_ref(),
                &publisher,
            )
            .await
        }
    };

    let val = match commit_obj_result {
        Ok(obj) => {
            // Roll the per-commit task usage (recorded in stages/publisher) into RunTotals.
            // For review, the existing pipeline already updates `totals` via &mut. For
            // build/test we rely on the per-commit `usage` field.
            if let Some(u) = obj.get("usage") {
                let p = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                let c = u
                    .get("completion_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let cw = u
                    .get("cache_creation_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let cr = u
                    .get("cache_read_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let calls = u.get("api_calls").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                if matches!(
                    action,
                    CommitAction::TestBuild | CommitAction::TestBoot { .. }
                ) {
                    totals.api_calls += calls;
                    totals.prompt += p;
                    totals.completion += c;
                    totals.cache_creation += cw;
                    totals.cache_read += cr;
                }
            }
            publisher.mark_complete();
            obj
        }
        Err(e) => {
            let msg = format!("{:#}", e);
            publisher.set_error(msg.clone());
            eprintln!(
                "[boro] commit {} {} failed (continuing): {:#}",
                sha_ref,
                action.label(),
                e
            );
            v(
                &vd,
                format!("commit {sha_ref}: recording error and continuing with remaining commits"),
            );
            json!({
                "sha": sha_ref,
                "findings": [],
                "error": msg,
            })
        }
    };

    let mut val = inject_commit_metadata(
        val,
        commit_meta.as_ref(),
        patch_diff_full.as_deref(),
        changed_paths_full.as_deref(),
    );
    val["wall_ms"] = json!(task_start.elapsed().as_millis() as u64);

    if let Some(w) = worker_line {
        w.finish_commit_line();
    }

    (val, totals)
}

/// Drop any `location` entry on a finding whose `file` is not in the patch's changed
/// paths. Run once at the commit level after consolidation so it covers fast-mode,
/// two-pass-mode, and merged second-opinion findings uniformly. The finding itself is
/// preserved (the prose still has review signal); only the suspect anchor is dropped.
fn drop_hallucinated_locations(findings_val: &mut Value, changed: &[String], vd: &VerboseDest) {
    let Some(arr) = findings_val
        .get_mut("findings")
        .and_then(|f| f.as_array_mut())
    else {
        return;
    };
    let changed_set: std::collections::HashSet<&str> = changed.iter().map(|s| s.as_str()).collect();
    for f in arr.iter_mut() {
        let Some(obj) = f.as_object_mut() else {
            continue;
        };
        let loc_file = obj
            .get("location")
            .and_then(|l| l.get("file"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        if let Some(file) = loc_file {
            if !changed_set.contains(file.as_str()) {
                v(
                    vd,
                    format!(
                        "dropping location for finding (file {file:?} not in commit's changed paths)"
                    ),
                );
                obj.remove("location");
            }
        }
    }
}

/// Drop `location` (or just `line_end`) entries that don't land on a real hunk line in
/// the patch under review. The model sometimes writes prose about call site A but anchors
/// at the line of call site B, or cites a line that's outside any hunk for the given
/// side - both produce misleading inline placements in the JSON viewer. The finding text
/// is preserved; only the bad anchor is removed (and the viewer falls back to rendering it
/// as a commit-level comment).
///
/// Cheaper-but-strictly-narrower than [`drop_hallucinated_locations`] (which only checks
/// the file name): run this **after** that one so the cheap file-level check still logs
/// its own message when the file is wrong, and this pass focuses on the line/side mismatch.
fn drop_unanchored_locations(
    findings_val: &mut Value,
    idx: &diff_index::DiffIndex,
    vd: &VerboseDest,
) {
    let Some(arr) = findings_val
        .get_mut("findings")
        .and_then(|f| f.as_array_mut())
    else {
        return;
    };
    for f in arr.iter_mut() {
        let Some(obj) = f.as_object_mut() else {
            continue;
        };
        let Some(loc) = obj.get("location").cloned() else {
            continue;
        };
        let file = loc.get("file").and_then(|x| x.as_str()).unwrap_or("");
        let line = loc.get("line").and_then(|x| x.as_u64()).unwrap_or(0);
        let side =
            diff_index::Side::from_str(loc.get("side").and_then(|x| x.as_str()).unwrap_or("RIGHT"));
        if file.is_empty() || line == 0 {
            continue;
        }
        if !idx.contains(file, line, side) {
            v(
                vd,
                format!(
                    "dropping location for finding (file {file:?} line {line} side {side:?} not on any hunk line)"
                ),
            );
            obj.remove("location");
            continue;
        }
        // line anchors. Check line_end if present; drop just line_end if it doesn't anchor.
        if let Some(end) = loc.get("line_end").and_then(|x| x.as_u64()) {
            if end > line && !idx.contains(file, end, side) {
                v(
                    vd,
                    format!(
                        "dropping line_end for finding (file {file:?} line_end {end} side {side:?} not on any hunk line)"
                    ),
                );
                if let Some(loc_obj) = obj.get_mut("location").and_then(|l| l.as_object_mut()) {
                    loc_obj.remove("line_end");
                }
            }
        }

        let identifiers = obj
            .get("problem")
            .and_then(Value::as_str)
            .map(backticked_c_identifiers)
            .unwrap_or_default();
        if !identifiers.is_empty() {
            let end = obj
                .get("location")
                .and_then(|l| l.get("line_end"))
                .and_then(Value::as_u64)
                .unwrap_or(line);
            if !idx.range_contains_identifier(file, line, end, side, &identifiers) {
                v(
                    vd,
                    format!(
                        "dropping semantically mismatched location for finding (anchor does not contain any named identifier: {})",
                        identifiers.join(", ")
                    ),
                );
                obj.remove("location");
            }
        }
    }
}

/// Validate model-authored anchors before using their presence to decide whether deterministic
/// source repair is needed. Repair may create a new anchor, so validate once more afterward.
fn cleanup_repair_and_validate_findings(
    findings_val: &mut Value,
    commit_message: &str,
    patch: &str,
    changed: &[String],
    vd: &VerboseDest,
) -> api::MessageConcernRepair {
    drop_hallucinated_locations(findings_val, changed, vd);
    let diff_idx = diff_index::DiffIndex::from_unified_diff(patch);
    drop_unanchored_locations(findings_val, &diff_idx, vd);

    let repaired = api::repair_misattributed_message_findings(findings_val, commit_message, patch);

    // `added_line_matches()` should only create real RIGHT-side hunk anchors. Keep this final
    // check as a postcondition so a future repair strategy cannot leak an invalid location.
    drop_unanchored_locations(findings_val, &diff_idx, vd);
    repaired
}

fn backticked_c_identifiers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        let token = rest[..end].trim().trim_end_matches("()");
        if !token.is_empty()
            && token.chars().enumerate().all(|(i, c)| {
                c == '_' || c.is_ascii_alphanumeric() && (i > 0 || !c.is_ascii_digit())
            })
            && !out.iter().any(|existing| existing == token)
        {
            out.push(token.to_string());
        }
        rest = &rest[end + 1..];
    }
    out
}

/// Merge per-commit metadata (subject/author/date/parents, raw diff, changed paths) onto the
/// commit's result `Value`. Inserted by [`execute_commit_task`] so every subcommand
/// (review/build/test) and every error path (worktree failure, inner error) carries the same
/// shape - the `--json` consumer can rely on these fields appearing whenever they could be
/// fetched. Values already present on `obj` win, so subcommand-specific fields are preserved.
fn inject_commit_metadata(
    mut obj: Value,
    meta: Option<&git::CommitMeta>,
    patch: Option<&str>,
    changed: Option<&[String]>,
) -> Value {
    let m = match obj.as_object_mut() {
        Some(m) => m,
        None => return obj,
    };
    if let Some(meta) = meta {
        m.entry("subject").or_insert_with(|| json!(meta.subject));
        m.entry("author").or_insert_with(|| json!(meta.author));
        m.entry("date").or_insert_with(|| json!(meta.date));
        m.entry("parents").or_insert_with(|| json!(meta.parents));
    }
    if let Some(p) = patch {
        m.entry("patch").or_insert_with(|| json!(p));
    }
    if let Some(c) = changed {
        m.entry("changed_paths").or_insert_with(|| json!(c));
    }
    obj
}

/// Emit a one-line `validation: mode=... model=... base_url=...` verbose
/// message before kicking off the validation call. Shared between LKML and
/// findings paths so the log shape stays identical.
fn log_validation_header(
    vdest: &VerboseDest,
    main_model: &config::ResolvedModel,
    validation_cfg: &config::ResolvedModel,
    mode: ValidationMode,
) {
    if config::validation_differs(main_model, validation_cfg) {
        v(
            vdest,
            format!(
                "validation: mode={} model={} base_url={} (differs from main)",
                mode.label(),
                validation_cfg.model_id,
                if validation_cfg.base_url.is_empty() {
                    "(n/a)"
                } else {
                    validation_cfg.base_url.as_str()
                }
            ),
        );
    } else {
        v(
            vdest,
            format!(
                "validation: mode={} (reusing main model + endpoint)",
                mode.label()
            ),
        );
    }
}

fn usage_step_json(
    step: impl Into<String>,
    usage: TokenUsage,
    wall_ms: u64,
    error: Option<String>,
) -> Value {
    json!({
        "step": step.into(),
        "prompt_tokens": usage.prompt,
        "completion_tokens": usage.completion,
        "cache_creation_tokens": usage.cache_creation,
        "cache_read_tokens": usage.cache_read,
        "api_calls": 1,
        "wall_ms": wall_ms,
        "error": error,
    })
}

fn usage_step_json_from_totals(
    step: impl Into<String>,
    totals: &RunTotals,
    wall_ms: u64,
    error: Option<String>,
) -> Value {
    json!({
        "step": step.into(),
        "prompt_tokens": totals.prompt,
        "completion_tokens": totals.completion,
        "cache_creation_tokens": totals.cache_creation,
        "cache_read_tokens": totals.cache_read,
        "api_calls": totals.api_calls,
        "wall_ms": wall_ms,
        "error": error,
    })
}

fn validation_usage_json_from_steps(model_id: &str, steps: Vec<Value>) -> Value {
    let mut usage = usage_summary_json_from_step_values(&steps);
    let wall_ms: u64 = steps
        .iter()
        .map(|s| s.get("wall_ms").and_then(|v| v.as_u64()).unwrap_or(0))
        .sum();
    usage["wall_ms"] = json!(wall_ms);
    usage["model"] = json!(model_id);
    usage["usage_steps"] = Value::Array(steps);
    usage
}

fn append_validation_usage_step(out: &mut Value, model_id: &str, step: Value) {
    let mut steps: Vec<Value> = out
        .get("validation_usage")
        .and_then(|v| v.get("usage_steps"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    steps.push(step);
    out["validation_usage"] = validation_usage_json_from_steps(model_id, steps);
}

fn without_unverified_sensitive_findings(findings: &Value) -> (Vec<Value>, usize) {
    let raw = findings.as_array().cloned().unwrap_or_default();
    let before = raw.len();
    let retained: Vec<Value> = raw
        .into_iter()
        .filter(|finding| !api::finding_requires_repository_verification(finding))
        .collect();
    let withheld = before.saturating_sub(retained.len());
    (retained, withheld)
}

/// Findings-mode validation: takes per-commit `findings[]` JSON and produces
/// per-commit `out["commits"][i]["validated_findings"]`. Skips commits with
/// empty findings; commits absent from the validator's response keep their
/// raw findings.
#[allow(clippy::too_many_arguments)]
async fn run_findings_validation(
    client: &reqwest::Client,
    validation_cfg: &config::ResolvedModel,
    main_model: &config::ResolvedModel,
    mode: ValidationMode,
    out: &mut Value,
    totals: &mut RunTotals,
    vdest: &VerboseDest,
    repo: &Path,
    no_tools: bool,
    progress_ui: Option<&MultiPatchSpinner>,
) {
    // Snapshot the commits we need to send. We only validate commits that
    // have at least one finding; commits with empty findings are passed
    // through unchanged.
    let mut payload_owned: Vec<(String, String, String, String, String, Value)> = Vec::new();
    if let Some(commits) = out["commits"].as_array() {
        for c in commits {
            let findings = match c.get("findings").and_then(|f| f.as_array()) {
                Some(arr) if !arr.is_empty() => arr,
                _ => continue,
            };
            let sha_full = c.get("sha").and_then(|s| s.as_str()).unwrap_or("");
            if sha_full.is_empty() {
                continue;
            }
            let sha12 = sha_full.get(..12).unwrap_or(sha_full).to_string();
            let subject = c
                .get("subject")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let diff = c
                .get("patch")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let commit_message = git::show_commit_headers(repo, sha_full).unwrap_or_default();
            let reference_context = c
                .get("_validation_context")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            payload_owned.push((
                sha12,
                subject,
                commit_message,
                reference_context,
                diff,
                Value::Array(findings.clone()),
            ));
        }
    }
    if payload_owned.is_empty() {
        v(vdest, "validation skipped (no findings to validate)");
        return;
    }
    let refs_for = |indices: &[usize]| -> Vec<api::ValidationFindingsCommit<'_>> {
        indices
            .iter()
            .map(|&idx| {
                let (sha, subject, commit_message, reference_context, diff, findings) =
                    &payload_owned[idx];
                api::ValidationFindingsCommit {
                    sha: sha.as_str(),
                    subject: subject.as_str(),
                    commit_message: commit_message.as_str(),
                    reference_context: reference_context.as_str(),
                    diff: diff.as_str(),
                    findings,
                }
            })
            .collect()
    };
    let user_budget = api::validation_findings_user_budget(validation_cfg.max_input_tokens);
    let all_indices: Vec<usize> = (0..payload_owned.len()).collect();
    let all_payload_refs = refs_for(&all_indices);
    let batches = api::validation_findings_batches(&all_payload_refs, user_budget);

    log_validation_header(vdest, main_model, validation_cfg, mode);
    v(
        vdest,
        format!(
            "validation: {} commit(s) split into {} structured batch(es), user-message budget {} bytes",
            payload_owned.len(),
            batches.len(),
            user_budget
        ),
    );
    let mut stage_tot = api::CumulativeTokenUsage::default();
    let tool_cfg = (!no_tools).then(|| {
        api::ToolLoopConfig::new(repo).requiring(api::ToolVerification::SensitiveFindings)
    });
    let mut by_sha: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut verification_failed_shas: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut successful_batches = 0usize;
    for (batch_idx, indices) in batches.iter().enumerate() {
        let batch_num = batch_idx + 1;
        let payload_refs = refs_for(indices);
        let user_msg =
            match api::validation_findings_user_payload_bounded(&payload_refs, user_budget) {
                Ok(user_msg) => user_msg,
                Err(e) => {
                    let error = format!("{e:#}");
                    v(
                        vdest,
                        format!(
                            "validation batch {batch_num}/{} skipped: {error}",
                            batches.len()
                        ),
                    );
                    append_validation_usage_step(
                        out,
                        &validation_cfg.model_id,
                        usage_step_json(
                            format!("findings-{batch_num}/{}", batches.len()),
                            TokenUsage::default(),
                            0,
                            Some(error),
                        ),
                    );
                    continue;
                }
            };
        let label = format!(
            "[validation:{}] Validating findings batch {batch_num}/{}",
            mode.label(),
            batches.len()
        );
        let progress_line = progress_ui.map(|ui| ui.stage_ctx(label.clone()));
        let t_val = Instant::now();
        let (parsed_opt, _last_raw, summed, last_err, _attempts) =
            api::chat_completion_with_retry_stage_timeout_preserve_input(
                client,
                validation_cfg,
                api::SYSTEM_REVIEW_VALIDATION_FINDINGS,
                &user_msg,
                validation_cfg.temperature,
                Some(&label),
                Some(&mut stage_tot),
                vdest,
                tool_cfg.as_ref(),
                progress_line.as_ref(),
                repo,
                api::parse_validation_findings,
                api::RETRY_REMINDER_FINDINGS_VALIDATION,
                api::STAGE_RETRY_MAX_ATTEMPTS,
            )
            .await;
        if let Some(w) = progress_line.as_ref() {
            w.finish_commit_line();
        }
        totals.add_usage(summed);
        let wall_ms = t_val.elapsed().as_millis() as u64;
        let mandatory_verification_failed = last_err
            .as_ref()
            .is_some_and(api::is_required_tool_verification_error);
        let validation_error = last_err.as_ref().map(api::short_error_reason);
        append_validation_usage_step(
            out,
            &validation_cfg.model_id,
            usage_step_json(
                format!("findings-{batch_num}/{}", batches.len()),
                summed,
                wall_ms,
                validation_error,
            ),
        );
        let Some(parsed) = parsed_opt else {
            if mandatory_verification_failed {
                for &idx in indices {
                    verification_failed_shas.insert(payload_owned[idx].0.clone());
                }
            }
            let msg = last_err
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "no response".to_string());
            v(
                vdest,
                if mandatory_verification_failed {
                    format!(
                        "validation batch {batch_num}/{} failed mandatory repository verification (withholding sensitive raw findings): {msg}",
                        batches.len()
                    )
                } else {
                    format!(
                        "validation batch {batch_num}/{} failed (continuing with raw findings for that batch): {msg}",
                        batches.len()
                    )
                },
            );
            continue;
        };
        successful_batches += 1;
        v(
            vdest,
            format!(
                "validation batch {batch_num}/{} done in {:.1?} ({} prompt + {} completion tokens)",
                batches.len(),
                t_val.elapsed(),
                summed.prompt.unwrap_or(0),
                summed.completion.unwrap_or(0)
            ),
        );
        if let Some(arr) = parsed.get("commits").and_then(|c| c.as_array()) {
            for entry in arr {
                let Some(sha) = entry.get("sha").and_then(|s| s.as_str()) else {
                    continue;
                };
                let findings = entry
                    .get("findings")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                by_sha.insert(sha.to_string(), findings);
            }
        }
    }
    if successful_batches == 0 && verification_failed_shas.is_empty() {
        return;
    }

    if let Some(commits) = out["commits"].as_array_mut() {
        for c in commits.iter_mut() {
            let sha_full = c
                .get("sha")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if sha_full.is_empty() {
                continue;
            }
            let sha12 = sha_full.get(..12).unwrap_or(sha_full.as_str());
            let validated = by_sha
                .get(sha12)
                .or_else(|| by_sha.get(sha_full.as_str()))
                .cloned();
            if let Some(findings) = validated {
                // Defence-in-depth: even though the prompt says preserve `location`
                // byte-for-byte (which means anchors we cleaned upstream stay clean),
                // re-verify against the commit's own diff in case the validator
                // emits anything new.
                let mut wrapped = json!({ "findings": findings });
                let patch = c.get("patch").and_then(|s| s.as_str()).unwrap_or("");
                if !patch.is_empty() {
                    let idx = diff_index::DiffIndex::from_unified_diff(patch);
                    drop_unanchored_locations(&mut wrapped, &idx, vdest);
                }
                c["validated_findings"] = wrapped
                    .get_mut("findings")
                    .map(|f| f.take())
                    .unwrap_or(Value::Array(Vec::new()));
            } else if verification_failed_shas.contains(sha12) {
                let (retained, withheld) = without_unverified_sensitive_findings(
                    c.get("findings").unwrap_or(&Value::Null),
                );
                v(
                    vdest,
                    format!(
                        "validation commit {sha12}: withheld {} sensitive unverified finding(s), retained {} unrelated finding(s)",
                        withheld,
                        retained.len()
                    ),
                );
                c["validated_findings"] = Value::Array(retained);
            }
        }
    }

    out["validation_mode"] = json!(mode.label());
}

/// Phase 3 of the review pipeline: render per-commit LKML prose from the
/// survivors of findings validation. Skipped entirely when
/// `--validation-mode=findings` (structured findings replace the narrative
/// section) or `--dry-run`/Ctrl-C. For each commit:
///   - prefer `validated_findings[]` when present (filter mode), else fall
///     back to `findings[]` (off mode or validation failed);
///   - if the array is empty, set `lkml_report = "No issues found."`
///     without an LLM call;
///   - else call `api::chat_completion` with the target LKML prompt to render prose
///     from the chosen finding set;
///   - record the LKML render under validation-model usage and add the
///     tokens to run-wide `totals`.
///
/// Tasks run in parallel under a fresh `Semaphore` capped at `max_workers`,
/// matching the per-commit worker pool's concurrency. Each task builds its
/// own `ToolLoopConfig` (gated by `no_tools`) so the LKML model can grep /
/// read source while writing prose - earlier this stage was tool-less,
/// which caused the model to ask source-grounded questions in prose
/// instead of answering them.
///
/// The `model` is the BORO_VALIDATION_* resolution (with fallback to
/// BORO_MODEL) so the LKML render uses the same model that just decided
/// keep/drop/tighten on the structured findings. When the validation
/// model is distinct from the main model (e.g. a stronger validator), the
/// prose is rendered by that stronger model.
#[allow(clippy::too_many_arguments)]
async fn render_commit_lkml_phase(
    client: &reqwest::Client,
    model: &config::ResolvedModel,
    target: config::ReviewTarget,
    out: &mut Value,
    totals: &mut RunTotals,
    vdest: &VerboseDest,
    repo: &Path,
    max_workers: usize,
    max_extras: usize,
    no_tools: bool,
    progress_ui: Option<Arc<MultiPatchSpinner>>,
) {
    let template = match prompts::load_inline_template(target, max_extras.saturating_mul(4)) {
        Ok(Some(t)) => t,
        _ => api::LKML_FALLBACK_TEMPLATE.to_string(),
    };
    let template = Arc::new(template);
    let sem = Arc::new(Semaphore::new(max_workers.max(1)));
    let mut join_set: JoinSet<(usize, String, Option<String>, Option<StageUsage>)> = JoinSet::new();
    let phase_start = Instant::now();

    let commits_count = out["commits"].as_array().map(|a| a.len()).unwrap_or(0);
    for idx in 0..commits_count {
        let c = &out["commits"][idx];
        let sha = c
            .get("sha")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if sha.is_empty() {
            continue;
        }
        // Prefer validated_findings (set by run_findings_validation in filter
        // mode); fall back to raw findings (off mode or validator failed).
        let chosen = c
            .get("validated_findings")
            .cloned()
            .or_else(|| c.get("findings").cloned())
            .unwrap_or_else(|| json!([]));
        let arr_empty = chosen.as_array().is_some_and(|a| a.is_empty());
        let patch = c
            .get("patch")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();

        let client = client.clone();
        let model = model.clone();
        let template = template.clone();
        let sem = sem.clone();
        let vd = vdest.clone();
        let repo = repo.to_path_buf();
        let sha_for_task = sha.clone();
        let progress_ui = progress_ui.clone();

        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore closed");
            if arr_empty {
                return (
                    idx,
                    sha_for_task,
                    Some("No issues found.".to_string()),
                    None,
                );
            }
            let commit_headers =
                git::show_commit_headers(repo.as_path(), &sha_for_task).unwrap_or_default();
            let patch_capped = api::cap_utf8(&patch, 120_000);
            let findings_val = json!({ "findings": chosen });
            let user = api::lkml_report_user_payload(
                template.as_str(),
                &findings_val,
                &commit_headers,
                &patch_capped,
            );
            let label = format!("[lkml] {}", &sha_for_task[..sha_for_task.len().min(12)]);
            let progress_line = progress_ui.as_ref().map(|ui| ui.stage_ctx(label.clone()));
            let mut stage_tot = api::CumulativeTokenUsage::default();
            let t_lkml = Instant::now();
            let tool_cfg = (!no_tools).then(|| api::ToolLoopConfig::new(repo.as_path()));
            let (body, usage, error) = match api::chat_completion_stage_timeout(
                &client,
                &model,
                crate::target::lkml_system_prompt(target),
                &user,
                model.temperature,
                Some(&label),
                Some(&mut stage_tot),
                &vd,
                tool_cfg.as_ref(),
                progress_line.as_ref(),
                repo.as_path(),
            )
            .await
            {
                Ok((raw, u)) => (Some(api::strip_json_fences(&raw)), u, None),
                Err(e) => {
                    v(&vd, format!("LKML render failed for {sha_for_task}: {e:#}"));
                    (
                        None,
                        TokenUsage::default(),
                        Some(api::short_error_reason(&e)),
                    )
                }
            };
            if let Some(w) = progress_line.as_ref() {
                w.finish_commit_line();
            }
            let stage = StageUsage {
                step: "lkml",
                usage,
                wall: t_lkml.elapsed(),
                error,
            };
            (idx, sha_for_task, body, Some(stage))
        });
    }

    let mut lkml_totals = RunTotals::default();
    let mut lkml_errors = 0u64;
    while let Some(res) = join_set.join_next().await {
        let Ok((idx, _sha, body, stage)) = res else {
            continue;
        };
        if let Some(text) = body {
            out["commits"][idx]["lkml_report"] = json!(text);
        }
        if let Some(stage) = stage {
            if stage.error.is_some() {
                lkml_errors += 1;
            }
            totals.add_usage(stage.usage);
            lkml_totals.add_usage(stage.usage);
        }
    }

    if lkml_totals.api_calls > 0 {
        let error = (lkml_errors > 0).then(|| {
            format!(
                "{} LKML render call(s) failed; see per-commit LKML text",
                lkml_errors
            )
        });
        append_validation_usage_step(
            out,
            &model.model_id,
            usage_step_json_from_totals(
                if lkml_totals.api_calls == 1 {
                    "lkml".to_string()
                } else {
                    format!("lkml x{}", lkml_totals.api_calls)
                },
                &lkml_totals,
                phase_start.elapsed().as_millis() as u64,
                error,
            ),
        );
    }
}

fn resolve_quick_summary_highlights(
    response: &api::QuickSummaryResponse,
    commits_data: &[(String, String, Value)],
) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut resolved = Vec::new();

    for highlight in &response.highlights {
        if resolved.len() == api::QUICK_SUMMARY_MAX_HIGHLIGHTS {
            break;
        }
        if seen.contains(&highlight.finding_ref) {
            continue;
        }

        let authoritative = commits_data.iter().find_map(|(sha, _, findings)| {
            findings
                .as_array()?
                .iter()
                .enumerate()
                .find_map(|(index, finding)| {
                    let finding_ref = format!("{sha}:{index}");
                    (highlight.finding_ref == finding_ref).then_some((
                        finding_ref,
                        sha.as_str(),
                        finding,
                    ))
                })
        });
        let Some((finding_ref, sha, finding)) = authoritative else {
            continue;
        };
        let Some(severity @ ("Critical" | "High" | "Medium" | "Low")) =
            finding.get("severity").and_then(Value::as_str)
        else {
            continue;
        };

        seen.insert(finding_ref.clone());
        let mut resolved_highlight = json!({
            "finding_ref": finding_ref,
            "commit": sha,
            "severity": severity,
            "title": highlight.title,
            "question": highlight.question,
        });
        if let Some(location) = finding.get("location") {
            resolved_highlight["location"] = location.clone();
        }
        resolved.push(resolved_highlight);
    }

    resolved
}

/// Run-wide quick summary. Counts severities locally over the chosen findings array
/// (validated_findings when present, raw findings otherwise) for every non-dry-run commit, then
/// asks the validation model for a 1-3 sentence prose summary that highlights the most important
/// issues. The AI text and the counts are stashed under `out["summary"]` so both the human report
/// and `--json` consumers can render them. The stage is bounded: one extra chat call per run,
/// always on for the `review` subcommand regardless of `--validation-mode` (filter, off,
/// findings - even the no-LKML `findings` mode benefits from a one-line TL;DR).
///
/// Skipped via the caller when `--dry-run`, Ctrl-C, or non-review subcommand.
#[allow(clippy::too_many_arguments)]
async fn run_quick_summary(
    client: &reqwest::Client,
    cfg: &config::ResolvedModel,
    main_model: &config::ResolvedModel,
    target: config::ReviewTarget,
    out: &mut Value,
    totals: &mut RunTotals,
    vdest: &VerboseDest,
    repo: &Path,
    progress_ui: Option<&MultiPatchSpinner>,
) {
    // Gather per-commit (full sha, subject, chosen findings) tuples. Skip dry-run rows and rows
    // with no sha (defensive - real commits always have one).
    let mut commits_data: Vec<(String, String, Value)> = Vec::new();
    if let Some(commits) = out["commits"].as_array() {
        for c in commits {
            if c.get("dry_run").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            let sha_full = c.get("sha").and_then(|s| s.as_str()).unwrap_or("");
            if sha_full.is_empty() {
                continue;
            }
            let subject = c
                .get("subject")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let findings_val = c
                .get("validated_findings")
                .cloned()
                .or_else(|| c.get("findings").cloned())
                .unwrap_or_else(|| json!([]));
            commits_data.push((sha_full.to_string(), subject, findings_val));
        }
    }

    // Count severities across all commits before deciding whether to call the model. The count
    // line always renders, even on empty / no-findings runs; counts use the canonical labels.
    let mut critical = 0u64;
    let mut high = 0u64;
    let mut medium = 0u64;
    let mut low = 0u64;
    let mut total_findings = 0u64;
    for (_, _, findings_val) in &commits_data {
        if let Some(arr) = findings_val.as_array() {
            for f in arr {
                let sev = f.get("severity").and_then(|v| v.as_str()).unwrap_or("");
                match sev {
                    "Critical" => critical += 1,
                    "High" => high += 1,
                    "Medium" => medium += 1,
                    "Low" => low += 1,
                    _ => {}
                }
                total_findings += 1;
            }
        }
    }

    let (summary_text, summary_highlights) = if commits_data.is_empty() {
        ("No commits reviewed.".to_string(), Vec::new())
    } else if total_findings == 0 {
        (
            "No issues found across the reviewed commits.".to_string(),
            Vec::new(),
        )
    } else {
        // Build refs for payload from owned tuples.
        let payload_refs: Vec<api::QuickSummaryCommit<'_>> = commits_data
            .iter()
            .map(|(sha, subject, findings)| api::QuickSummaryCommit {
                sha: sha.as_str(),
                subject: subject.as_str(),
                findings,
            })
            .collect();
        let user_msg = api::quick_summary_user_payload(&payload_refs);
        if config::validation_differs(main_model, cfg) {
            v(
                vdest,
                format!(
                    "quick-summary: model={} base_url={} (differs from main)",
                    cfg.model_id,
                    if cfg.base_url.is_empty() {
                        "(n/a)"
                    } else {
                        cfg.base_url.as_str()
                    }
                ),
            );
        } else {
            v(vdest, "quick-summary: reusing main model + endpoint");
        }
        let label = "[summary] Quick summary".to_string();
        let progress_line = progress_ui.map(|ui| ui.stage_ctx(label.clone()));
        let mut stage_tot = api::CumulativeTokenUsage::default();
        let t = Instant::now();
        // Tools are off: the model has every finding in the payload; nothing to look up.
        let (response, usage, error) = match api::chat_completion_stage_timeout(
            client,
            cfg,
            crate::target::quick_summary_system_prompt(target),
            &user_msg,
            cfg.temperature,
            Some(&label),
            Some(&mut stage_tot),
            vdest,
            None,
            progress_line.as_ref(),
            repo,
        )
        .await
        {
            Ok((raw, u)) => match api::parse_quick_summary_response(&raw) {
                Ok(response) => (Some(response), u, None),
                Err(e) => {
                    let reason = api::short_error_reason(&e);
                    v(
                        vdest,
                        format!(
                            "quick-summary response parse failed (using deterministic fallback): {e:#}"
                        ),
                    );
                    (
                        None,
                        u,
                        Some(format!("invalid quick-summary response: {reason}")),
                    )
                }
            },
            Err(e) => {
                let reason = api::short_error_reason(&e);
                v(
                    vdest,
                    format!("quick-summary failed (continuing without text): {e:#}"),
                );
                (None, TokenUsage::default(), Some(reason))
            }
        };
        let wall_ms = t.elapsed().as_millis() as u64;
        if let Some(w) = progress_line.as_ref() {
            w.finish_commit_line();
        }
        totals.add_usage(usage);
        append_validation_usage_step(
            out,
            &cfg.model_id,
            usage_step_json("summary", usage, wall_ms, error),
        );
        v(
            vdest,
            format!(
                "quick-summary done in {:.1?} ({} prompt + {} completion tokens)",
                Duration::from_millis(wall_ms),
                usage.prompt.unwrap_or(0),
                usage.completion.unwrap_or(0)
            ),
        );
        match response {
            Some(response) => {
                let highlights = resolve_quick_summary_highlights(&response, &commits_data);
                (response.text, highlights)
            }
            None => (
                format!(
                    "Reviewed {} commit(s); {} finding(s) recorded.",
                    commits_data.len(),
                    total_findings
                ),
                Vec::new(),
            ),
        }
    };

    out["summary"] = json!({
        "text": summary_text,
        "counts": {
            "Critical": critical,
            "High": high,
            "Medium": medium,
            "Low": low,
        },
        "highlights": summary_highlights,
    });
}

async fn run_apply_command(
    global: &GlobalOpts,
    args: &ApplyArgs,
    run_start: Instant,
) -> Result<()> {
    let vdest = VerboseDest::new(global.verbose);
    v(
        &vdest,
        format!("starting (subcommand=apply, commit={:?})", args.commit_id),
    );

    let repo = match global.source.clone() {
        Some(p) => p,
        None => std::env::current_dir().context("default source: current working directory")?,
    };
    v(
        &vdest,
        format!("working directory / source hint: {}", repo.display()),
    );

    let repo = git::repo_root(&repo).context("resolve git root")?;
    v(&vdest, format!("git repository root: {}", repo.display()));

    std::env::set_current_dir(&repo)
        .with_context(|| format!("set cwd to repository root {}", repo.display()))?;
    v(
        &vdest,
        format!("process working directory: {}", repo.display()),
    );

    let backend = global.backend.to_config();
    let mut model = if global.dry_run {
        config::dry_run_placeholder()
    } else {
        config::resolve_model_from_env(backend)?
    };
    model.prompt_cache = !global.no_prompt_caching;

    let validation = if global.dry_run {
        model.clone()
    } else {
        config::resolve_validation_from_env(&model)
            .context("resolve validation config from BORO_VALIDATION_* env")?
    };

    v(
        &vdest,
        format!(
            "resolved model (env): backend={} base_url={} model_id={}",
            backend.as_str(),
            if model.base_url.is_empty() {
                "(n/a)"
            } else {
                model.base_url.as_str()
            },
            if model.model_id.is_empty() {
                "(backend default)"
            } else {
                model.model_id.as_str()
            },
        ),
    );
    if config::validation_differs(&model, &validation) {
        v(
            &vdest,
            format!(
                "validation: model={} base_url={} (differs from main)",
                validation.model_id,
                if validation.base_url.is_empty() {
                    "(n/a)"
                } else {
                    validation.base_url.as_str()
                }
            ),
        );
    }

    let apply_subject = git::commit_subject(&repo, &args.commit_id)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Build a single-row progress UI so each chat call in the 3-stage apply workflow updates one
    // live line and a shared `prompt:N tokens:M` footer. The dry-run path skips the UI entirely.
    let apply_ui = if global.dry_run {
        None
    } else {
        progress::MultiPatchSpinner::try_new(1)
    };
    let apply_worker = apply_ui.as_ref().map(|ui| ui.worker_ctx(0));

    let outcome_result = if global.dry_run {
        Ok(apply::dry_run(&args.commit_id))
    } else {
        let client = http::build_http_client().context("HTTP client")?;
        apply::run(apply::ApplyRequest {
            repo: repo.as_path(),
            commit_id: &args.commit_id,
            client: &client,
            model: &model,
            validation_model: &validation,
            verbose: &vdest,
            worker_line: apply_worker.clone(),
        })
        .await
    };

    if let Some(worker) = &apply_worker {
        worker.finish_commit_line();
    }
    if let Some(ui) = &apply_ui {
        ui.finish_footer_clear();
    }

    let mut outcome = outcome_result?;
    if outcome.commit_subject.is_none() {
        outcome.commit_subject = apply_subject;
    }
    if matches!(outcome.status, apply::ApplyStatus::DryRun) {
        outcome.wall_ms = run_start.elapsed().as_millis() as u64;
    }

    let outcome_json = outcome.json(&model, &validation);
    if global.json {
        output::print_report_json(&outcome_json);
    } else {
        output::eprint_apply_stats(&outcome_json);
        outcome.print_human();
    }
    let _ = std::io::stdout().flush();

    // Final usage footer line: same shape as the live footer, printed once after the report
    // so it lands at a stable position on stderr regardless of whether the live UI was used.
    eprintln!(
        "{}",
        progress::usage_footer_line(
            outcome.usage.prompt,
            outcome.usage.completion,
            outcome.usage.cache_creation,
            outcome.usage.cache_read,
        )
    );

    let code = outcome.exit_code();
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let run_start = Instant::now();
    let cli = Cli::parse();

    if let Command::Apply(args) = &cli.command {
        return run_apply_command(&cli.global, args, run_start).await;
    }

    if let Command::Review(args) = &cli.command {
        model_timeout::set_review_stage_timeout(Duration::from_secs(args.timeout));
    }

    // Resolve subcommand into CommitAction + range + per-subcommand defaults.
    // CommitAction::Review's `second_opinion` is set later once validation_cfg
    // has been resolved (we need the main model to fall back to).
    let (mut action, range, default_workers) = match &cli.command {
        Command::Review(args) => (
            CommitAction::Review {
                target: args.target.to_config(),
                fast: args.fast,
                no_tools: args.no_tools,
                max_context_size: args.max_context_size,
                upstream: args.upstream.clone(),
                upstream_branch: args.upstream_branch.clone(),
                second_opinion: None,
            },
            args.range.clone(),
            8usize,
        ),
        Command::Apply(_) => unreachable!("apply handled before range pipeline"),
        Command::TestBuild(args) => (CommitAction::TestBuild, args.range.clone(), 1usize),
        Command::TestBoot(args) => {
            let (target, selector) = if let Some(config) = &args.config {
                (
                    test_boot::TestTarget::from_config_arg(config)?,
                    config.clone(),
                )
            } else {
                (
                    test_boot::TestTarget::Commit,
                    args.range
                        .clone()
                        .expect("clap requires COMMIT_RANGE unless --config is present"),
                )
            };
            (
                CommitAction::TestBoot {
                    timeout_secs: args.timeout,
                    plan_only: args.plan,
                    target,
                },
                selector,
                1usize,
            )
        }
    };
    let range = match &action {
        CommitAction::TestBoot { target, .. } => target.display_name(&range),
        _ => git::normalize_commit_range_arg(&range),
    };

    // Validation is review-only and on by default; capture the opt-out flag.
    let validation_mode = match &cli.command {
        Command::Review(a) => a.validation_mode,
        _ => ValidationMode::Off,
    };
    let validation_disabled = validation_mode == ValidationMode::Off;

    // Review target (kernel default). Only meaningful for `review`; used for the
    // verbose prompt-source line, the global LKML/summary phases, and the
    // source-tree mismatch warning.
    let review_target = match &action {
        CommitAction::Review { target, .. } => *target,
        _ => config::ReviewTarget::default(),
    };

    let vdest = VerboseDest::new(cli.global.verbose);

    v(
        &vdest,
        format!(
            "starting (subcommand={}, range={:?})",
            action.label(),
            range
        ),
    );

    // Fail fast if the user asked for a vng-driven mode but vng isn't installed.
    if matches!(
        action,
        CommitAction::TestBuild
            | CommitAction::TestBoot {
                plan_only: false,
                ..
            }
    ) {
        vng::ensure_vng_available().context("build/test require `vng` (virtme-ng)")?;
    }

    let repo = match cli.global.source.clone() {
        Some(p) => p,
        None => std::env::current_dir().context("default source: current working directory")?,
    };
    v(
        &vdest,
        format!("working directory / source hint: {}", repo.display()),
    );

    let repo = git::repo_root(&repo).context("resolve git root")?;
    v(&vdest, format!("git repository root: {}", repo.display()));

    std::env::set_current_dir(&repo)
        .with_context(|| format!("set cwd to repository root {}", repo.display()))?;
    v(
        &vdest,
        format!("process working directory: {}", repo.display()),
    );

    // Warn (don't fail) if the source tree looks like a different codebase than
    // --target selects — a quietly-wrong review is the easy mistake to make.
    // Heuristic: only warn when the tree is confidently classified.
    if action.is_review() {
        match config::detect_tree_kind(&repo) {
            Some(detected) if detected != review_target => {
                eprintln!(
                    "[boro] warning: --target {} was given, but {} looks like a {} tree. \
The review will use {} prompts and persona and may be inaccurate — did you mean --target {}?",
                    review_target.as_str(),
                    repo.display(),
                    detected.as_str(),
                    review_target.as_str(),
                    detected.as_str(),
                );
            }
            Some(detected) => v(
                &vdest,
                format!(
                    "tree-kind check: source matches --target {}",
                    detected.as_str()
                ),
            ),
            None => v(
                &vdest,
                "tree-kind check: source tree not confidently classified (no warning)",
            ),
        }
    }

    let backend = cli.global.backend.to_config();
    let model = {
        let mut m = if cli.global.dry_run {
            config::dry_run_placeholder()
        } else {
            config::resolve_model_from_env(backend)?
        };
        m.prompt_cache = !cli.global.no_prompt_caching;
        m
    };

    // Worktrees pin each parallel commit task to its own working tree.
    //   - review: required for source-context prefetch, which reads files after the
    //     target commit is checked out; skip only for --dry-run.
    //   - build / test: required (we cd in to build/boot the kernel) - but skip on
    //     --dry-run since we don't actually invoke vng.
    let use_worktree = match &action {
        CommitAction::Review { .. } => !cli.global.dry_run,
        CommitAction::TestBuild => !cli.global.dry_run,
        CommitAction::TestBoot { plan_only, .. } => !cli.global.dry_run && !plan_only,
    };
    if use_worktree {
        if let Err(e) = worktree::sweep_stale(&repo, action.label(), &vdest) {
            v(
                &vdest,
                format!("worktree: startup sweep failed (continuing): {e:#}"),
            );
        }
    }

    // Resolve validation/second-opinion config once at startup. Both stages
    // (global validation, per-commit second-opinion) draw from the same
    // BORO_VALIDATION_* env vars (with main-model fallback). Validating env
    // vars early gives a fast failure for bad input, and lets the header
    // preview the validation model when it differs from the main one. The
    // global validation stage can still be opted out via --validation-mode=off;
    // the per-commit second-opinion call is always on for review runs.
    let validation_cfg = if action.is_review() {
        Some(
            config::resolve_validation_from_env(&model)
                .context("resolve validation config from BORO_VALIDATION_* env")?,
        )
    } else {
        None
    };
    // Inject the second-opinion config into the action so commit tasks can use it.
    if let CommitAction::Review {
        ref mut second_opinion,
        ..
    } = action
    {
        *second_opinion = validation_cfg.clone();
    }
    // The header preview still tracks the global-validation gate (validation
    // is the stage most users notice in the header).
    let validation_header_model = validation_cfg
        .as_ref()
        .filter(|_| !validation_disabled)
        .filter(|r| r.model_id != model.model_id)
        .map(|r| r.model_id.as_str());
    output::eprint_run_header(&range, &model.model_id, validation_header_model);

    v(
        &vdest,
        format!(
            "resolved model (env): backend={} base_url={} model_id={}",
            backend.as_str(),
            if model.base_url.is_empty() {
                "(n/a)"
            } else {
                model.base_url.as_str()
            },
            if model.model_id.is_empty() {
                "(backend default)"
            } else {
                model.model_id.as_str()
            },
        ),
    );

    v(
        &vdest,
        format!(
            "prompts: {}",
            crate::target::prompts_source_verbose(review_target)
        ),
    );
    v(
        &vdest,
        format!(
            "repo tools: {}",
            match (&action, backend) {
                (CommitAction::TestBuild, _) | (CommitAction::TestBoot { .. }, _) =>
                    "n/a (build/test do not call repo tools)",
                (CommitAction::Review { .. }, config::Backend::Claude) =>
                    "delegated to claude CLI (boro tool sandbox bypassed)",
                (CommitAction::Review { .. }, config::Backend::Opencode) =>
                    "delegated to opencode CLI (boro tool sandbox bypassed)",
                (CommitAction::Review { .. }, config::Backend::Codex) =>
                    "delegated to codex CLI (boro tool sandbox bypassed)",
                (CommitAction::Review { .. }, config::Backend::OpenAi) if action.review_no_tools() =>
                    "disabled (--no-tools)",
                (CommitAction::Review { .. }, config::Backend::OpenAi) =>
                    "enabled (grep_repo, read_files, read_symbol, git history/diff/show, run_git, rg - review, specialists, validation, LKML; not phase 0 or consolidation)",
            }
        ),
    );

    let client = http::build_http_client().context("HTTP client")?;
    v(
        &vdest,
        "HTTP: reqwest (HTTP/1.1 only, connect_timeout=300s, timeout=3600s, tcp_keepalive=60s, pool_idle=45s, POST retries up to 5 on transient errors)",
    );

    let mut shas = match &action {
        CommitAction::TestBoot { target, .. } if target.is_config() => {
            vec![git::rev_parse_commit(&repo, "HEAD").context("resolve HEAD for CONFIG_ test")?]
        }
        _ => git::rev_list(&repo, &range).with_context(|| format!("invalid range {:?}", range))?,
    };
    if shas.is_empty() {
        anyhow::bail!("no commits in range {:?}", range);
    }

    if let CommitAction::TestBoot {
        plan_only: true,
        target,
        ..
    } = &mut action
    {
        if matches!(target, test_boot::TestTarget::Commit) && shas.len() > 1 {
            let commits = shas.clone();
            v(
                &vdest,
                format!(
                    "test plan: collapsing {} commits into one range-wide plan",
                    commits.len()
                ),
            );
            *target = test_boot::TestTarget::CommitRange {
                range: range.clone(),
                commits,
            };
            shas = vec!["range".to_string()];
        }
    }

    // Fetch the selected upstream branch once into FETCH_HEAD. Workers only run
    // read-only `git log` queries against that one-run snapshot. The last SHA
    // in the review range is the reviewed branch/commit tip; upstream fixes
    // already reachable there are not reportable follow-ups.
    let review_tip = shas.last().map(String::as_str);
    let master_repo = if action.is_review()
        && review_target == config::ReviewTarget::Kernel
        && !cli.global.dry_run
    {
        let (upstream, upstream_branch) = match &action {
            CommitAction::Review {
                upstream,
                upstream_branch,
                ..
            } => (upstream, upstream_branch),
            _ => unreachable!("only review uses the upstream branch lookup"),
        };
        v(
            &vdest,
            format!("upstream-followup: fetching {upstream} {upstream_branch} into FETCH_HEAD"),
        );
        match lore::prepare_master_fetch(&repo, upstream, upstream_branch, review_tip).await {
            Ok(master) => Some(Arc::new(master)),
            Err(e) => {
                v(
                    &vdest,
                    format!("upstream-followup: upstream branch fetch failed (continuing): {e:#}"),
                );
                None
            }
        }
    } else {
        None
    };

    v(
        &vdest,
        format!(
            "revision list: {} commit(s) - {}",
            shas.len(),
            shas.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );

    let workers = cli.global.max_workers.unwrap_or(default_workers).max(1);
    v(
        &vdest,
        format!(
            "commit task concurrency: {} ({} concurrent slot(s) for {} commit(s))",
            workers,
            workers.min(shas.len()),
            shas.len()
        ),
    );

    // One MultiProgress row per commit when work is actually parallel. Single-commit runs use the
    // one-line SpinnerGuard path; a two-line MultiProgress footer is prone to leaving stale footer
    // redraws in scrollback when later validation/LKML/summary rows are inserted and removed.
    let patch_ui: Option<Arc<MultiPatchSpinner>> = {
        let n = shas.len();
        let use_patch_rows = n > 1 && workers > 1;
        if use_patch_rows {
            MultiPatchSpinner::try_new(n).map(Arc::new)
        } else {
            None
        }
    };

    let num_commits = shas.len();
    let sem = Arc::new(Semaphore::new(workers));
    let mut join_set = JoinSet::new();
    let mut snapshots: Vec<Arc<Mutex<CommitSnapshot>>> = Vec::with_capacity(num_commits);

    for (idx, sha) in shas.iter().enumerate() {
        let sha = sha.clone();
        let sem = Arc::clone(&sem);
        let repo = repo.clone();
        let client = client.clone();
        let model = model.clone();
        let range = range.clone();
        let action = action.clone();
        let master_repo = master_repo.clone();
        let vd = vdest.with_prefix(verbose_worker_line_prefix(
            idx + 1,
            num_commits,
            sha.as_str(),
        ));
        let dry_run = cli.global.dry_run;
        let worker_line = patch_ui.as_ref().map(|ui| ui.worker_ctx(idx));
        let (snap_handle, publisher) = SnapshotPublisher::new(sha.as_str());
        snapshots.push(snap_handle);

        join_set.spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore should not be closed");
            let (val, run_totals) = execute_commit_task(
                idx,
                sha,
                num_commits,
                range,
                repo,
                use_worktree,
                client,
                model,
                action,
                vd,
                dry_run,
                worker_line,
                publisher,
                master_repo,
            )
            .await;
            (idx, val, run_totals)
        });
    }

    let mut triples: Vec<(usize, Value, RunTotals)> = Vec::with_capacity(shas.len());
    let mut cancelled = false;
    loop {
        tokio::select! {
            biased;
            sig = tokio::signal::ctrl_c(), if !cancelled => {
                if let Err(e) = sig {
                    v(&vdest, format!("ctrl_c handler install failed: {e:#}"));
                    continue;
                }
                eprintln!("\n[boro] Ctrl-C received - aborting in-flight stages and dumping partial state...");
                cancelled = true;
                join_set.abort_all();
            }
            joined = join_set.join_next() => {
                match joined {
                    Some(Ok((idx, val, local))) => triples.push((idx, val, local)),
                    Some(Err(e)) if e.is_cancelled() => {
                        // Worker was aborted by Ctrl-C; partial state is in `snapshots[idx]`.
                    }
                    Some(Err(e)) => {
                        if cancelled {
                            v(&vdest, format!("commit worker join error after cancel: {e:#}"));
                        } else {
                            return Err(anyhow::Error::from(e))
                                .context("commit task worker panicked");
                        }
                    }
                    None => break,
                }
            }
        }
    }

    if cancelled {
        let have: std::collections::HashSet<usize> = triples.iter().map(|(i, _, _)| *i).collect();
        for (i, snap) in snapshots.iter().enumerate() {
            if have.contains(&i) {
                continue;
            }
            let g = snap.lock().expect("snapshot mutex poisoned");
            let val = snapshot_to_value(&g);
            let (calls, prompt, completion, cache_creation, cache_read) =
                snapshot::snapshot_run_totals(&g);
            let local = RunTotals {
                api_calls: calls,
                prompt,
                completion,
                cache_creation,
                cache_read,
            };
            triples.push((i, val, local));
        }
    }
    triples.sort_by_key(|(i, _, _)| *i);

    let mut totals = RunTotals::default();
    let mut commit_values: Vec<Value> = Vec::with_capacity(triples.len());
    for (_, val, local) in triples {
        totals.merge_from(local);
        commit_values.push(val);
    }

    // Top-level JSON shape consumed by output::print_report_json and downstream
    // tooling. Bump `schema_version` on field rename / removal at the top, commit,
    // or finding layer; adding new optional fields does not require a bump.
    let mut out = json!({
        "schema_version": 1,
        "range": range,
        "subcommand": action.label(),
        "model": model.model_id,
        "verbose": cli.global.verbose,
    });
    out["commits"] = Value::Array(commit_values);
    if cancelled {
        out["cancelled"] = json!(true);
    }

    // Global findings-validation stage. Runs once across all per-commit
    // findings and stashes `validated_findings[]` on each commit.
    // Skipped under --dry-run, --validation-mode=off, Ctrl-C, or on
    // non-review subcommands.
    if let Some(validation_cfg) = validation_cfg
        .as_ref()
        .filter(|_| !cli.global.dry_run && !cancelled && !validation_disabled)
    {
        // Findings validation now runs for both filter and findings modes.
        // The post-validation LKML phase below renders per-commit prose
        // from the survivors for filter mode.
        run_findings_validation(
            &client,
            validation_cfg,
            &model,
            validation_mode,
            &mut out,
            &mut totals,
            &vdest,
            repo.as_path(),
            action.review_no_tools(),
            patch_ui.as_deref(),
        )
        .await;
    }

    // The prefetched source block is an internal validation hand-off, not report data.
    if let Some(commits) = out["commits"].as_array_mut() {
        for commit in commits {
            if let Some(obj) = commit.as_object_mut() {
                obj.remove("_validation_context");
            }
        }
    }

    // Phase 3: render per-commit LKML from validated_findings (or raw
    // findings when validation was off / failed). Skipped in findings mode
    // (structured findings replace the narrative) and on dry-run / Ctrl-C.
    //
    // Uses the BORO_VALIDATION_* model (with fallback to BORO_MODEL when
    // those env vars are unset), since the LKML pass is now a post-
    // validation step - same model that decided keep/drop renders the
    // surviving prose.
    if !cli.global.dry_run
        && !cancelled
        && action.is_review()
        && validation_mode != ValidationMode::Findings
    {
        let max_extras = match &action {
            CommitAction::Review {
                max_context_size, ..
            } => max_context_size / 2,
            _ => 0,
        };
        let lkml_model = validation_cfg.as_ref().unwrap_or(&model);
        render_commit_lkml_phase(
            &client,
            lkml_model,
            review_target,
            &mut out,
            &mut totals,
            &vdest,
            repo.as_path(),
            workers,
            max_extras,
            action.review_no_tools(),
            patch_ui.clone(),
        )
        .await;
    }

    // Run-wide quick summary. One AI call (validation model, falls back to main model) that
    // produces a short prose summary of the findings, plus locally-computed severity counts.
    // Always on for `review` regardless of validation mode (filter / off / findings); skipped
    // on dry-run, Ctrl-C, or non-review subcommands.
    if !cli.global.dry_run && !cancelled && action.is_review() {
        let summary_model = validation_cfg.as_ref().unwrap_or(&model);
        run_quick_summary(
            &client,
            summary_model,
            &model,
            review_target,
            &mut out,
            &mut totals,
            &vdest,
            repo.as_path(),
            patch_ui.as_deref(),
        )
        .await;
    }

    // Keep the shared footer live through every progress-using phase. In particular,
    // run_quick_summary() adds a temporary row and records tokens into this footer.
    if let Some(ui) = patch_ui.as_ref() {
        ui.finish_footer_eprintln();
    } else if std::io::stderr().is_terminal() && totals.api_calls > 0 {
        eprintln!(
            "{}",
            progress::usage_footer_line(
                totals.prompt,
                totals.completion,
                totals.cache_creation,
                totals.cache_read,
            )
        );
    }

    out["usage_summary"] = totals.json();
    out["wall_ms"] = json!(run_start.elapsed().as_millis() as u64);

    v(
        &vdest,
        format!(
            "{}. API calls={} prompt_tokens={} completion_tokens={}",
            if cancelled {
                "cancelled (Ctrl-C)"
            } else {
                "finished"
            },
            totals.api_calls,
            totals.prompt,
            totals.completion,
        ),
    );

    if cli.global.json {
        output::print_report_json(&out);
    } else {
        output::print_report_human(&out);
    }
    if cancelled {
        std::process::exit(130);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_single_pass(
    client: &reqwest::Client,
    model: &config::ResolvedModel,
    target: config::ReviewTarget,
    reference: &str,
    validation_context: &str,
    commit_headers: &str,
    patch_diff: &str,
    vd: &VerboseDest,
    patch_tag: &str,
    sha_short: &str,
    tool_cfg: Option<&api::ToolLoopConfig<'_>>,
    totals: &mut RunTotals,
    worker_ctx: Option<&WorkerLineCtx>,
    publisher: &SnapshotPublisher,
    effective_repo: &Path,
    second_opinion: Option<&config::ResolvedModel>,
) -> Result<CommitReviewResult> {
    let user = api::single_pass_user_payload(reference, commit_headers, patch_diff);
    let sys_len = crate::target::reviewer_system_prompt(target).len();
    let usr_len = user.len();
    v(
        vd,
        format!(
            "API: single-pass chat/completions - system={sys_len} chars, user={usr_len} chars (~{} input tokens rough)",
            rough_token_hint(sys_len + usr_len)
        ),
    );

    let display_total = 1 + second_opinion.is_some() as u32;
    let spin = stage_progress_line(patch_tag, sha_short, 1, display_total, "Single-pass review");
    let mut stage_tot = api::CumulativeTokenUsage::default();
    let t_review = Instant::now();
    let (parsed_review, _raw, usage, review_err, _attempts) =
        api::chat_completion_with_retry_stage_timeout(
            client,
            model,
            crate::target::reviewer_system_prompt(target),
            &user,
            model.temperature,
            Some(&spin),
            Some(&mut stage_tot),
            vd,
            tool_cfg,
            worker_ctx,
            effective_repo,
            api::parse_findings_json,
            api::RETRY_REMINDER_FINDINGS,
            api::STAGE_RETRY_MAX_ATTEMPTS,
        )
        .await;
    let d_review = t_review.elapsed();
    totals.add_usage(usage);
    let review_stage = StageUsage {
        step: "review",
        usage,
        wall: d_review,
        error: review_err.as_ref().map(api::short_error_reason),
    };
    let mut usage_step: Vec<StageUsage> = vec![review_stage.clone()];
    publisher.add_stage(review_stage);

    v(
        vd,
        format!(
            "single-pass response: prompt_tokens={:?} tokens={:?}",
            usage.prompt, usage.completion,
        ),
    );

    let mut findings_val = match parsed_review {
        Some(v) => v,
        None => {
            if let Some(e) = review_err {
                v(
                    vd,
                    format!(
                        "single-pass failed after retries (continuing with empty findings): {e:#}"
                    ),
                );
            }
            json!({ "findings": [] })
        }
    };
    publisher.set_findings(findings_val.clone());

    if let Some(cfg) = second_opinion {
        let so_findings = run_second_opinion(
            client,
            cfg,
            target,
            reference,
            &findings_val,
            commit_headers,
            patch_diff,
            patch_tag,
            sha_short,
            2,
            display_total,
            vd,
            worker_ctx,
            totals,
            publisher,
            &mut usage_step,
            effective_repo,
            tool_cfg,
        )
        .await;
        let added = append_findings(&mut findings_val, &so_findings);
        if added > 0 {
            v(
                vd,
                format!("second-opinion added {added} finding(s); merged into commit findings"),
            );
            publisher.set_findings(findings_val.clone());
        } else {
            v(vd, "second-opinion returned no additional findings");
        }
    }

    let usage_json = commit_usage_json(&usage_step);
    let steps = usage_steps_array(&usage_step);
    Ok(CommitReviewResult {
        findings_val,
        usage_commit: usage_json,
        usage_steps: Some(steps),
        phase0_selected_prompts: None,
        validation_context: validation_context.to_string(),
    })
}

async fn prefetch_context_block(
    effective_repo: &Path,
    patch_diff: &str,
    vd: &VerboseDest,
) -> String {
    match prefetch::prompt_block(effective_repo, patch_diff).await {
        Ok(Some(block)) => {
            v(
                vd,
                format!(
                    "pre-fetched source context: {} characters",
                    block.context_chars
                ),
            );
            block.text
        }
        Ok(None) => {
            v(vd, "pre-fetched source context: empty");
            String::new()
        }
        Err(e) => {
            v(
                vd,
                format!("pre-fetched source context failed (continuing without): {e:#}"),
            );
            String::new()
        }
    }
}

/// Phase 0: optional `subsystem/subsystem.md` guide selection (JSON).
/// Series context: `git log --reverse --format=%s` over the user range when later commits exist.
fn series_context_for_consolidation(
    repo: &Path,
    range: &str,
    commit_index: usize,
    num_commits: usize,
) -> String {
    if num_commits <= 1 {
        return "Not applicable: only one commit in the review range; there are no later patches in this range to compare against.".to_string();
    }
    if commit_index == num_commits - 1 {
        return "Not applicable: this is the last commit in the review range (no subsequent commits in the same range).".to_string();
    }
    match git::log_subjects_in_range(repo, range) {
        Ok(subjects) => {
            let t = subjects.trim_end();
            if t.is_empty() {
                format!(
                    "Git range `{}` returned no subjects from `git log --reverse --format=%s` (unexpected).",
                    range
                )
            } else {
                format!(
                    "User range: `{}` (same revision range as this boro run).\n\nPatch subjects in this range (oldest first):\n{}",
                    range, t
                )
            }
        }
        Err(e) => format!(
            "Could not run `git log` for series context on range `{}`: {}.",
            range, e
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_phase0_selection(
    client: &reqwest::Client,
    model: &config::ResolvedModel,
    target: config::ReviewTarget,
    subsystem_index_md: Option<&str>,
    patch: &str,
    vd: &VerboseDest,
    totals: &mut RunTotals,
    spinner_line: Option<&str>,
    cumulative: Option<&mut api::CumulativeTokenUsage>,
    worker_ctx: Option<&WorkerLineCtx>,
    effective_repo: &Path,
) -> Result<Option<(Vec<String>, String, TokenUsage, Duration)>> {
    let Some(index) = subsystem_index_md else {
        v(vd, "phase 0 skipped: subsystem/subsystem.md not found");
        return Ok(None);
    };
    let patch_capped = api::cap_utf8(patch, 200_000);
    let user = api::phase0_user_payload(index, &patch_capped);
    v(
        vd,
        format!(
            "API: phase 0 (identify subsystem) - user={} chars (~{} tokens rough)",
            user.len(),
            rough_token_hint(crate::target::phase0_system_prompt(target).len() + user.len())
        ),
    );
    let t0 = Instant::now();
    let (guides_opt, raw, usage, err, _attempts) = api::chat_completion_with_retry_stage_timeout(
        client,
        model,
        crate::target::phase0_system_prompt(target),
        &user,
        model.temperature,
        spinner_line,
        cumulative,
        vd,
        None,
        worker_ctx,
        effective_repo,
        api::parse_phase0_response,
        api::RETRY_REMINDER_PHASE0,
        api::STAGE_RETRY_MAX_ATTEMPTS,
    )
    .await;
    let elapsed = t0.elapsed();
    totals.add_usage(usage);
    let Some(guides) = guides_opt else {
        if let Some(e) = err {
            v(
                vd,
                format!("phase 0 failed after retries (continuing without): {e:#}"),
            );
        }
        return Ok(None);
    };
    v(vd, format!("phase 0 selected {} guide(s)", guides.len()));
    Ok(Some((guides, raw, usage, elapsed)))
}

/// Run the upstream-followup stage. Returns the rendered Markdown summary to splice into the
/// reference bundle for downstream discovery stages plus any deterministic upstream-branch
/// `Fixes:` hits. Errors are logged but never propagated - a failing follow-up stage must not
/// block a commit review.
#[allow(clippy::too_many_arguments)]
async fn run_upstream_followup_stage(
    client: &reqwest::Client,
    model: &config::ResolvedModel,
    repo: &Path,
    sha: &str,
    commit_headers: &str,
    patch_diff: &str,
    lore_cfg: &lore::LoreConfig,
    spinner_line: &str,
    vd: &VerboseDest,
    cumulative: &mut api::CumulativeTokenUsage,
    worker_ctx: Option<&WorkerLineCtx>,
    effective_repo: &Path,
    totals: &mut RunTotals,
    publisher: &SnapshotPublisher,
    usage_step: &mut Vec<StageUsage>,
    master_repo: Option<&lore::MasterRepo>,
    lore_active: bool,
) -> Option<UpstreamFollowupResult> {
    let subject = match git::commit_subject(repo, sha) {
        Ok(s) => s,
        Err(e) => {
            v(
                vd,
                format!("upstream-followup: commit_subject failed: {e:#}"),
            );
            return None;
        }
    };
    if subject.trim().is_empty() {
        v(vd, "upstream-followup: empty subject, skipping");
        return None;
    }

    let t_lei = Instant::now();
    let mut master_fixes = Vec::new();
    let master_summary = if let Some(master) = master_repo {
        match lore::find_master_fixes(master, sha).await {
            Ok(fixes) => {
                v(
                    vd,
                    format!(
                        "upstream-followup: upstream branch returned {} unapplied Fixes: match(es)",
                        fixes.len()
                    ),
                );
                if vd.stream_model_responses() && !fixes.is_empty() {
                    eprintln!(
                        "upstream-followup: found {} follow-up fix(es) in upstream branch:",
                        fixes.len()
                    );
                    for fix in &fixes {
                        eprintln!("  {} {}", short_sha10(&fix.sha), fix.subject);
                    }
                }
                let rendered = lore::render_master_fixes(&fixes);
                master_fixes = fixes;
                rendered
            }
            Err(e) => {
                v(
                    vd,
                    format!("upstream-followup: upstream branch query failed: {e:#}"),
                );
                String::new()
            }
        }
    } else {
        String::new()
    };
    if !lore_active {
        let stage = StageUsage {
            step: "followup",
            usage: TokenUsage::default(),
            wall: t_lei.elapsed(),
            error: None,
        };
        usage_step.push(stage.clone());
        publisher.add_stage(stage);
        return (!master_summary.is_empty()).then_some(UpstreamFollowupResult {
            summary: master_summary,
            master_fixes,
        });
    }
    v(
        vd,
        format!(
            "upstream-followup: running lei q -I {} (window {}, max_bytes {})",
            lore_cfg.inbox_url, lore_cfg.window, lore_cfg.max_bytes
        ),
    );
    let mbox_result = match lore::fetch_upstream_mbox(&subject, lore_cfg).await {
        Ok(r) => r,
        Err(e) => {
            v(vd, format!("upstream-followup: lei failed: {e:#}"));
            let stage = StageUsage {
                step: "followup",
                usage: TokenUsage::default(),
                wall: t_lei.elapsed(),
                error: Some(api::short_error_reason(&e)),
            };
            usage_step.push(stage.clone());
            publisher.add_stage(stage);
            return (!master_summary.is_empty()).then_some(UpstreamFollowupResult {
                summary: master_summary,
                master_fixes,
            });
        }
    };
    v(
        vd,
        format!(
            "upstream-followup: lei returned {} message(s) ({} bytes) in {:?}",
            mbox_result.hit_count,
            mbox_result.mbox.len(),
            t_lei.elapsed()
        ),
    );

    if mbox_result.hit_count == 0 {
        // Short-circuit: no need to call the model.
        let stage = StageUsage {
            step: "followup",
            usage: TokenUsage::default(),
            wall: t_lei.elapsed(),
            error: None,
        };
        usage_step.push(stage.clone());
        publisher.add_stage(stage);
        return Some(UpstreamFollowupResult {
            summary: format!(
                "{}{}",
                lore::render_followup_summary(&lore::no_activity_json(), &lore_cfg.inbox_url,),
                master_summary
            ),
            master_fixes,
        });
    }

    let user = api::upstream_followup_user_payload(
        &subject,
        commit_headers,
        patch_diff,
        &mbox_result.mbox,
        &mbox_result.query,
    );
    v(
        vd,
        format!(
            "API: upstream follow-up - user={} chars (~{} tokens rough)",
            user.len(),
            rough_token_hint(api::SYSTEM_UPSTREAM_FOLLOWUP.len() + user.len())
        ),
    );
    let t0 = Instant::now();
    let (parsed, _raw, usage, err, _attempts) = api::chat_completion_with_retry_stage_timeout(
        client,
        model,
        api::SYSTEM_UPSTREAM_FOLLOWUP,
        &user,
        model.temperature,
        Some(spinner_line),
        Some(cumulative),
        vd,
        None,
        worker_ctx,
        effective_repo,
        api::parse_upstream_followup_response,
        api::RETRY_REMINDER_UPSTREAM_FOLLOWUP,
        api::STAGE_RETRY_MAX_ATTEMPTS,
    )
    .await;
    let elapsed = t0.elapsed();
    totals.add_usage(usage);
    let stage = StageUsage {
        step: "followup",
        usage,
        wall: elapsed,
        error: err.as_ref().map(api::short_error_reason),
    };
    usage_step.push(stage.clone());
    publisher.add_stage(stage);
    match parsed {
        Some(json) => Some(UpstreamFollowupResult {
            summary: format!(
                "{}{}",
                lore::render_followup_summary(&json, &lore_cfg.inbox_url),
                master_summary
            ),
            master_fixes,
        }),
        None => {
            if let Some(e) = err {
                v(
                    vd,
                    format!("upstream-followup failed after retries (continuing without): {e:#}"),
                );
            }
            (!master_summary.is_empty()).then_some(UpstreamFollowupResult {
                summary: master_summary,
                master_fixes,
            })
        }
    }
}

/// Run the validation-model second-opinion call on a commit. Returns the parsed
/// `{"findings": [...]}` value (possibly empty). On API or parse failure,
/// returns an empty findings array so the main pipeline's findings are preserved.
#[allow(clippy::too_many_arguments)]
async fn run_second_opinion(
    client: &reqwest::Client,
    cfg: &config::ResolvedModel,
    target: config::ReviewTarget,
    reference: &str,
    current_findings: &Value,
    commit_headers: &str,
    patch_diff: &str,
    patch_tag: &str,
    sha_short: &str,
    step: u32,
    step_total: u32,
    vd: &VerboseDest,
    worker_ctx: Option<&WorkerLineCtx>,
    totals: &mut RunTotals,
    publisher: &SnapshotPublisher,
    usage_step: &mut Vec<StageUsage>,
    effective_repo: &Path,
    tool_cfg: Option<&api::ToolLoopConfig<'_>>,
) -> Value {
    let user =
        api::second_opinion_user_payload(reference, current_findings, commit_headers, patch_diff);
    v(
        vd,
        format!(
            "API: second-opinion - model={} user={} chars",
            cfg.model_id,
            user.len()
        ),
    );
    let spinner = stage_progress_line(
        patch_tag,
        sha_short,
        step,
        step_total,
        "Second-opinion review",
    );
    let t = Instant::now();
    let mut cum = api::CumulativeTokenUsage::default();
    let (parsed, _raw, usage, err, _attempts) = api::chat_completion_with_retry_stage_timeout(
        client,
        cfg,
        crate::target::second_opinion_system_prompt(target),
        &user,
        cfg.temperature,
        Some(&spinner),
        Some(&mut cum),
        vd,
        tool_cfg,
        worker_ctx,
        effective_repo,
        api::parse_findings_json,
        api::RETRY_REMINDER_FINDINGS,
        api::STAGE_RETRY_MAX_ATTEMPTS,
    )
    .await;
    let elapsed = t.elapsed();
    totals.add_usage(usage);
    let stage = StageUsage {
        step: "2nd-opinion",
        usage,
        wall: elapsed,
        error: err.as_ref().map(api::short_error_reason),
    };
    usage_step.push(stage.clone());
    publisher.add_stage(stage);
    match parsed {
        Some(p) => p,
        None => {
            if let Some(e) = err {
                v(
                    vd,
                    format!(
                        "second-opinion failed after retries (continuing without second-opinion findings): {e:#}"
                    ),
                );
            }
            json!({ "findings": [] })
        }
    }
}

fn append_findings(base: &mut Value, extra: &Value) -> usize {
    let extra_findings = extra
        .get("findings")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    let added = extra_findings.len();
    if added == 0 {
        return 0;
    }

    if !base.is_object() {
        *base = json!({ "findings": [] });
    }
    if base.get("findings").and_then(|f| f.as_array()).is_none() {
        base["findings"] = json!([]);
    }
    if let Some(findings) = base.get_mut("findings").and_then(|f| f.as_array_mut()) {
        findings.extend(extra_findings);
    }
    added
}

fn append_upstream_fix_findings(base: &mut Value, fixes: &[lore::MasterFix]) -> usize {
    if fixes.is_empty() {
        return 0;
    }
    if !base.is_object() {
        *base = json!({ "findings": [] });
    }
    if base.get("findings").and_then(|f| f.as_array()).is_none() {
        base["findings"] = json!([]);
    }
    let Some(findings) = base.get_mut("findings").and_then(|f| f.as_array_mut()) else {
        return 0;
    };
    let mut added = 0usize;
    for fix in fixes {
        let short = short_sha10(&fix.sha);
        findings.push(json!({
            "problem": format!(
                "The reviewed commit has an upstream follow-up fix: {short} {}.",
                fix.subject
            ),
            "severity": "High",
            "severity_explanation": format!(
                "The configured upstream branch contains commit {} with a Fixes: trailer naming this reviewed commit. This is high-confidence evidence that upstream later corrected a regression introduced here.",
                fix.sha
            ),
            "source": "upstream-fixes",
            "upstream_fix": {
                "sha": &fix.sha,
                "subject": &fix.subject,
                "date": &fix.date,
            }
        }));
        added += 1;
    }
    added
}

#[allow(clippy::too_many_arguments)]
async fn run_two_pass(
    client: &reqwest::Client,
    model: &config::ResolvedModel,
    target: config::ReviewTarget,
    repo: &Path,
    sha: &str,
    patch_tag: &str,
    sha_short: &str,
    patch: &str,
    commit_headers: &str,
    patch_diff: &str,
    changed_paths: &[String],
    max_context_size: usize,
    max_extras: usize,
    series_context: &str,
    vd: &VerboseDest,
    tool_cfg: Option<&api::ToolLoopConfig<'_>>,
    totals: &mut RunTotals,
    worker_ctx: Option<&WorkerLineCtx>,
    publisher: &SnapshotPublisher,
    effective_repo: &Path,
    second_opinion: Option<&config::ResolvedModel>,
    master_repo: Option<&lore::MasterRepo>,
) -> Result<CommitReviewResult> {
    let mut usage_step: Vec<StageUsage> = Vec::new();
    let mut stage_tot = api::CumulativeTokenUsage::default();

    let subsystem_index = prompts::load_subsystem_index(target, 120_000)?;
    let lore_cfg = lore::LoreConfig::from_env();
    let lore_active = lore_cfg.enabled && lore::lei_available();
    if lore_cfg.enabled && !lore_active {
        v(
            vd,
            "upstream-followup stage skipped for this run: `lei` not found on $PATH \
             (install public-inbox to enable lore.kernel.org retrieval)",
        );
    }
    // Display total = highest per-commit table index. Run-wide validation and LKML rendering use
    // their own progress rows after per-commit workers finish. Stages that don't run for a given
    // commit (subsystem skipped when no index, lore skipped when `lei` is missing, second-opinion
    // not configured) simply don't emit a progress line.
    let display_total: u32 = 9 + second_opinion.is_some() as u32;

    let phase0_spinner: Option<String> = if subsystem_index.is_some() {
        Some(stage_progress_line(
            patch_tag,
            sha_short,
            0,
            display_total,
            "Identify subsystem",
        ))
    } else {
        None
    };

    let phase0_selected_prompts = match run_phase0_selection(
        client,
        model,
        target,
        subsystem_index.as_deref(),
        patch,
        vd,
        totals,
        phase0_spinner.as_deref(),
        Some(&mut stage_tot),
        worker_ctx,
        effective_repo,
    )
    .await
    {
        Ok(Some((guides, _raw, u, dur))) => {
            let stage = StageUsage {
                step: "subsystem",
                usage: u,
                wall: dur,
                error: None,
            };
            usage_step.push(stage.clone());
            publisher.add_stage(stage);
            publisher.set_phase0(Some(guides.clone()));
            Some(guides)
        }
        Ok(None) => None,
        Err(e) => {
            v(vd, format!("phase 0 failed (continuing without): {e:#}"));
            let stage = StageUsage {
                step: "subsystem",
                usage: TokenUsage::default(),
                wall: Duration::from_millis(0),
                error: Some(api::short_error_reason(&e)),
            };
            usage_step.push(stage.clone());
            publisher.add_stage(stage);
            None
        }
    };

    let followup_summary = if lore_active || master_repo.is_some() {
        let spinner =
            stage_progress_line(patch_tag, sha_short, 1, display_total, "Upstream follow-up");
        run_upstream_followup_stage(
            client,
            model,
            repo,
            sha,
            commit_headers,
            patch_diff,
            &lore_cfg,
            &spinner,
            vd,
            &mut stage_tot,
            worker_ctx,
            effective_repo,
            totals,
            publisher,
            &mut usage_step,
            master_repo,
            lore_active,
        )
        .await
    } else {
        None
    };

    v(
        vd,
        format!(
            "building reference bundle (max {} chars, phase0={}) ...",
            max_context_size,
            phase0_selected_prompts.is_some()
        ),
    );
    let reference = prompts::build_reference_context(
        target,
        changed_paths,
        max_context_size,
        phase0_selected_prompts.as_deref(),
        followup_summary.as_ref().map(|f| f.summary.as_str()),
    )?;
    let prefetch_block = prefetch_context_block(effective_repo, patch_diff, vd).await;
    let reference_with_prefetch = if prefetch_block.is_empty() {
        reference.clone()
    } else {
        format!("{reference}{prefetch_block}")
    };

    let pass1_user =
        api::broad_concerns_user_payload(&reference_with_prefetch, commit_headers, patch_diff);
    v(
        vd,
        format!(
            "API: pass 1 (concerns) - system={} user={} chars (~{} tokens rough)",
            crate::target::reviewer_system_prompt(target).len(),
            pass1_user.len(),
            rough_token_hint(
                crate::target::reviewer_system_prompt(target).len() + pass1_user.len()
            )
        ),
    );

    let pass1_line = stage_progress_line(patch_tag, sha_short, 2, display_total, "Broad concerns");
    let t_pass1 = Instant::now();
    let (parsed_pass1, _raw1, u1, concerns_error, _attempts) =
        api::chat_completion_with_retry_stage_timeout(
            client,
            model,
            crate::target::reviewer_system_prompt(target),
            &pass1_user,
            model.temperature,
            Some(&pass1_line),
            Some(&mut stage_tot),
            vd,
            tool_cfg,
            worker_ctx,
            effective_repo,
            api::parse_concerns_strict,
            api::RETRY_REMINDER_CONCERNS,
            api::STAGE_RETRY_MAX_ATTEMPTS,
        )
        .await;
    let d_pass1 = t_pass1.elapsed();
    totals.add_usage(u1);
    let concerns_stage = StageUsage {
        step: "concerns",
        usage: u1,
        wall: d_pass1,
        error: concerns_error.as_ref().map(api::short_error_reason),
    };
    usage_step.push(concerns_stage.clone());
    publisher.add_stage(concerns_stage);

    v(
        vd,
        format!(
            "pass 1 done: prompt_tokens={:?} tokens={:?}",
            u1.prompt, u1.completion
        ),
    );

    let mut concerns = match parsed_pass1 {
        Some(v1) => v1.get("concerns").cloned().unwrap_or_else(|| json!([])),
        None => {
            v(
                vd,
                "pass 1 (concerns) failed after retries - continuing with empty concerns",
            );
            json!([])
        }
    };
    let repaired =
        api::repair_misattributed_message_concerns(&mut concerns, commit_headers, patch_diff);
    if repaired.relocated > 0 || repaired.dropped > 0 {
        v(
            vd,
            format!(
                "pass 1 source repair: relocated {} patch-text concern(s), dropped {} ambiguous concern(s)",
                repaired.relocated, repaired.dropped
            ),
        );
    }

    let patch_slim = api::cap_utf8(patch_diff, 400_000);
    v(
        vd,
        format!(
            "specialist stages 3-8: diff-only patch context {} characters",
            patch_slim.len()
        ),
    );

    let mut merged_concerns: Vec<Value> = Vec::new();
    if let Some(a) = concerns.as_array() {
        merged_concerns.extend(a.iter().cloned());
    }
    publish_fallback_findings(publisher, &merged_concerns);

    let prior_block = api::format_prior_concerns_for_specialist(&concerns, 8_000);
    if !prior_block.is_empty() {
        v(
            vd,
            format!(
                "specialist stages 3-8: chaining {} chars of Pass 1 concerns",
                prior_block.len()
            ),
        );
    }
    let fp_digest = prompts::load_false_positive_digest();
    v(
        vd,
        format!(
            "specialist stages 3-8: injecting {} chars of FP digest",
            fp_digest.len()
        ),
    );

    for st in 3u8..=8u8 {
        let Some(instr) = stages::instruction_body(st) else {
            continue;
        };
        // Stage 8 (comment / code consistency) is comment-vs-code only - if the
        // diff added or removed no comment lines there is nothing to audit and
        // the call would burn tokens (the prompt is tool-heavy). Skip outright,
        // and drop the prior-concerns block + FP digest when we do run it: both
        // are concern-hunting context that doesn't help a comment audit.
        if st == 8 && !api::diff_touches_comments(&patch_slim) {
            v(
                vd,
                "specialist stage 8 skipped (diff touches no comment lines)",
            );
            continue;
        }
        let addon = prompts::load_stage_prompt_files(target, st, max_extras)?;
        let (prior_for_stage, fp_for_stage) = if st == 8 {
            ("", "")
        } else {
            (prior_block.as_str(), fp_digest.as_str())
        };
        let user = api::specialist_stage_user_payload(
            instr,
            &addon,
            &patch_slim,
            &prefetch_block,
            st,
            prior_for_stage,
            fp_for_stage,
        );
        v(
            vd,
            format!(
                "API: specialist stage {st} - user={} chars (~{} tokens rough)",
                user.len(),
                rough_token_hint(crate::target::reviewer_system_prompt(target).len() + user.len())
            ),
        );
        let step_label: &'static str = stages::short_label(st);
        let spinner = stage_progress_line(
            patch_tag,
            sha_short,
            u32::from(st),
            display_total,
            stages::short_description(st),
        );
        let t_st = Instant::now();
        let required_tool_cfg = (st == 7 && tool_cfg.is_some()).then(|| {
            api::ToolLoopConfig::new(effective_repo).requiring(api::ToolVerification::Stage7Linkage)
        });
        let stage_tool_cfg = required_tool_cfg.as_ref().or(tool_cfg);
        let (parsed_stage, _raw_s, u_s, stage_error, _attempts) =
            api::chat_completion_with_retry_stage_timeout(
                client,
                model,
                crate::target::reviewer_system_prompt(target),
                &user,
                model.temperature,
                Some(&spinner),
                Some(&mut stage_tot),
                vd,
                stage_tool_cfg,
                worker_ctx,
                effective_repo,
                |raw| {
                    if st == 7 {
                        api::parse_stage7_concerns_strict(raw)
                    } else {
                        api::parse_concerns_strict(raw)
                    }
                },
                if st == 7 {
                    api::RETRY_REMINDER_STAGE7_CONCERNS
                } else {
                    api::RETRY_REMINDER_CONCERNS
                },
                api::STAGE_RETRY_MAX_ATTEMPTS,
            )
            .await;
        let d_st = t_st.elapsed();
        totals.add_usage(u_s);
        let stage_entry = StageUsage {
            step: step_label,
            usage: u_s,
            wall: d_st,
            error: stage_error.as_ref().map(api::short_error_reason),
        };
        usage_step.push(stage_entry.clone());
        publisher.add_stage(stage_entry);
        if let Some(vs) = parsed_stage {
            if let Some(a) = vs.get("concerns").and_then(|x| x.as_array()) {
                merged_concerns.extend(a.iter().cloned());
            }
        } else {
            v(
                vd,
                format!("specialist stage {st} failed after retries (continuing with empty)"),
            );
        }
        publish_fallback_findings(publisher, &merged_concerns);
    }

    let merged = Value::Array(merged_concerns);
    if merged.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        v(
            vd,
            "pass 2 skipped (no concerns after broad pass + stages 3-8)",
        );

        // Validation-model second-opinion review. Run even though the main pipeline produced no
        // findings so the stronger model gets a full independent look at the patch.
        let mut findings_val = json!({ "findings": [] });
        let upstream_added = followup_summary
            .as_ref()
            .map(|f| append_upstream_fix_findings(&mut findings_val, &f.master_fixes))
            .unwrap_or(0);
        if upstream_added > 0 {
            v(
                vd,
                format!(
                    "upstream-followup added {upstream_added} upstream fix finding(s); merged into commit findings"
                ),
            );
            publisher.set_findings(findings_val.clone());
        }
        if let Some(cfg) = second_opinion {
            let so_findings = run_second_opinion(
                client,
                cfg,
                target,
                &reference_with_prefetch,
                &findings_val,
                commit_headers,
                patch_diff,
                patch_tag,
                sha_short,
                10,
                display_total,
                vd,
                worker_ctx,
                totals,
                publisher,
                &mut usage_step,
                effective_repo,
                tool_cfg,
            )
            .await;
            let added = append_findings(&mut findings_val, &so_findings);
            if added > 0 {
                v(
                    vd,
                    format!("second-opinion added {added} finding(s); merged into commit findings"),
                );
                publisher.set_findings(findings_val.clone());
            } else {
                v(vd, "second-opinion returned no additional findings");
            }
        }

        let usage_json = commit_usage_json(&usage_step);
        let steps = usage_steps_array(&usage_step);
        return Ok(CommitReviewResult {
            findings_val,
            usage_commit: usage_json,
            usage_steps: Some(steps),
            phase0_selected_prompts,
            validation_context: prefetch_block.clone(),
        });
    }

    v(
        vd,
        "loading consolidation extras (false-positive-guide, severity) ...",
    );
    let extras = prompts::load_consolidation_extras(target, max_extras)?;
    v(
        vd,
        format!("consolidation extras: {} characters", extras.len()),
    );

    // Pre-cluster the merged concerns before consolidation: trigram-based local dedup
    // collapses near-duplicate descriptions emitted by Pass 1 + the 5 specialist stages,
    // so the consolidator sees one row per distinct concern instead of N near-duplicates.
    let clustered_concerns =
        cluster::cluster_concerns(merged.as_array().map(|a| a.as_slice()).unwrap_or(&[]));
    let merged_before = merged.as_array().map(|a| a.len()).unwrap_or(0);
    let merged_after = clustered_concerns.len();
    if merged_after < merged_before {
        v(
            vd,
            format!(
                "consolidation pre-cluster: {merged_before} concerns → {merged_after} after local dedup"
            ),
        );
    }
    let clustered_value = Value::Array(clustered_concerns);

    let pass2_user = api::consolidation_user_payload(
        &extras,
        &json!({ "concerns": clustered_value }),
        series_context,
        &prefetch_block,
    );
    v(
        vd,
        format!(
            "API: pass 2 (consolidation) - user={} chars (~{} tokens rough)",
            pass2_user.len(),
            rough_token_hint(
                crate::target::reviewer_system_prompt(target).len() + pass2_user.len()
            )
        ),
    );

    let pass2_line =
        stage_progress_line(patch_tag, sha_short, 9, display_total, "Consolidation pass");
    let t_p2 = Instant::now();
    let (parsed_pass2, _raw2, u2, pass2_err, _attempts) =
        api::chat_completion_with_retry_stage_timeout(
            client,
            model,
            crate::target::reviewer_system_prompt(target),
            &pass2_user,
            model.temperature,
            Some(&pass2_line),
            Some(&mut stage_tot),
            vd,
            // Consolidation is pure synthesis over the clustered prior concerns plus
            // the prefetched source context. Disable tools here so the model can't
            // drift into re-investigating concerns it should only be deduping and
            // severity-ranking. Saves 3-4 tool-loop iterations of re-sent context
            // per commit on tool-happy models.
            None,
            worker_ctx,
            effective_repo,
            api::parse_findings_json,
            api::RETRY_REMINDER_FINDINGS,
            api::STAGE_RETRY_MAX_ATTEMPTS,
        )
        .await;
    let d_p2 = t_p2.elapsed();
    totals.add_usage(u2);
    let pass2_unusable = parsed_pass2.is_none();
    let pass2_error = pass2_err.as_ref().map(api::short_error_reason);
    let consolidation_stage = StageUsage {
        step: "consolidation",
        usage: u2,
        wall: d_p2,
        error: pass2_error,
    };
    usage_step.push(consolidation_stage.clone());
    publisher.add_stage(consolidation_stage);

    v(
        vd,
        format!(
            "pass 2 done: prompt_tokens={:?} tokens={:?}",
            u2.prompt, u2.completion
        ),
    );

    if pass2_unusable {
        if let Some(e) = &pass2_err {
            v(
                vd,
                format!(
                    "consolidation failed after retries (continuing with empty findings): {e:#}"
                ),
            );
        }
    }
    let mut consolidated_findings = parsed_pass2.unwrap_or_else(|| json!({ "findings": [] }));
    publisher.set_findings(consolidated_findings.clone());

    let findings_empty = consolidated_findings
        .get("findings")
        .and_then(|f| f.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);

    let merged_non_empty = merged.as_array().map(|a| !a.is_empty()).unwrap_or(false);
    if findings_empty && merged_non_empty && pass2_unusable {
        v(
            vd,
            "consolidation did not return usable findings; using merged concerns as fallback findings",
        );
        consolidated_findings = api::findings_from_merged_concerns(&merged);
        publisher.set_findings(consolidated_findings.clone());
    }

    let upstream_added = followup_summary
        .as_ref()
        .map(|f| append_upstream_fix_findings(&mut consolidated_findings, &f.master_fixes))
        .unwrap_or(0);
    if upstream_added > 0 {
        v(
            vd,
            format!(
                "upstream-followup added {upstream_added} upstream fix finding(s); merged into commit findings"
            ),
        );
        publisher.set_findings(consolidated_findings.clone());
    }

    // Validation-model second-opinion review. It runs after the main pipeline produced its
    // current findings, appends any additional findings, and leaves global findings validation
    // to drop false positives / tighten / merge same-location duplicates.
    if let Some(cfg) = second_opinion {
        let so_findings = run_second_opinion(
            client,
            cfg,
            target,
            &reference_with_prefetch,
            &consolidated_findings,
            commit_headers,
            patch_diff,
            patch_tag,
            sha_short,
            10,
            display_total,
            vd,
            worker_ctx,
            totals,
            publisher,
            &mut usage_step,
            effective_repo,
            tool_cfg,
        )
        .await;
        let added = append_findings(&mut consolidated_findings, &so_findings);
        if added > 0 {
            v(
                vd,
                format!("second-opinion added {added} finding(s); merged into commit findings"),
            );
            publisher.set_findings(consolidated_findings.clone());
        } else {
            v(vd, "second-opinion returned no additional findings");
        }
    }

    let usage_json = commit_usage_json(&usage_step);
    let steps = usage_steps_array(&usage_step);
    Ok(CommitReviewResult {
        findings_val: consolidated_findings,
        usage_commit: usage_json,
        usage_steps: Some(steps),
        phase0_selected_prompts,
        validation_context: prefetch_block,
    })
}

/// Update the snapshot's findings to a fallback derived from the merged concerns
/// gathered so far, so a Ctrl-C dump after stage 3–7 still surfaces something.
fn publish_fallback_findings(publisher: &SnapshotPublisher, merged: &[Value]) {
    if merged.is_empty() {
        return;
    }
    let val = api::findings_from_merged_concerns(&Value::Array(merged.to_vec()));
    publisher.set_findings(val);
}

fn commit_usage_json(steps: &[StageUsage]) -> Value {
    let mut prompt: u64 = 0;
    let mut completion: u64 = 0;
    let mut cache_creation: u64 = 0;
    let mut cache_read: u64 = 0;
    for s in steps {
        if let Some(p) = s.usage.prompt {
            prompt += u64::from(p);
        }
        if let Some(c) = s.usage.completion {
            completion += u64::from(c);
        }
        if let Some(cw) = s.usage.cache_creation {
            cache_creation += u64::from(cw);
        }
        if let Some(cr) = s.usage.cache_read {
            cache_read += u64::from(cr);
        }
    }
    usage_summary_json(
        steps.len() as u64,
        prompt,
        completion,
        cache_creation,
        cache_read,
    )
}

fn usage_summary_json_from_step_values(steps: &[Value]) -> Value {
    let mut prompt: u64 = 0;
    let mut completion: u64 = 0;
    let mut cache_creation: u64 = 0;
    let mut cache_read: u64 = 0;
    let mut api_calls: u64 = 0;
    for s in steps {
        api_calls += s.get("api_calls").and_then(|v| v.as_u64()).unwrap_or(1);
        prompt += s.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        completion += s
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        cache_creation += s
            .get("cache_creation_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        cache_read += s
            .get("cache_read_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
    }
    usage_summary_json(api_calls, prompt, completion, cache_creation, cache_read)
}

fn usage_summary_json(
    api_calls: u64,
    prompt: u64,
    completion: u64,
    cache_creation: u64,
    cache_read: u64,
) -> Value {
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "cache_creation_tokens": cache_creation,
        "cache_read_tokens": cache_read,
        "api_calls": api_calls,
    })
}

fn usage_steps_array(steps: &[StageUsage]) -> Value {
    let arr: Vec<Value> = steps
        .iter()
        .map(|s| {
            json!({
                "step": s.step,
                "prompt_tokens": s.usage.prompt,
                "completion_tokens": s.usage.completion,
                "cache_creation_tokens": s.usage.cache_creation,
                "cache_read_tokens": s.usage.cache_read,
                "wall_ms": s.wall.as_millis() as u64,
                "error": s.error,
            })
        })
        .collect();
    Value::Array(arr)
}

#[cfg(test)]
mod drop_unanchored_tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/foo.c b/foo.c
--- a/foo.c
+++ b/foo.c
@@ -10,3 +10,4 @@
 ctx
-removed
+added_a
+added_b
 last
";

    fn vd() -> VerboseDest {
        VerboseDest::new(false)
    }

    #[test]
    fn json_step_usage_summary_counts_aggregate_step_calls() {
        let steps = vec![
            json!({
                "step": "concerns",
                "prompt_tokens": 10,
                "completion_tokens": 2,
                "cache_creation_tokens": null,
                "cache_read_tokens": 3,
                "wall_ms": 1000,
                "error": null,
            }),
            json!({
                "step": "lkml x3",
                "prompt_tokens": 7,
                "completion_tokens": 5,
                "cache_creation_tokens": 1,
                "cache_read_tokens": null,
                "api_calls": 3,
                "wall_ms": 2000,
                "error": null,
            }),
        ];

        let usage = usage_summary_json_from_step_values(&steps);
        assert_eq!(usage["api_calls"], 4);
        assert_eq!(usage["prompt_tokens"], 17);
        assert_eq!(usage["completion_tokens"], 7);
        assert_eq!(usage["cache_creation_tokens"], 1);
        assert_eq!(usage["cache_read_tokens"], 3);
    }

    #[test]
    fn mandatory_verification_failure_withholds_only_sensitive_findings() {
        let findings = json!([{
            "problem": "helper has no caller",
            "severity": "Medium",
            "severity_explanation": "repository lookup required"
        }, {
            "problem": "lock is released twice",
            "severity": "High",
            "severity_explanation": "double unlock on the error path"
        }]);

        let (retained, withheld) = without_unverified_sensitive_findings(&findings);
        assert_eq!(withheld, 1);
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0]["problem"], "lock is released twice");
    }

    #[test]
    fn quick_summary_resolver_uses_authoritative_metadata_and_filters_refs() {
        let full_sha = "a64709a4a613d3008b63c1c7d20c295bdd1cad49";
        let response = api::parse_quick_summary_response(
            &json!({
                "text": "One issue needs attention.",
                "highlights": [{
                    "finding_ref": format!("{full_sha}:0"),
                    "title": "Notifier callbacks can self-deadlock",
                    "question": "Can callbacks re-enter registration?",
                    "commit": "model-authored-commit",
                    "severity": "Critical",
                    "location": {"file": "invented.c", "line": 999, "side": "LEFT"}
                }, {
                    "finding_ref": format!("{full_sha}:0"),
                    "title": "Later duplicate",
                    "question": "Should this duplicate be dropped?"
                }, {
                    "finding_ref": "unknown:0",
                    "title": "Unknown reference",
                    "question": "Should this unknown reference be dropped?"
                }]
            })
            .to_string(),
        )
        .expect("parse response");
        let commits_data = vec![(
            full_sha.to_string(),
            "dpll: fix notifier locking".to_string(),
            json!([{
                "problem": "callback re-entry deadlocks",
                "severity": "Medium",
                "severity_explanation": "lock is held across callbacks",
                "location": {
                    "file": "drivers/dpll/dpll_core.c",
                    "line": 51,
                    "line_end": 54,
                    "side": "RIGHT"
                }
            }]),
        )];

        let resolved = resolve_quick_summary_highlights(&response, &commits_data);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0]["finding_ref"], format!("{full_sha}:0"));
        assert_eq!(resolved[0]["commit"], full_sha);
        assert_eq!(resolved[0]["severity"], "Medium");
        assert_eq!(resolved[0]["location"]["file"], "drivers/dpll/dpll_core.c");
        assert_eq!(resolved[0]["location"]["line"], 51);
        assert_eq!(resolved[0]["title"], "Notifier callbacks can self-deadlock");
        assert_eq!(
            resolved[0]["question"],
            "Can callbacks re-enter registration?"
        );
    }

    #[test]
    fn quick_summary_resolver_publishes_at_most_three_known_refs() {
        let full_sha = "0123456789abcdef0123456789abcdef01234567";
        let response = api::QuickSummaryResponse {
            text: "Several issues need attention.".to_string(),
            highlights: (0..4)
                .map(|index| api::QuickSummaryHighlight {
                    finding_ref: format!("{full_sha}:{index}"),
                    title: format!("Issue {index}"),
                    question: format!("Question {index}?"),
                })
                .collect(),
        };
        let commits_data = vec![(
            full_sha.to_string(),
            "subject".to_string(),
            json!([
                {"severity": "Critical"},
                {"severity": "High"},
                {"severity": "Medium"},
                {"severity": "Low"}
            ]),
        )];

        let resolved = resolve_quick_summary_highlights(&response, &commits_data);

        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0]["finding_ref"], format!("{full_sha}:0"));
        assert_eq!(resolved[1]["finding_ref"], format!("{full_sha}:1"));
        assert_eq!(resolved[2]["finding_ref"], format!("{full_sha}:2"));
    }

    #[test]
    fn quick_summary_resolver_drops_findings_without_canonical_severity() {
        let full_sha = "fedcba9876543210fedcba9876543210fedcba98";
        let response = api::QuickSummaryResponse {
            text: "Malformed findings must not become highlights.".to_string(),
            highlights: (0..3)
                .map(|index| api::QuickSummaryHighlight {
                    finding_ref: format!("{full_sha}:{index}"),
                    title: format!("Issue {index}"),
                    question: format!("Question {index}?"),
                })
                .collect(),
        };
        let commits_data = vec![(
            full_sha.to_string(),
            "subject".to_string(),
            json!([
                {"problem": "missing severity"},
                {"problem": "non-string severity", "severity": 2},
                {"problem": "noncanonical severity", "severity": "medium"}
            ]),
        )];

        let resolved = resolve_quick_summary_highlights(&response, &commits_data);

        assert!(resolved.is_empty());
    }

    #[test]
    fn upstream_fixes_become_high_severity_findings() {
        let fixes = vec![lore::MasterFix {
            sha: "0123456789abcdef".to_string(),
            subject: "net: fix later regression".to_string(),
            date: "2026-06-01T00:00:00+00:00".to_string(),
        }];
        let mut findings = json!({ "findings": [] });

        let added = append_upstream_fix_findings(&mut findings, &fixes);

        assert_eq!(added, 1);
        let f = &findings["findings"][0];
        assert_eq!(f["severity"], "High");
        assert_eq!(f["source"], "upstream-fixes");
        assert_eq!(f["upstream_fix"]["sha"], "0123456789abcdef");
        assert!(f["problem"]
            .as_str()
            .unwrap()
            .contains("net: fix later regression"));
    }

    #[test]
    fn keeps_location_on_real_hunk_line() {
        let idx = diff_index::DiffIndex::from_unified_diff(DIFF);
        let mut findings = json!({
            "findings": [
                {"problem": "x", "severity": "Low", "severity_explanation": "y",
                 "location": {"file": "foo.c", "line": 11, "side": "RIGHT"}}
            ]
        });
        drop_unanchored_locations(&mut findings, &idx, &vd());
        let arr = findings["findings"].as_array().unwrap();
        assert!(arr[0].get("location").is_some());
    }

    #[test]
    fn drops_location_outside_any_hunk() {
        let idx = diff_index::DiffIndex::from_unified_diff(DIFF);
        let mut findings = json!({
            "findings": [
                {"problem": "x", "severity": "Low", "severity_explanation": "y",
                 "location": {"file": "foo.c", "line": 999, "side": "RIGHT"}}
            ]
        });
        drop_unanchored_locations(&mut findings, &idx, &vd());
        let arr = findings["findings"].as_array().unwrap();
        assert!(arr[0].get("location").is_none());
        // Finding itself survives.
        assert_eq!(arr[0]["problem"], "x");
    }

    #[test]
    fn drops_location_that_does_not_contain_named_symbol() {
        let diff = "--- a/foo.c\n+++ b/foo.c\n@@ -1,3 +1,4 @@\n int unrelated;\n+int still_unrelated;\n+void target_helper(void);\n int tail;\n";
        let idx = diff_index::DiffIndex::from_unified_diff(diff);
        let mut findings = json!({"findings":[{
            "problem":"`target_helper()` has broken linkage",
            "severity":"High",
            "severity_explanation":"x",
            "location":{"file":"foo.c","line":2,"side":"RIGHT"}
        }]});
        drop_unanchored_locations(&mut findings, &idx, &vd());
        assert!(findings["findings"][0].get("location").is_none());
    }

    #[test]
    fn keeps_location_containing_named_symbol() {
        let diff = "--- a/foo.c\n+++ b/foo.c\n@@ -1,1 +1,1 @@\n+void target_helper(void);\n";
        let idx = diff_index::DiffIndex::from_unified_diff(diff);
        let mut findings = json!({"findings":[{
            "problem":"`target_helper()` has broken linkage",
            "severity":"High",
            "severity_explanation":"x",
            "location":{"file":"foo.c","line":1,"side":"RIGHT"}
        }]});
        drop_unanchored_locations(&mut findings, &idx, &vd());
        assert!(findings["findings"][0].get("location").is_some());
    }

    #[test]
    fn drops_only_line_end_when_line_anchors_but_end_does_not() {
        let idx = diff_index::DiffIndex::from_unified_diff(DIFF);
        let mut findings = json!({
            "findings": [
                {"problem": "x", "severity": "Low", "severity_explanation": "y",
                 "location": {"file": "foo.c", "line": 11, "line_end": 999, "side": "RIGHT"}}
            ]
        });
        drop_unanchored_locations(&mut findings, &idx, &vd());
        let loc = findings["findings"][0].get("location").unwrap();
        assert_eq!(loc["line"], 11);
        assert!(loc.get("line_end").is_none());
    }

    #[test]
    fn no_location_is_noop() {
        let idx = diff_index::DiffIndex::from_unified_diff(DIFF);
        let mut findings = json!({
            "findings": [
                {"problem": "x", "severity": "Low", "severity_explanation": "y"}
            ]
        });
        drop_unanchored_locations(&mut findings, &idx, &vd());
        assert!(findings["findings"][0].get("location").is_none());
    }

    #[test]
    fn wrong_side_drops_location() {
        // line 13 is RIGHT (the `last` context line, new_no=13); on LEFT side line 13
        // is past the hunk so it does not exist.
        let idx = diff_index::DiffIndex::from_unified_diff(DIFF);
        let mut findings = json!({
            "findings": [
                {"problem": "x", "severity": "Low", "severity_explanation": "y",
                 "location": {"file": "foo.c", "line": 13, "side": "LEFT"}}
            ]
        });
        drop_unanchored_locations(&mut findings, &idx, &vd());
        assert!(findings["findings"][0].get("location").is_none());
    }

    #[test]
    fn invalid_location_is_cleaned_before_message_typo_repair() {
        let patch = "\
diff --git a/foo.c b/foo.c
--- a/foo.c
+++ b/foo.c
@@ -10 +10,2 @@
 context
+/* CPU is avaialable. */
";
        let mut findings = json!({"findings": [{
            "problem": "commit message typo: `avaialable` should be `available`.",
            "severity": "Low",
            "severity_explanation": "The misspelling is in the commit message.",
            "offending_text": "avaialable",
            "replacement_text": "available",
            "location": {"file": "foo.c", "line": 999, "side": "RIGHT"}
        }]});

        let repaired = cleanup_repair_and_validate_findings(
            &mut findings,
            "Clean commit message.",
            patch,
            &["foo.c".to_string()],
            &vd(),
        );

        assert_eq!(repaired.relocated, 1);
        assert_eq!(repaired.dropped, 0);
        assert_eq!(findings["findings"][0]["location"]["file"], "foo.c");
        assert_eq!(findings["findings"][0]["location"]["line"], 11);
        assert_eq!(findings["findings"][0]["location"]["side"], "RIGHT");
        assert!(findings["findings"][0]["problem"]
            .as_str()
            .unwrap()
            .contains("added source comment"));
    }
}
