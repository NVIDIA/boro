// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Generate a per-commit kernel `.config` fragment for `boro build` / `boro test`.
//!
//! Without this, `vng -b` builds with virtme-ng's default config and may skip every file the
//! patch touched (because the gating `CONFIG_*` symbol is unset). We ask the model to read the
//! diff and emit the `CONFIG_*` options needed to compile the changed files (and exercise them at
//! boot, when relevant), then write a fragment to a tempfile and pass it to `vng -b --config`.
//!
//! Generation is best-effort: if the model fails or returns nothing usable, we fall back to
//! running `vng -b` without `--config`. The previous behavior is therefore the worst-case
//! outcome, never a regression.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tempfile::NamedTempFile;

use crate::api::{self, TokenUsage};
use crate::config::ResolvedModel;
use crate::progress::WorkerLineCtx;
use crate::verbose::VerboseDest;

/// Outcome of one `generate_fragment` call. The caller adds this to its `usage_steps` accounting
/// and passes `file.as_ref().map(NamedTempFile::path)` to `vng::run_build`.
///
/// **The `NamedTempFile` must outlive the `vng -b` invocation**: dropping it deletes the backing
/// file from disk before vng has a chance to read it.
pub struct KconfigStage {
    pub file: Option<NamedTempFile>,
    pub lines: Vec<String>,
    pub usage: TokenUsage,
    pub wall: Duration,
    pub error: Option<String>,
}

/// Strict whitelist of accepted `.config` line shapes — guards against the model smuggling
/// arbitrary text into the fragment.
fn line_is_kconfig_entry(s: &str) -> bool {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("# CONFIG_") {
        let Some(name) = rest.strip_suffix(" is not set") else {
            return false;
        };
        return !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    }
    let Some(rest) = s.strip_prefix("CONFIG_") else {
        return false;
    };
    let Some((name, val)) = rest.split_once('=') else {
        return false;
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    matches!(val, "y" | "m" | "n")
}

/// Broader validator for explicit user-supplied config fragments.
///
/// Model output stays on [`line_is_kconfig_entry`] so it cannot smuggle arbitrary values. The CLI
/// path (`boro test --config CONFIG_FOO=...`) needs real Kconfig scalar/string values such as
/// `CONFIG_NR_CPUS=512`, `CONFIG_PHYSICAL_START=0x1000000`, or
/// `CONFIG_CMDLINE="console=ttyS0 root=/dev/vda"`.
fn line_is_user_kconfig_entry(s: &str) -> bool {
    let s = s.trim();
    if line_is_kconfig_entry(s) {
        return true;
    }
    let Some(rest) = s.strip_prefix("CONFIG_") else {
        return false;
    };
    let Some((name, val)) = rest.split_once('=') else {
        return false;
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    !val.is_empty() && !val.contains('\n') && !val.contains('\r')
}

fn parse_lines(raw: &str) -> Result<ParsedFragment> {
    let v = api::parse_model_json_with_key(raw, "config")?;
    let arr = v
        .get("config")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut config_lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in arr {
        let Some(s) = entry.as_str() else { continue };
        let trimmed = s.trim();
        if line_is_kconfig_entry(trimmed) && seen.insert(trimmed.to_string()) {
            config_lines.push(trimmed.to_string());
        }
    }

    // Optional `kselftests`: list of area names relative to `tools/testing/selftests/`. We
    // dedupe and validate here; non-existent areas are silently dropped later when the resolver
    // can't open `<area>/config`. Missing key is fine (older models won't emit it).
    let mut kselftest_areas: Vec<String> = Vec::new();
    let mut seen_areas: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(arr) = v.get("kselftests").and_then(Value::as_array) {
        for entry in arr {
            let Some(s) = entry.as_str() else { continue };
            // Trim whitespace; strip a *trailing* slash only (so `sched_ext/` normalises to
            // `sched_ext`). Don't strip leading slashes — that would silently rewrite an
            // absolute-looking `/abs` into `abs` and bypass the validator.
            let trimmed = s.trim().trim_end_matches('/');
            if trimmed.is_empty() || !validate_kselftest_area(trimmed) {
                continue;
            }
            if seen_areas.insert(trimmed.to_string()) {
                kselftest_areas.push(trimmed.to_string());
            }
        }
    }

    Ok(ParsedFragment {
        config_lines,
        kselftest_areas,
    })
}

/// Whitelist of acceptable kselftest area names. Rejects empty, absolute paths, parent-segment
/// traversal (`..`), and anything outside the alphanumeric/underscore/hyphen/forward-slash set.
/// The build-side resolver also requires the corresponding `config` file to exist on disk before
/// reading it, but this lexical guard prevents path-traversal attempts before they reach `Path::join`.
fn validate_kselftest_area(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') {
        return false;
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/';
    if !name.chars().all(allowed) {
        return false;
    }
    // Reject any `..` segment. (Also catches `foo/../bar`, `..foo` is actually allowed because
    // it's a single segment without `/`, but `..` alone or as a path segment is forbidden.)
    name.split('/').all(|seg| seg != "..")
}

/// Outcome of parsing the model's JSON reply: the validated `CONFIG_*` lines plus any kselftest
/// area names the model identified as exercising the changed code (each relative to
/// `tools/testing/selftests/`, e.g. `"sched_ext"` or `"net/forwarding"`).
struct ParsedFragment {
    config_lines: Vec<String>,
    kselftest_areas: Vec<String>,
}

fn write_fragment(lines: &[String]) -> Result<NamedTempFile> {
    let mut f = tempfile::Builder::new()
        .prefix("boro-kconfig-")
        .suffix(".config")
        .tempfile()
        .context("create kconfig fragment tempfile")?;
    writeln!(
        f,
        "# Generated by boro: kconfig fragment selected by the model from the patch diff."
    )?;
    for l in lines {
        writeln!(f, "{l}")?;
    }
    f.flush()?;
    Ok(f)
}

/// Build a [`KconfigStage`] from caller-supplied config lines instead of asking the model.
///
/// This is used by `boro test --config CONFIG_FOO`: the user already supplied the primary option under
/// test, so the build should merge that fragment directly. Lines still pass through validation
/// before being written to disk, but this path accepts Kconfig scalar/string values in addition
/// to tristates.
pub fn fragment_from_lines(lines: Vec<String>, step_label: &str, vd: &VerboseDest) -> KconfigStage {
    let mut merged = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in lines {
        let trimmed = line.trim();
        if line_is_user_kconfig_entry(trimmed) && seen.insert(trimmed.to_string()) {
            merged.push(trimmed.to_string());
        }
    }

    if merged.is_empty() {
        let reason = "no usable config lines supplied".to_string();
        vd.line(format!("{step_label}: {reason}; using default config"));
        return KconfigStage {
            file: None,
            lines: Vec::new(),
            usage: TokenUsage::default(),
            wall: Duration::from_millis(0),
            error: Some(reason),
        };
    }

    match write_fragment(&merged) {
        Ok(file) => {
            vd.line(format!(
                "{step_label}: wrote {} user-supplied option(s) to {} ({})",
                merged.len(),
                file.path().display(),
                merged.join(", "),
            ));
            KconfigStage {
                file: Some(file),
                lines: merged,
                usage: TokenUsage::default(),
                wall: Duration::from_millis(0),
                error: None,
            }
        }
        Err(e) => {
            let reason = format!("write tempfile: {e:#}");
            vd.line(format!(
                "{step_label}: {reason}; falling back to default config"
            ));
            KconfigStage {
                file: None,
                lines: merged,
                usage: TokenUsage::default(),
                wall: Duration::from_millis(0),
                error: Some(reason),
            }
        }
    }
}

/// Find kselftest config files relevant to the changed paths and return their `CONFIG_*` lines
/// (validated through [`line_is_kconfig_entry`] so a malformed file can't smuggle anything into the
/// fragment).
///
/// For every changed path under `tools/testing/selftests/<area>/...` we walk up the directory tree
/// — collecting `config` siblings at each level — until we hit `tools/testing/selftests/` itself
/// (which has no useful "test config" of its own). This handles both flat layouts
/// (`tools/testing/selftests/sched_ext/config`) and nested ones
/// (`tools/testing/selftests/net/forwarding/config`).
///
/// Best-effort: missing files are skipped silently. Returns lines in the order we discover them
/// (innermost directory first), deduplicated by exact string. Order matters when the result is
/// later merged with the model's lines and written to a fragment: later occurrences of the same
/// `CONFIG_X=` win in `merge_config.sh`-style merging, so callers should append kselftest lines
/// after model lines if they want kselftest requirements to override model picks.
pub fn kselftest_config_lines(repo_root: &Path, changed_paths: &[String]) -> Vec<String> {
    const PREFIX: &str = "tools/testing/selftests/";
    let mut starts: Vec<std::path::PathBuf> = Vec::new();
    for p in changed_paths {
        if !p.starts_with(PREFIX) {
            continue;
        }
        if let Some(parent) = Path::new(p).parent() {
            starts.push(parent.to_path_buf());
        }
    }
    read_configs_walking_up(repo_root, &starts)
}

/// Like [`kselftest_config_lines`], but for kselftest areas the model named explicitly
/// (e.g. `["sched_ext"]` for a patch that touches kernel/sched/ext.c — the changed files are not
/// under `tools/testing/selftests/`, so the walk-from-changed-paths variant wouldn't find them).
///
/// Each `area` is interpreted as a path under `tools/testing/selftests/`; we walk up from there
/// looking for `config` siblings, same as for changed paths. Areas that fail
/// [`validate_kselftest_area`] are dropped before we touch the filesystem; areas whose `config`
/// file doesn't exist are silently skipped.
pub fn kselftest_config_lines_for_areas(repo_root: &Path, areas: &[String]) -> Vec<String> {
    const PREFIX: &str = "tools/testing/selftests/";
    let mut starts: Vec<std::path::PathBuf> = Vec::new();
    for area in areas {
        let trimmed = area.trim().trim_end_matches('/');
        if trimmed.is_empty() || !validate_kselftest_area(trimmed) {
            continue;
        }
        starts.push(Path::new(PREFIX).join(trimmed));
    }
    read_configs_walking_up(repo_root, &starts)
}

/// Walk each starting directory upward, collecting `config` files at every level until we hit
/// `tools/testing/selftests/` itself. Read each found file (relative to `repo_root`) and return
/// the validated, deduplicated `CONFIG_*` lines.
fn read_configs_walking_up(repo_root: &Path, starts: &[std::path::PathBuf]) -> Vec<String> {
    const PREFIX: &str = "tools/testing/selftests/";
    let mut configs_to_read: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    for start in starts {
        let mut cur: Option<&Path> = Some(start.as_path());
        while let Some(dir) = cur {
            let s = dir.to_string_lossy();
            let Some(rest) = s.strip_prefix(PREFIX) else {
                break;
            };
            if rest.is_empty() {
                break;
            }
            configs_to_read.insert(dir.join("config"));
            cur = dir.parent();
        }
    }

    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cfg in configs_to_read {
        let path = repo_root.join(&cfg);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let trimmed = line.trim();
            if line_is_kconfig_entry(trimmed) && seen.insert(trimmed.to_string()) {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}

/// Ask the model which `CONFIG_*` options should be set for `diff` and write a fragment to a
/// tempfile. When `changed_paths` includes files under `tools/testing/selftests/`, the matching
/// kselftest `config` file(s) are also read and merged in (server-side, never going through the
/// model) so the kernel build always satisfies the test's requirements even when the model misses
/// them. Always returns a `KconfigStage` so the caller can record token / wall / error in
/// `usage_steps`; `file` is `None` only when neither the model nor the kselftest configs produced
/// any usable lines (in which case the caller falls back to running `vng -b` without `--config`).
#[allow(clippy::too_many_arguments)]
pub async fn generate_fragment(
    client: &reqwest::Client,
    model: &ResolvedModel,
    diff: &str,
    changed_paths: &[String],
    step_label: &str,
    vd: &VerboseDest,
    worker_ctx: Option<&WorkerLineCtx>,
    effective_repo: &Path,
) -> KconfigStage {
    if let Some(w) = worker_ctx {
        w.set_line_message(step_label.to_string());
    }
    let t = Instant::now();
    let res = api::chat_completion(
        client,
        model,
        api::SYSTEM_CONFIG_FRAGMENT,
        diff,
        None,
        Some(step_label),
        None,
        vd,
        None,
        worker_ctx,
        effective_repo,
    )
    .await;
    let wall = t.elapsed();

    // Kselftest lines come from on-disk files in the per-commit worktree. We collect from two
    // independent signals and merge them later:
    //   1. Configs whose owning kselftest is ITSELF in the patch (paths under
    //      `tools/testing/selftests/<area>/...`).
    //   2. Configs the MODEL named as exercising the change (e.g. `kernel/sched/ext.c` patch →
    //      area "sched_ext"). Computed below after we have the parsed reply.
    let kselftest_lines_from_paths = kselftest_config_lines(effective_repo, changed_paths);
    if !kselftest_lines_from_paths.is_empty() {
        vd.line(format!(
            "{step_label}: discovered {} kselftest config line(s) under tools/testing/selftests/ via changed paths",
            kselftest_lines_from_paths.len()
        ));
    }

    let (model_lines, model_areas, usage, model_error): (
        Vec<String>,
        Vec<String>,
        TokenUsage,
        Option<String>,
    ) = match res {
        Ok((text, usage)) => {
            if let Some(w) = worker_ctx {
                w.record_tokens(
                    usage.prompt,
                    usage.completion,
                    usage.cache_creation,
                    usage.cache_read,
                );
            }
            match parse_lines(&text) {
                Ok(parsed) => (parsed.config_lines, parsed.kselftest_areas, usage, None),
                Err(e) => {
                    let reason = format!("parse model output: {e:#}");
                    vd.line(format!(
                        "{step_label}: {reason}; using kselftest lines only (if any)"
                    ));
                    (Vec::new(), Vec::new(), usage, Some(reason))
                }
            }
        }
        Err(e) => {
            let reason = api::short_error_reason(&e);
            vd.line(format!(
                "{step_label}: model call failed ({reason}); using kselftest lines only (if any)"
            ));
            (Vec::new(), Vec::new(), TokenUsage::default(), Some(reason))
        }
    };

    let kselftest_lines_from_areas = kselftest_config_lines_for_areas(effective_repo, &model_areas);
    if !kselftest_lines_from_areas.is_empty() {
        vd.line(format!(
            "{step_label}: model named kselftest area(s) [{}]; merged {} config line(s)",
            model_areas.join(", "),
            kselftest_lines_from_areas.len(),
        ));
    } else if !model_areas.is_empty() {
        vd.line(format!(
            "{step_label}: model named kselftest area(s) [{}] but none had a readable config under tools/testing/selftests/",
            model_areas.join(", "),
        ));
    }

    // Merge: model `CONFIG_*` lines first, then kselftest lines (path-discovered, then
    // area-discovered), deduped by exact string. Appending kselftest lines last means a
    // `CONFIG_X=y` from a kselftest config wins over a `# CONFIG_X is not set` from the model in
    // `merge_config.sh`-style consumers.
    let merged_capacity =
        model_lines.len() + kselftest_lines_from_paths.len() + kselftest_lines_from_areas.len();
    let mut merged: Vec<String> = Vec::with_capacity(merged_capacity);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for l in model_lines
        .into_iter()
        .chain(kselftest_lines_from_paths.into_iter())
        .chain(kselftest_lines_from_areas.into_iter())
    {
        if seen.insert(l.clone()) {
            merged.push(l);
        }
    }

    if merged.is_empty() {
        vd.line(format!(
            "{step_label}: no usable config lines from model or kselftests; using default config"
        ));
        return KconfigStage {
            file: None,
            lines: Vec::new(),
            usage,
            wall,
            error: model_error,
        };
    }

    match write_fragment(&merged) {
        Ok(file) => {
            vd.line(format!(
                "{step_label}: wrote {} option(s) to {} ({})",
                merged.len(),
                file.path().display(),
                merged.join(", "),
            ));
            KconfigStage {
                file: Some(file),
                lines: merged,
                usage,
                wall,
                error: model_error,
            }
        }
        Err(e) => {
            let reason = format!("write tempfile: {e:#}");
            vd.line(format!(
                "{step_label}: {reason}; falling back to default config"
            ));
            KconfigStage {
                file: None,
                lines: merged,
                usage,
                wall,
                error: Some(reason),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_y_m_n_and_disable_form() {
        assert!(line_is_kconfig_entry("CONFIG_FOO=y"));
        assert!(line_is_kconfig_entry("CONFIG_BAR_BAZ=m"));
        assert!(line_is_kconfig_entry("CONFIG_X_2=n"));
        assert!(line_is_kconfig_entry("# CONFIG_FOO is not set"));
    }

    #[test]
    fn rejects_numeric_and_string_values() {
        assert!(!line_is_kconfig_entry("CONFIG_HZ=250"));
        assert!(!line_is_kconfig_entry("CONFIG_HOSTNAME=\"x\""));
        assert!(!line_is_kconfig_entry("CONFIG_FOO="));
        assert!(!line_is_kconfig_entry("CONFIG_FOO=Y"));
    }

    #[test]
    fn rejects_non_config_lines() {
        assert!(!line_is_kconfig_entry(""));
        assert!(!line_is_kconfig_entry("FOO=y"));
        assert!(!line_is_kconfig_entry("rm -rf /"));
        assert!(!line_is_kconfig_entry("# random comment"));
        assert!(!line_is_kconfig_entry("CONFIG_FOO BAR=y"));
    }

    #[test]
    fn parse_lines_strips_invalid_entries_and_dedups() {
        let raw = r##"{
            "config": [
                "CONFIG_FOO=y",
                "CONFIG_FOO=y",
                "CONFIG_HZ=250",
                "garbage",
                "  CONFIG_BAR=m  ",
                "# CONFIG_BAZ is not set"
            ],
            "rationale": "..."
        }"##;
        let parsed = parse_lines(raw).unwrap();
        assert_eq!(
            parsed.config_lines,
            vec!["CONFIG_FOO=y", "CONFIG_BAR=m", "# CONFIG_BAZ is not set"],
        );
        assert!(parsed.kselftest_areas.is_empty());
    }

    #[test]
    fn parse_lines_handles_empty_array() {
        let raw = r#"{"config": [], "rationale": "no Kconfig-gated changes"}"#;
        let parsed = parse_lines(raw).unwrap();
        assert!(parsed.config_lines.is_empty());
        assert!(parsed.kselftest_areas.is_empty());
    }

    #[test]
    fn parse_lines_errors_when_top_level_key_missing() {
        let raw = r#"{"options": ["CONFIG_FOO=y"]}"#;
        assert!(parse_lines(raw).is_err());
    }

    #[test]
    fn parse_lines_extracts_kselftest_areas() {
        let raw = r##"{
            "config": ["CONFIG_SCHED_CLASS_EXT=y"],
            "kselftests": ["sched_ext", "net/forwarding", "sched_ext", ""],
            "rationale": "..."
        }"##;
        let parsed = parse_lines(raw).unwrap();
        assert_eq!(parsed.config_lines, vec!["CONFIG_SCHED_CLASS_EXT=y"]);
        // Duplicates and empties dropped; order preserved.
        assert_eq!(
            parsed.kselftest_areas,
            vec!["sched_ext".to_string(), "net/forwarding".to_string()]
        );
    }

    #[test]
    fn parse_lines_rejects_path_traversal_in_kselftest_areas() {
        let raw = r##"{
            "config": [],
            "kselftests": ["../etc", "/abs", "ok_area", "bad..segment", "a/../b", "good/area"]
        }"##;
        let parsed = parse_lines(raw).unwrap();
        // `../etc` → rejected (starts with `..` which is not allowed in segments).
        // `/abs` → rejected (leading slash; we only trim trailing slashes).
        // `bad..segment` → rejected (the validator's allowed-char set excludes `.`).
        // `a/../b` → rejected (has a `..` segment).
        assert_eq!(
            parsed.kselftest_areas,
            vec!["ok_area".to_string(), "good/area".to_string()]
        );
    }

    #[test]
    fn validate_kselftest_area_rules() {
        assert!(validate_kselftest_area("sched_ext"));
        assert!(validate_kselftest_area("net/forwarding"));
        assert!(validate_kselftest_area("a-b/c_d"));
        assert!(!validate_kselftest_area(""));
        assert!(!validate_kselftest_area("/abs"));
        assert!(!validate_kselftest_area(".."));
        assert!(!validate_kselftest_area("a/.."));
        assert!(!validate_kselftest_area("../foo"));
        assert!(!validate_kselftest_area("a b")); // space is not allowed
        assert!(!validate_kselftest_area("a;b")); // shell metacharacter
        assert!(!validate_kselftest_area("a.b")); // `.` is not in the allowed char set
    }

    #[test]
    fn fragment_from_lines_filters_and_dedups_user_lines() {
        let stage = fragment_from_lines(
            vec![
                " CONFIG_FOO=y ".to_string(),
                "CONFIG_FOO=y".to_string(),
                "CONFIG_NR_CPUS=512".to_string(),
                "CONFIG_CMDLINE=\"console=ttyS0 root=/dev/vda\"".to_string(),
                "CONFIG_EMPTY=".to_string(),
                "# CONFIG_BAR is not set".to_string(),
            ],
            "test",
            &VerboseDest::new(false),
        );
        assert_eq!(
            stage.lines,
            vec![
                "CONFIG_FOO=y".to_string(),
                "CONFIG_NR_CPUS=512".to_string(),
                "CONFIG_CMDLINE=\"console=ttyS0 root=/dev/vda\"".to_string(),
                "# CONFIG_BAR is not set".to_string()
            ]
        );
        assert!(stage.file.is_some());
        assert!(stage.error.is_none());
    }

    /// Lay out a fake `tools/testing/selftests/<area>/...` tree under `root` and write `text`
    /// at `<area>/config`. Used by the kselftest discovery tests to keep them hermetic.
    fn write_test_config(root: &Path, area_rel: &str, text: &str) {
        let dir = root.join("tools/testing/selftests").join(area_rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), text).unwrap();
    }

    #[test]
    fn kselftest_returns_empty_when_no_paths_under_selftests() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = kselftest_config_lines(
            tmp.path(),
            &[
                "kernel/sched/core.c".to_string(),
                "fs/ext4/super.c".to_string(),
            ],
        );
        assert!(lines.is_empty());
    }

    #[test]
    fn kselftest_reads_flat_area_config() {
        let tmp = tempfile::tempdir().unwrap();
        write_test_config(
            tmp.path(),
            "sched_ext",
            "CONFIG_SCHED_CLASS_EXT=y\nCONFIG_BPF_SYSCALL=y\n# stray comment\n",
        );
        let lines = kselftest_config_lines(
            tmp.path(),
            &["tools/testing/selftests/sched_ext/runner.c".to_string()],
        );
        assert_eq!(
            lines,
            vec![
                "CONFIG_SCHED_CLASS_EXT=y".to_string(),
                "CONFIG_BPF_SYSCALL=y".to_string(),
            ]
        );
    }

    #[test]
    fn kselftest_walks_nested_directories() {
        let tmp = tempfile::tempdir().unwrap();
        write_test_config(tmp.path(), "net", "CONFIG_NET=y\n");
        write_test_config(tmp.path(), "net/forwarding", "CONFIG_BRIDGE=m\n");
        let lines = kselftest_config_lines(
            tmp.path(),
            &["tools/testing/selftests/net/forwarding/router.sh".to_string()],
        );
        // Innermost first (BTreeSet order — `net/forwarding/config` sorts after `net/config`,
        // so `net/config` is read first; both must be present).
        assert!(lines.iter().any(|l| l == "CONFIG_BRIDGE=m"));
        assert!(lines.iter().any(|l| l == "CONFIG_NET=y"));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn kselftest_dedups_across_multiple_changed_paths() {
        let tmp = tempfile::tempdir().unwrap();
        write_test_config(tmp.path(), "kvm", "CONFIG_KVM=y\nCONFIG_KVM_INTEL=y\n");
        let lines = kselftest_config_lines(
            tmp.path(),
            &[
                "tools/testing/selftests/kvm/x86_64/foo.c".to_string(),
                "tools/testing/selftests/kvm/aarch64/bar.c".to_string(),
            ],
        );
        // Only one config file matters here (the area-level one), and we shouldn't read it twice.
        assert_eq!(
            lines,
            vec!["CONFIG_KVM=y".to_string(), "CONFIG_KVM_INTEL=y".to_string(),]
        );
    }

    #[test]
    fn kselftest_skips_invalid_lines() {
        let tmp = tempfile::tempdir().unwrap();
        write_test_config(
            tmp.path(),
            "bpf",
            // Mix of valid + bogus + comments. Only the two whitelisted forms survive.
            "CONFIG_BPF=y\nrandom garbage\nCONFIG_BPF_JIT=invalid\n# CONFIG_BPF_LSM is not set\n# friendly comment\n",
        );
        let lines = kselftest_config_lines(
            tmp.path(),
            &["tools/testing/selftests/bpf/test.c".to_string()],
        );
        assert_eq!(
            lines,
            vec![
                "CONFIG_BPF=y".to_string(),
                "# CONFIG_BPF_LSM is not set".to_string(),
            ]
        );
    }

    #[test]
    fn kselftest_stops_at_selftests_root() {
        let tmp = tempfile::tempdir().unwrap();
        // A `config` file directly inside `tools/testing/selftests/` would be the kselftests
        // top-level Makefile-adjacent config — not a per-test config, so we deliberately skip it.
        std::fs::create_dir_all(tmp.path().join("tools/testing/selftests")).unwrap();
        std::fs::write(
            tmp.path().join("tools/testing/selftests/config"),
            "CONFIG_TOPLEVEL=y\n",
        )
        .unwrap();
        write_test_config(tmp.path(), "area", "CONFIG_AREA=y\n");
        let lines = kselftest_config_lines(
            tmp.path(),
            &["tools/testing/selftests/area/foo.c".to_string()],
        );
        assert_eq!(lines, vec!["CONFIG_AREA=y".to_string()]);
        assert!(!lines.iter().any(|l| l.contains("CONFIG_TOPLEVEL")));
    }

    #[test]
    fn kselftest_areas_resolve_to_config_when_patch_is_outside_selftests() {
        let tmp = tempfile::tempdir().unwrap();
        write_test_config(
            tmp.path(),
            "sched_ext",
            "CONFIG_SCHED_CLASS_EXT=y\nCONFIG_BPF_SYSCALL=y\n",
        );
        // Simulates the real-world case: the patch only touches kernel/sched/ext.c, but the
        // model named "sched_ext" as the corresponding kselftest area.
        let lines = kselftest_config_lines_for_areas(tmp.path(), &["sched_ext".to_string()]);
        assert_eq!(
            lines,
            vec![
                "CONFIG_SCHED_CLASS_EXT=y".to_string(),
                "CONFIG_BPF_SYSCALL=y".to_string()
            ]
        );
    }

    #[test]
    fn kselftest_areas_resolve_nested_paths_walking_up() {
        let tmp = tempfile::tempdir().unwrap();
        write_test_config(tmp.path(), "net", "CONFIG_NET=y\n");
        write_test_config(tmp.path(), "net/forwarding", "CONFIG_BRIDGE=m\n");
        let lines = kselftest_config_lines_for_areas(tmp.path(), &["net/forwarding".to_string()]);
        assert!(lines.iter().any(|l| l == "CONFIG_BRIDGE=m"));
        assert!(lines.iter().any(|l| l == "CONFIG_NET=y"));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn kselftest_areas_silently_skip_unknown_area_names() {
        let tmp = tempfile::tempdir().unwrap();
        write_test_config(tmp.path(), "real_area", "CONFIG_REAL=y\n");
        let lines = kselftest_config_lines_for_areas(
            tmp.path(),
            &[
                "real_area".to_string(),
                "made_up_area".to_string(),
                "../etc".to_string(),        // rejected by validator
                "real_area/sub".to_string(), // dir doesn't exist; walks up to real_area/config
            ],
        );
        // CONFIG_REAL is read at most once (real_area is named directly AND is reached via the
        // walk-up from the unknown subdir).
        assert_eq!(lines, vec!["CONFIG_REAL=y".to_string()]);
    }
}
