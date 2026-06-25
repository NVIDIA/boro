// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub fn normalize_commit_range_arg(range: &str) -> String {
    if is_explicit_commit_range(range) {
        range.to_string()
    } else {
        format!("{range}^..{range}")
    }
}

fn is_explicit_commit_range(range: &str) -> bool {
    range.contains("..")
        || range.starts_with('^')
        || range.ends_with("^!")
        || range.ends_with("^@")
        || range.contains("^-")
}

pub fn repo_root(repo: &Path) -> Result<std::path::PathBuf> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("git rev-parse")?;
    if !out.status.success() {
        anyhow::bail!("not a git repository: {}", repo.display());
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(Path::new(&s).to_path_buf())
}

pub fn rev_list(repo: &Path, range: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-list", "--reverse", range])
        .output()
        .with_context(|| format!("git rev-list {range}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Diff only (no commit log), for specialist stages (slim dynamic context).
pub fn show_patch_diff_only(repo: &Path, sha: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["show", "--format=", "-p", sha])
        .output()
        .with_context(|| format!("git show (diff-only) {sha}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git show (diff-only) {}: {}",
            sha,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn show_patch(repo: &Path, sha: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["show", "--pretty=medium", sha])
        .output()
        .with_context(|| format!("git show {sha}"))?;
    if !out.status.success() {
        anyhow::bail!("git show {}: {}", sha, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Single-line subject for `sha` (`git log -1 --format=%s`). Used to build the lei query for
/// the upstream-followup stage.
pub fn commit_subject(repo: &Path, sha: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["log", "-1", "--format=%s", sha])
        .output()
        .with_context(|| format!("git log -1 --format=%s {sha}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git log -1 --format=%s {}: {}",
            sha,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Subject + author + ISO-8601 author date + parent SHAs, fetched in a single
/// `git log -1` call. Used to populate the per-commit JSON object for `--json`
/// output (and the Ctrl-C snapshot dump).
#[derive(Clone, Debug, Default)]
pub struct CommitMeta {
    pub subject: String,
    pub author: String,
    pub date: String,
    pub parents: Vec<String>,
}

pub fn commit_metadata(repo: &Path, sha: &str) -> Result<CommitMeta> {
    // %s subject, %an author name, %ae author email, %aI ISO-8601-strict author date,
    // %P space-separated parent SHAs. Each on its own line via %n.
    let out = Command::new("git")
        .current_dir(repo)
        .args(["log", "-1", "--format=%s%n%an <%ae>%n%aI%n%P", sha])
        .output()
        .with_context(|| format!("git log -1 (metadata) {sha}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git log -1 (metadata) {}: {}",
            sha,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let subject = lines.next().unwrap_or("").to_string();
    let author = lines.next().unwrap_or("").to_string();
    let date = lines.next().unwrap_or("").to_string();
    let parents = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    Ok(CommitMeta {
        subject,
        author,
        date,
        parents,
    })
}

/// Commit headers only (no diff), for LKML pass context.
pub fn show_commit_headers(repo: &Path, sha: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["show", "-s", "--pretty=medium", sha])
        .output()
        .with_context(|| format!("git show -s {sha}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git show -s {}: {}",
            sha,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One-line subjects for each commit in `range`, oldest first (`git log --reverse --format=%s`).
/// Used for series awareness (other commit subjects) during consolidation.
pub fn log_subjects_in_range(repo: &Path, range: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["--no-pager", "log", "--reverse", "--format=%s", range])
        .output()
        .with_context(|| format!("git log --reverse --format=%s {range}"))?;
    if !out.status.success() {
        anyhow::bail!("git log {range}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn changed_paths(repo: &Path, sha: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", sha])
        .output()
        .context("git diff-tree")?;
    if !out.status.success() {
        anyhow::bail!("git diff-tree: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::normalize_commit_range_arg;

    #[test]
    fn normalizes_single_revision_to_one_commit_range() {
        assert_eq!(normalize_commit_range_arg("abc123"), "abc123^..abc123");
        assert_eq!(normalize_commit_range_arg("HEAD~2"), "HEAD~2^..HEAD~2");
        assert_eq!(normalize_commit_range_arg("HEAD^"), "HEAD^^..HEAD^");
    }

    #[test]
    fn leaves_explicit_ranges_unchanged() {
        assert_eq!(normalize_commit_range_arg("base..HEAD"), "base..HEAD");
        assert_eq!(normalize_commit_range_arg("base...HEAD"), "base...HEAD");
        assert_eq!(normalize_commit_range_arg("base.."), "base..");
        assert_eq!(normalize_commit_range_arg("..HEAD"), "..HEAD");
        assert_eq!(normalize_commit_range_arg("abc123^!"), "abc123^!");
        assert_eq!(normalize_commit_range_arg("abc123^-"), "abc123^-");
        assert_eq!(normalize_commit_range_arg("^abc123"), "^abc123");
    }
}
