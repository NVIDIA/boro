// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Upstream follow-up retrieval via `lei q -I https://lore.kernel.org/all/`.
//!
//! Runs a single deterministic, stateless lei query per commit (no `lei add-external`,
//! no on-disk mirror). The bare-phrase query catches references to the patch's subject
//! anywhere a public-inbox message indexes - including `Fixes: <sha> ("<subject>")` tags
//! in the bodies of later patches.
//!
//! Caller in `main.rs` invokes the per-commit follow-up stage which either short-circuits
//! (empty mbox) or hands the mbox to the model with the `stage-00b-upstream-followup.md`
//! prompt for structured extraction. The rendered summary then flows into
//! `prompts::build_reference_context` for every downstream discovery stage.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::process::Command;

/// Default upstream branch repository, used to find commits with a `Fixes:`
/// trailer for a patch being reviewed. It is fetched directly into
/// `FETCH_HEAD`, without changing configured remotes.
pub const UPSTREAM_MASTER_BRANCH_URL: &str =
    "git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git";
const UPSTREAM_MASTER_REF: &str = "FETCH_HEAD";

#[derive(Debug)]
pub struct MasterRepo {
    repo: PathBuf,
    applied_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterFix {
    pub sha: String,
    pub subject: String,
    pub date: String,
}

/// Result of a lei fetch for a single commit. `mbox` has noisy headers stripped and is
/// already truncated to at most `BORO_LORE_MAX_BYTES` bytes (whole-message boundaries).
#[derive(Debug, Clone)]
pub struct LoreMboxResult {
    pub mbox: String,
    pub hit_count: u32,
    pub query: String,
}

/// Resolved config for the lore follow-up stage. Populated from `BORO_LORE_*` env vars.
#[derive(Debug, Clone)]
pub struct LoreConfig {
    pub enabled: bool,
    pub window: String,
    pub max_bytes: usize,
    pub inbox_url: String,
}

impl LoreConfig {
    pub fn from_env() -> Self {
        let enabled = std::env::var("BORO_LORE_ENABLED")
            .ok()
            .map(|s| !matches!(s.trim(), "0" | "false" | "no" | ""))
            .unwrap_or(true);
        let window = std::env::var("BORO_LORE_WINDOW")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "1.year.ago..".to_string());
        let max_bytes = std::env::var("BORO_LORE_MAX_BYTES")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(65_536);
        let inbox_url = std::env::var("BORO_LORE_INBOX_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "https://lore.kernel.org/all/".to_string());
        Self {
            enabled,
            window,
            max_bytes,
            inbox_url,
        }
    }
}

/// Fetch the selected upstream branch directly into Git's transient
/// `FETCH_HEAD`, without adding or changing any configured remote.
pub async fn prepare_master_fetch(
    repo: &Path,
    upstream_uri: &str,
    upstream_branch: &str,
    applied_ref: Option<&str>,
) -> Result<MasterRepo> {
    let fetch = Command::new("git")
        .current_dir(repo)
        .args(["fetch", "--no-tags", upstream_uri, upstream_branch])
        .output()
        .await
        .context("fetch selected upstream branch")?;
    if !fetch.status.success() {
        anyhow::bail!(
            "git fetch of selected upstream branch failed: {}",
            String::from_utf8_lossy(&fetch.stderr).trim()
        );
    }
    Ok(MasterRepo {
        repo: repo.to_path_buf(),
        applied_ref: applied_ref.map(str::to_string),
    })
}

/// Return upstream commits whose `Fixes:` trailer names `sha`, excluding fixes
/// already applied at the review range tip. The kernel's documented convention
/// uses the first 12 hex digits; longer hashes are intentionally reduced to
/// that canonical lookup form.
pub async fn find_master_fixes(master: &MasterRepo, sha: &str) -> Result<Vec<MasterFix>> {
    let prefix: String = sha.chars().take(12).collect();
    if prefix.len() < 7 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Vec::new());
    }
    let needle = format!("Fixes: {prefix}");
    let output = Command::new("git")
        .current_dir(&master.repo)
        .args([
            "log",
            UPSTREAM_MASTER_REF,
            "--regexp-ignore-case",
            "--fixed-strings",
            "--grep",
            &needle,
            "--format=%H%x1f%s%x1f%cI",
        ])
        .output()
        .await
        .context("query Fixes trailers in selected upstream branch")?;
    if !output.status.success() {
        anyhow::bail!(
            "git log against selected upstream branch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let fixes: Vec<MasterFix> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\x1f');
            Some(MasterFix {
                sha: fields.next()?.to_string(),
                subject: fields.next()?.to_string(),
                date: fields.next()?.to_string(),
            })
        })
        .collect();
    if let Some(applied_ref) = master.applied_ref.as_deref() {
        filter_unapplied_master_fixes(master, &prefix, applied_ref, fixes).await
    } else {
        Ok(fixes)
    }
}

async fn filter_unapplied_master_fixes(
    master: &MasterRepo,
    reviewed_prefix: &str,
    applied_ref: &str,
    fixes: Vec<MasterFix>,
) -> Result<Vec<MasterFix>> {
    if fixes.is_empty() {
        return Ok(fixes);
    }
    let needle = format!("Fixes: {reviewed_prefix}");
    let output = Command::new("git")
        .current_dir(&master.repo)
        .args([
            "log",
            applied_ref,
            "--regexp-ignore-case",
            "--fixed-strings",
            "--grep",
            &needle,
            "--format=%H%x1f%s",
        ])
        .output()
        .await
        .with_context(|| format!("query already-applied Fixes trailers in {applied_ref}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "git log against reviewed branch tip failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut applied_shas = HashSet::new();
    let mut applied_subjects = HashSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split('\x1f');
        if let Some(sha) = fields.next().filter(|s| !s.is_empty()) {
            applied_shas.insert(sha.to_string());
        }
        if let Some(subject) = fields.next().filter(|s| !s.is_empty()) {
            applied_subjects.insert(subject.to_string());
        }
    }

    Ok(fixes
        .into_iter()
        .filter(|fix| !applied_shas.contains(&fix.sha) && !applied_subjects.contains(&fix.subject))
        .collect())
}

pub fn render_master_fixes(fixes: &[MasterFix]) -> String {
    if fixes.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Follow-up fixes in configured upstream branch\n\n");
    for fix in fixes {
        out.push_str(&format!(
            "- [{}](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/commit/?id={}): {} ({})\n",
            fix.sha, fix.sha, fix.subject, fix.date
        ));
    }
    out
}

/// True when `lei` is installed and runnable on `$PATH`. Probed once per process via
/// `lei -h` and cached: a missing `lei` makes the upstream-followup stage silently
/// no-op for the whole run, so the review proceeds with local git information only.
pub fn lei_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("lei")
            .arg("-h")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

/// Strip `[PATCH ...]` / `[RFC ...]` / nested bracketed prefixes and leading `Re:` from a subject
/// so the lei phrase match works against followups whose subjects look like `Re: <bare>` or v-bumps
/// that strip the bracket altogether. Returns the trimmed bare subject.
pub fn strip_patch_prefix(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let prev_len = s.len();
        if let Some(rest) = s.strip_prefix("Re:").or_else(|| s.strip_prefix("re:")) {
            s = rest.trim_start();
        }
        if s.starts_with('[') {
            if let Some(close) = s.find(']') {
                s = s[close + 1..].trim_start();
            }
        }
        if s.len() == prev_len {
            break;
        }
    }
    s.trim().to_string()
}

/// Mbox `From ` line delimiter at column 0 - counts as one message per occurrence.
pub fn count_from_lines(mbox: &str) -> u32 {
    let mut n = 0u32;
    for line in mbox.lines() {
        if line.starts_with("From ") {
            n += 1;
        }
    }
    n
}

/// Keep whole messages from an mboxo stream until the next message would push the total
/// byte count past `max_bytes`. The first message is always included (even if it alone
/// exceeds the budget), so the function never returns empty for an input that contains at
/// least one `^From ` boundary - this guarantees the model sees the originating patch
/// when its replies happen to be very chatty. Splits at `^From ` boundaries; never cuts
/// mid-message. Lines preceding the first `From ` are dropped (matching the previous
/// count-based truncator).
pub fn truncate_mbox_to_bytes(mbox: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let mut messages: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_message = false;
    for line in mbox.lines() {
        if line.starts_with("From ") {
            if in_message && !current.is_empty() {
                messages.push(std::mem::take(&mut current));
            }
            in_message = true;
        }
        if in_message {
            current.push_str(line);
            current.push('\n');
        }
    }
    if in_message && !current.is_empty() {
        messages.push(current);
    }

    let mut iter = messages.into_iter();
    let mut out = match iter.next() {
        Some(first) => first,
        None => return String::new(),
    };
    for msg in iter {
        if out.len() + msg.len() > max_bytes {
            break;
        }
        out.push_str(&msg);
    }
    out
}

/// Drop the high-volume noise headers (`Received:`, DKIM/ARC, `X-*`, `Authentication-Results:`,
/// `Delivered-To:`, `Return-Path:`) while keeping every header the follow-up stage needs to
/// correlate messages (`From:`, `Date:`, `Subject:`, `Message-Id:`, `In-Reply-To:`,
/// `References:`, `List-Id:`) and every line of every body intact. Operates on full mboxo;
/// the header/body boundary inside each message is the first blank line.
pub fn strip_noisy_headers(mbox: &str) -> String {
    let drop_prefixes: &[&str] = &[
        "Received:",
        "Return-Path:",
        "Delivered-To:",
        "DKIM-Signature:",
        "ARC-Seal:",
        "ARC-Message-Signature:",
        "ARC-Authentication-Results:",
        "Authentication-Results:",
        "X-",
        "Precedence:",
        "List-Help:",
        "List-Post:",
        "List-Subscribe:",
        "List-Unsubscribe:",
        "List-Archive:",
        "List-Owner:",
        "Mailing-List:",
    ];
    let mut out = String::with_capacity(mbox.len());
    let mut in_headers = false;
    let mut skipping_continuation = false;
    for line in mbox.lines() {
        if line.starts_with("From ") {
            in_headers = true;
            skipping_continuation = false;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_headers {
            if line.is_empty() {
                in_headers = false;
                skipping_continuation = false;
                out.push('\n');
                continue;
            }
            // Continuation line of the previous header? RFC 5322 folds with leading whitespace.
            if line.starts_with(' ') || line.starts_with('\t') {
                if skipping_continuation {
                    continue;
                }
                out.push_str(line);
                out.push('\n');
                continue;
            }
            // New header. Decide whether to drop.
            if drop_prefixes.iter().any(|p| {
                line.starts_with(p)
                    || line
                        .to_ascii_lowercase()
                        .starts_with(&p.to_ascii_lowercase())
            }) {
                skipping_continuation = true;
                continue;
            }
            skipping_continuation = false;
            out.push_str(line);
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Build the lei query string. Bare quoted phrase (multi-field match) catches `Fixes:` body
/// references in later patches, v2/v3 cover letters, and the `Re:` thread on the original send.
fn build_query(stripped_subject: &str, window: &str) -> String {
    // Escape any embedded double-quotes in the subject (rare) by stripping them - public-inbox
    // queries don't have a robust escape mechanism inside phrase quoting, so we keep it simple.
    let safe = stripped_subject.replace('"', "");
    format!(r#""{safe}" AND rt:{window}"#)
}

/// Run lei against the configured remote inbox and return the (cleaned, truncated) mbox plus a
/// hit count. Empty result is not an error - it represents the common "no upstream activity" case.
pub async fn fetch_upstream_mbox(subject: &str, cfg: &LoreConfig) -> Result<LoreMboxResult> {
    let stripped = strip_patch_prefix(subject);
    if stripped.is_empty() {
        return Ok(LoreMboxResult {
            mbox: String::new(),
            hit_count: 0,
            query: String::new(),
        });
    }
    let query = build_query(&stripped, &cfg.window);
    let output = Command::new("lei")
        .args([
            "q",
            "-I",
            &cfg.inbox_url,
            "-f",
            "mboxo",
            "--threads",
            "-d",
            "mid",
            "--no-save",
            "-o",
            "-",
            "--",
            &query,
        ])
        .output()
        .await
        .context("spawning `lei q` for upstream follow-up retrieval")?;
    if !output.status.success() {
        anyhow::bail!(
            "lei q exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout).into_owned();
    let truncated = truncate_mbox_to_bytes(&raw, cfg.max_bytes);
    let cleaned = strip_noisy_headers(&truncated);
    let hit_count = count_from_lines(&cleaned);
    Ok(LoreMboxResult {
        mbox: cleaned,
        hit_count,
        query,
    })
}

/// JSON shape emitted directly when the lei result is empty - saves a model round-trip.
pub fn no_activity_json() -> Value {
    serde_json::json!({
        "followup_status": "no_upstream_activity",
        "is_superseded": false,
        "superseded_by": [],
        "fixes_of_this": [],
        "maintainer_concerns": [],
        "consensus_status": "no_followup",
        "key_observations": []
    })
}

fn is_unreserved_path_byte(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'@')
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        if is_unreserved_path_byte(b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    encoded
}

/// Build the public-inbox URL for a message-id. `mid` may include angle brackets; the URL form
/// drops them. `inbox_url` is the configured base (e.g. `https://lore.kernel.org/all/`); a
/// trailing slash is tolerated either way. Returns an empty string when the message-id is empty
/// or visibly malformed (whitespace-only).
pub fn lore_url(mid: &str, inbox_url: &str) -> String {
    let cleaned = mid
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if cleaned.is_empty() {
        return String::new();
    }
    let base = inbox_url.trim_end_matches('/');
    let encoded = encode_path_segment(cleaned);
    format!("{base}/{encoded}/")
}

/// Render the follow-up JSON (either model-emitted or the short-circuit literal) into the
/// compact Markdown section that gets appended to discovery-stage reference contexts.
/// Returns an empty string when there is nothing useful to surface.
///
/// `inbox_url` is used to mint `https://lore.kernel.org/all/<mid>/` citations next to every
/// entry that carries a `message_id`, so downstream review stages can copy the URL verbatim
/// when echoing an upstream concern.
pub fn render_followup_summary(v: &Value, inbox_url: &str) -> String {
    let status = v
        .get("followup_status")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if status == "no_upstream_activity" {
        return "## Upstream follow-up\nNo upstream follow-up found in lore (configured window).\n"
            .to_string();
    }
    if status == "all_hits_were_false_matches" {
        return "## Upstream follow-up\nlei query returned hits but none referenced this patch (all false matches).\n"
            .to_string();
    }
    let mut out = String::from("## Upstream follow-up summary\n\n");
    let consensus = v
        .get("consensus_status")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown");
    out.push_str(&format!("Consensus: {consensus}\n"));

    let is_superseded = v
        .get("is_superseded")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    if is_superseded {
        let by = v.get("superseded_by").and_then(|x| x.as_array());
        let entries: Vec<String> = by
            .map(|a| {
                a.iter()
                    .filter_map(|item| {
                        let mid = item
                            .get("message_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let ver = item.get("version").and_then(|x| x.as_str()).unwrap_or("?");
                        let date = item.get("date").and_then(|x| x.as_str()).unwrap_or("?");
                        let url = lore_url(mid, inbox_url);
                        if url.is_empty() {
                            Some(format!("  - {ver} on {date}"))
                        } else {
                            Some(format!("  - {ver} on {date} ({url})"))
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push_str("Superseded: yes\n");
        for e in entries {
            out.push_str(&e);
            out.push('\n');
        }
    }

    let fixes = v
        .get("fixes_of_this")
        .and_then(|x| x.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    if !fixes.is_empty() {
        out.push_str(&format!("Fixes-of-this: {} patch(es)\n", fixes.len()));
        for f in fixes {
            let sha = f.get("sha").and_then(|x| x.as_str()).unwrap_or("?");
            let subj = f.get("subject").and_then(|x| x.as_str()).unwrap_or("?");
            let summary = f.get("summary").and_then(|x| x.as_str()).unwrap_or("");
            let mid = f.get("message_id").and_then(|x| x.as_str()).unwrap_or("");
            let url = lore_url(mid, inbox_url);
            out.push_str(&format!("  - {sha}: {subj}"));
            if !summary.is_empty() {
                out.push_str(&format!(" - {summary}"));
            }
            if !url.is_empty() {
                out.push_str(&format!(" ({url})"));
            }
            out.push('\n');
        }
    }

    let concerns = v
        .get("maintainer_concerns")
        .and_then(|x| x.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    if !concerns.is_empty() {
        out.push_str("Maintainer concerns:\n");
        for c in concerns {
            let reviewer = c.get("reviewer").and_then(|x| x.as_str()).unwrap_or("?");
            let concern = c.get("concern").and_then(|x| x.as_str()).unwrap_or("?");
            let severity = c.get("severity").and_then(|x| x.as_str()).unwrap_or("?");
            let mid = c.get("message_id").and_then(|x| x.as_str()).unwrap_or("");
            let url = lore_url(mid, inbox_url);
            out.push_str(&format!("  - {reviewer} [{severity}]: {concern}"));
            if !url.is_empty() {
                out.push_str(&format!(" ({url})"));
            }
            out.push('\n');
        }
    }

    let obs = v
        .get("key_observations")
        .and_then(|x| x.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    if !obs.is_empty() {
        out.push_str("Key observations:\n");
        for o in obs {
            if let Some(s) = o.as_str() {
                out.push_str(&format!("  - {s}\n"));
            }
        }
    }

    out.push_str(
        "\nCitation rule: when you echo any of the maintainer concerns or follow-up entries above in your \
findings, include the parenthesised lore URL verbatim so the review report links back to the \
source thread (e.g. \"The upstream discussion (<url>) noted ...\").\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command as StdCommand;

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "git {} failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_git_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-b", "master"]);
        git(tmp.path(), &["config", "user.name", "Boro Test"]);
        git(tmp.path(), &["config", "user.email", "boro@example.com"]);
        tmp
    }

    fn empty_commit(repo: &Path, subject: &str, body: Option<&str>) -> String {
        let mut cmd = StdCommand::new("git");
        cmd.current_dir(repo)
            .args(["commit", "--allow-empty", "-m", subject]);
        if let Some(body) = body {
            cmd.args(["-m", body]);
        }
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("git commit failed to spawn: {e}"));
        assert!(
            out.status.success(),
            "git commit failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        git(repo, &["rev-parse", "HEAD"])
    }

    fn git_optional(repo: &Path, args: &[&str]) -> (i32, String, String) {
        let out = StdCommand::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    }

    #[test]
    fn strip_plain_subject() {
        assert_eq!(
            strip_patch_prefix("sch_htb: fix race in HTB drr"),
            "sch_htb: fix race in HTB drr"
        );
    }

    #[test]
    fn strip_single_patch_prefix() {
        assert_eq!(
            strip_patch_prefix("[PATCH] sch_htb: fix race"),
            "sch_htb: fix race"
        );
    }

    #[test]
    fn strip_versioned_patch_prefix() {
        assert_eq!(
            strip_patch_prefix("[PATCH v3 4/8 net-next] sch_htb: fix race in HTB drr"),
            "sch_htb: fix race in HTB drr"
        );
    }

    #[test]
    fn strip_rfc_patch_prefix() {
        assert_eq!(strip_patch_prefix("[RFC PATCH] foo: bar"), "foo: bar");
    }

    #[test]
    fn strip_reply_prefix() {
        assert_eq!(strip_patch_prefix("Re: [PATCH v2] foo: bar"), "foo: bar");
    }

    #[test]
    fn strip_multiple_bracketed_groups() {
        assert_eq!(
            strip_patch_prefix("[net-next] [PATCH v2] foo: bar"),
            "foo: bar"
        );
    }

    #[test]
    fn strip_handles_only_prefix() {
        assert_eq!(strip_patch_prefix("[PATCH]"), "");
        assert_eq!(strip_patch_prefix("Re: Re: [PATCH]"), "");
    }

    #[test]
    fn truncate_bytes_keeps_whole_messages_under_budget() {
        // Three messages of ~30 bytes each; budget of 100 should keep first two.
        let mbox =
            "From a@x\nFrom: a\n\nbody-a\n\nFrom b@x\nFrom: b\n\nbody-b\n\nFrom c@x\nFrom: c\n\nbody-c\n";
        let kept = truncate_mbox_to_bytes(mbox, 60);
        assert!(kept.contains("body-a"));
        assert!(kept.contains("body-b") || !kept.contains("body-c"));
        // The third message must not appear.
        assert!(!kept.contains("body-c"));
        // Output must not exceed the budget (whole-message boundaries respected).
        assert!(kept.len() <= mbox.len());
    }

    #[test]
    fn truncate_bytes_first_message_always_kept() {
        // First message is far bigger than the budget, but we still emit it whole.
        let big_body = "x".repeat(500);
        let mbox = format!("From a@x\nFrom: a\n\n{big_body}\n\nFrom b@x\nFrom: b\n\nbody-b\n");
        let kept = truncate_mbox_to_bytes(&mbox, 100);
        assert_eq!(count_from_lines(&kept), 1);
        assert!(kept.contains(&big_body));
        assert!(!kept.contains("body-b"));
    }

    #[test]
    fn truncate_bytes_under_budget_returns_all() {
        let mbox = "From a@x\nFrom: a\n\nbody-a\n\nFrom b@x\nFrom: b\n\nbody-b\n";
        let kept = truncate_mbox_to_bytes(mbox, 10_000);
        assert_eq!(count_from_lines(&kept), 2);
        assert!(kept.contains("body-a"));
        assert!(kept.contains("body-b"));
    }

    #[test]
    fn truncate_bytes_zero_returns_empty() {
        let mbox = "From a@x\nbody\n";
        assert!(truncate_mbox_to_bytes(mbox, 0).is_empty());
    }

    #[test]
    fn truncate_bytes_no_from_line_returns_empty() {
        let mbox = "no messages here\njust prose\n";
        assert!(truncate_mbox_to_bytes(mbox, 1_000).is_empty());
    }

    #[test]
    fn strip_noisy_headers_drops_received_and_dkim() {
        let mbox = "From a@x\n\
From: foo@example.com\n\
Received: from mx by relay; Tue\n\
Received: from internal by mx; Tue\n\
DKIM-Signature: a=rsa; b=xxx\n\
Date: Tue, 1 Jan 2026 00:00:00 +0000\n\
Subject: foo: bar\n\
Message-Id: <abc@x>\n\
X-Spam-Score: 0\n\
\n\
body line one\n\
body line two\n";
        let out = strip_noisy_headers(mbox);
        assert!(out.contains("From: foo@example.com"));
        assert!(out.contains("Subject: foo: bar"));
        assert!(out.contains("Message-Id: <abc@x>"));
        assert!(!out.contains("Received:"));
        assert!(!out.contains("DKIM-Signature:"));
        assert!(!out.contains("X-Spam-Score:"));
        assert!(out.contains("body line one"));
        assert!(out.contains("body line two"));
    }

    #[test]
    fn strip_noisy_headers_handles_folded_continuations() {
        let mbox = "From a@x\n\
DKIM-Signature: a=rsa;\n\
\tb=xxx;\n\
\tc=yyy\n\
Subject: keep me\n\
\n\
body\n";
        let out = strip_noisy_headers(mbox);
        assert!(out.contains("Subject: keep me"));
        assert!(!out.contains("DKIM-Signature"));
        assert!(!out.contains("b=xxx"));
        assert!(!out.contains("c=yyy"));
        assert!(out.contains("body"));
    }

    #[test]
    fn lore_config_defaults() {
        // Snapshot expected defaults when env is unset. (Test isolation: read each var
        // and skip if user has them set in the environment.)
        for k in [
            "BORO_LORE_ENABLED",
            "BORO_LORE_WINDOW",
            "BORO_LORE_MAX_BYTES",
            "BORO_LORE_INBOX_URL",
        ] {
            if std::env::var(k).is_ok() {
                return;
            }
        }
        let c = LoreConfig::from_env();
        assert!(c.enabled);
        assert_eq!(c.window, "1.year.ago..");
        assert_eq!(c.max_bytes, 65_536);
        assert_eq!(c.inbox_url, "https://lore.kernel.org/all/");
    }

    #[test]
    fn render_summary_no_activity() {
        let s = render_followup_summary(&no_activity_json(), "https://lore.kernel.org/all/");
        assert!(s.contains("No upstream follow-up found"));
    }

    #[test]
    fn render_summary_with_findings() {
        let v = serde_json::json!({
            "followup_status": "found_followups",
            "is_superseded": true,
            "superseded_by": [{"message_id":"<v2@x>","version":"v2","date":"2026-04-01"}],
            "fixes_of_this": [{"sha":"abc1234","subject":"net: foo: fix race","summary":"locking","message_id":"<fix@x>"}],
            "maintainer_concerns": [{"reviewer":"Eric <eric@x>","concern":"missing rcu_read_lock","severity":"high","message_id":"<concern@x>"}],
            "consensus_status": "under_discussion",
            "key_observations": ["v2 was rejected"]
        });
        let s = render_followup_summary(&v, "https://lore.kernel.org/all/");
        assert!(s.contains("Consensus: under_discussion"));
        assert!(s.contains("Superseded: yes"));
        assert!(s.contains("v2 on 2026-04-01 (https://lore.kernel.org/all/v2@x/)"));
        assert!(s.contains("abc1234: net: foo: fix race"));
        assert!(s.contains("https://lore.kernel.org/all/fix@x/"));
        assert!(s.contains("Eric <eric@x> [high]"));
        assert!(s.contains("https://lore.kernel.org/all/concern@x/"));
        assert!(s.contains("v2 was rejected"));
        assert!(s.contains("Citation rule"));
    }

    #[test]
    fn render_summary_omits_url_when_message_id_missing() {
        let v = serde_json::json!({
            "followup_status": "found_followups",
            "is_superseded": false,
            "superseded_by": [],
            "fixes_of_this": [],
            "maintainer_concerns": [{"reviewer":"x","concern":"y","severity":"low"}],
            "consensus_status": "under_discussion",
            "key_observations": []
        });
        let s = render_followup_summary(&v, "https://lore.kernel.org/all/");
        assert!(s.contains("x [low]: y"));
        assert!(!s.contains("(https://"));
    }

    #[test]
    fn lore_url_strips_angle_brackets() {
        assert_eq!(
            lore_url("<abc@example.com>", "https://lore.kernel.org/all/"),
            "https://lore.kernel.org/all/abc@example.com/"
        );
    }

    #[test]
    fn lore_url_handles_missing_brackets() {
        assert_eq!(
            lore_url("abc@example.com", "https://lore.kernel.org/all/"),
            "https://lore.kernel.org/all/abc@example.com/"
        );
    }

    #[test]
    fn lore_url_tolerates_trailing_slash_variations() {
        let with = lore_url("<x@y>", "https://lore.kernel.org/all/");
        let without = lore_url("<x@y>", "https://lore.kernel.org/all");
        assert_eq!(with, without);
        assert_eq!(with, "https://lore.kernel.org/all/x@y/");
    }

    #[test]
    fn lore_url_empty_when_no_message_id() {
        assert!(lore_url("", "https://lore.kernel.org/all/").is_empty());
        assert!(lore_url("  ", "https://lore.kernel.org/all/").is_empty());
        assert!(lore_url("<>", "https://lore.kernel.org/all/").is_empty());
    }

    #[test]
    fn lore_url_encodes_reserved_path_chars() {
        assert_eq!(
            lore_url("a+bc@example.com", "https://lore.kernel.org/all/"),
            "https://lore.kernel.org/all/a%2Bbc@example.com/"
        );
    }

    #[test]
    fn lore_url_encodes_percent_sign_and_spaces() {
        assert_eq!(
            lore_url("<a%b c@example.com>", "https://lore.kernel.org/all/"),
            "https://lore.kernel.org/all/a%25b%20c@example.com/"
        );
    }

    #[test]
    fn lore_url_encodes_slash() {
        assert_eq!(
            lore_url("<a/b@example.com>", "https://lore.kernel.org/all/"),
            "https://lore.kernel.org/all/a%2Fb@example.com/"
        );
    }

    #[test]
    fn render_master_fixes_includes_kernel_commit_links() {
        let fixes = vec![MasterFix {
            sha: "0123456789abcdef".to_string(),
            subject: "net: fix a race".to_string(),
            date: "2026-06-01T00:00:00+00:00".to_string(),
        }];
        let rendered = render_master_fixes(&fixes);
        assert!(rendered.contains("Follow-up fixes in configured upstream branch"));
        assert!(rendered.contains("net: fix a race"));
        assert!(rendered.contains("commit/?id=0123456789abcdef"));
    }

    #[tokio::test]
    async fn master_fixes_reported_when_not_applied_to_review_tip() {
        let tmp = init_git_repo();
        let repo = tmp.path();
        empty_commit(repo, "base", None);
        let reviewed = empty_commit(repo, "net: add bad change", None);
        let body = format!(
            "Fixes: {} (\"net: add bad change\")",
            reviewed.chars().take(12).collect::<String>()
        );
        let fix = empty_commit(repo, "net: follow-up fix", Some(&body));
        git(repo, &["fetch", ".", "master"]);

        let master = MasterRepo {
            repo: repo.to_path_buf(),
            applied_ref: Some(reviewed.clone()),
        };
        let fixes = find_master_fixes(&master, &reviewed).await.unwrap();

        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].sha, fix);
        assert_eq!(fixes[0].subject, "net: follow-up fix");
    }

    #[tokio::test]
    async fn prepare_master_fetch_does_not_leave_local_path_remote() {
        let tmp = init_git_repo();
        let repo = tmp.path();
        empty_commit(repo, "base", None);

        let _master = prepare_master_fetch(repo, ".", "master", None)
            .await
            .unwrap();
        let remotes = git(repo, &["remote"]);
        let (config_status, config, config_err) =
            git_optional(repo, &["config", "--get-regexp", "^remote\\."]);

        assert_eq!(remotes, "");
        assert_eq!(
            config_status, 1,
            "unexpected remote config after fetch:\nstdout:\n{config}\nstderr:\n{config_err}"
        );
    }

    #[tokio::test]
    async fn master_fixes_skipped_when_already_applied_to_review_tip() {
        let tmp = init_git_repo();
        let repo = tmp.path();
        empty_commit(repo, "base", None);
        let reviewed = empty_commit(repo, "net: add bad change", None);
        let body = format!(
            "Fixes: {} (\"net: add bad change\")",
            reviewed.chars().take(12).collect::<String>()
        );
        empty_commit(repo, "net: follow-up fix", Some(&body));
        git(repo, &["fetch", ".", "master"]);

        let master = MasterRepo {
            repo: repo.to_path_buf(),
            applied_ref: Some("HEAD".to_string()),
        };
        let fixes = find_master_fixes(&master, &reviewed).await.unwrap();

        assert!(fixes.is_empty());
    }

    #[tokio::test]
    async fn master_fixes_skipped_when_cherry_pick_subject_is_applied() {
        let tmp = init_git_repo();
        let repo = tmp.path();
        empty_commit(repo, "base", None);
        let reviewed = empty_commit(repo, "net: add bad change", None);
        let body = format!(
            "Fixes: {} (\"net: add bad change\")",
            reviewed.chars().take(12).collect::<String>()
        );
        let review_tip = empty_commit(repo, "net: follow-up fix", Some(&body));
        git(repo, &["branch", "reviewed", &review_tip]);
        git(repo, &["reset", "--hard", &reviewed]);
        empty_commit(repo, "net: follow-up fix", Some(&body));
        git(repo, &["fetch", ".", "master"]);

        let master = MasterRepo {
            repo: repo.to_path_buf(),
            applied_ref: Some("reviewed".to_string()),
        };
        let fixes = find_master_fixes(&master, &reviewed).await.unwrap();

        assert!(fixes.is_empty());
    }
}
