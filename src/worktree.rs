// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-commit git worktrees so each parallel review sees the working tree
//! pinned to its commit's SHA, not the latest state of the main checkout.
//!
//! Layout: `<main_repo>/.boro/worktrees/<command>/<sha>/` — a `git worktree
//! add --detach` checkout of `<sha>`, where `<command>` is `review` / `build`
//! / `test`. Per-command subdirs let you run e.g. `boro build` and
//! `boro test` against the same commit at the same time without their
//! worktree paths colliding. The main repo is left untouched, so the user
//! can keep editing / building / switching branches there while reviews are
//! in flight.
//!
//! Cleanup: a [`Worktree`] is an RAII handle whose `Drop` runs
//! `git worktree remove --force`. Tokio aborts (e.g. on Ctrl-C) drop the
//! task's locals, so the same path runs on cancellation. A best-effort
//! [`sweep_stale`] runs once at startup to clean up orphans from prior
//! crashes — scoped to the current command's subdir so it doesn't disturb
//! a concurrent boro run that's using a different command.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::verbose::VerboseDest;

const WORKTREES_SUBDIR: &str = ".boro/worktrees";

/// RAII handle for a `git worktree add --detach <path> <sha>` checkout.
///
/// On drop, runs `git worktree remove --force` against the main repo
/// (best-effort: failures are logged, never panic).
pub struct Worktree {
    main_repo: PathBuf,
    path: PathBuf,
}

/// A posted patch series applied in a disposable worktree as input to `boro apply`.
///
/// The imported commits live in the source repository's object database while this guard keeps
/// the worktree registered. Dropping the guard removes the worktree without changing the user's
/// branch or working tree.
pub struct ImportedSeries {
    _worktree: Worktree,
    range: String,
}

fn stderr_excerpt(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .chars()
        .take(4096)
        .collect::<String>()
        .trim()
        .to_string()
}

impl ImportedSeries {
    pub async fn prepare(
        main_repo: &Path,
        message_id: &str,
        dest: &VerboseDest,
        interrupt: &mut tokio::signal::unix::Signal,
    ) -> Result<Self> {
        Self::prepare_with_program(main_repo, message_id, Path::new("b4"), dest, interrupt).await
    }

    async fn prepare_with_program(
        main_repo: &Path,
        message_id: &str,
        b4_program: &Path,
        dest: &VerboseDest,
        interrupt: &mut tokio::signal::unix::Signal,
    ) -> Result<Self> {
        let message_id = message_id.trim();
        if message_id.is_empty()
            || message_id.len() > 2048
            || message_id.chars().any(char::is_control)
        {
            anyhow::bail!("invalid Message-ID");
        }

        let base = crate::git::rev_parse_commit(main_repo, "HEAD")
            .context("resolve source HEAD before importing Message-ID")?;
        let worktree_dir = tempfile::Builder::new()
            .prefix("boro-message-id-")
            .tempdir()
            .context("create temporary directory for Message-ID import")?;
        let path = worktree_dir.path().to_path_buf();
        let cleanup_partial = |dir: tempfile::TempDir| {
            let _ = Command::new("git")
                .current_dir(main_repo)
                .args(["worktree", "remove", "--force"])
                .arg(&path)
                .output();
            drop(dir);
            let _ = Command::new("git")
                .current_dir(main_repo)
                .args(["worktree", "prune", "--expire=now"])
                .output();
        };

        let mut add_worktree = tokio::process::Command::new("git");
        add_worktree
            .current_dir(main_repo)
            .args(["worktree", "add", "--detach"])
            .arg(&path)
            .arg(&base);
        let out = match crate::process::cancellable_output(
            add_worktree,
            "git worktree add for Message-ID import",
            interrupt,
        )
        .await
        {
            Ok(out) => out,
            Err(error) => {
                cleanup_partial(worktree_dir);
                return Err(error);
            }
        };
        if !out.status.success() {
            let error = anyhow::anyhow!(
                "git worktree add {} {base}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            cleanup_partial(worktree_dir);
            return Err(error);
        }

        // From here on Worktree owns removal. Keeping the directory prevents TempDir from
        // deleting it behind Git's back if `git worktree remove` ever fails.
        let path = worktree_dir.keep();
        let worktree = Worktree {
            main_repo: main_repo.to_path_buf(),
            path,
        };
        dest.line(format!(
            "message-id: applying posted series in {}",
            worktree.path().display()
        ));

        let scratch_dir = tempfile::Builder::new()
            .prefix("boro-message-id-data-")
            .tempdir()
            .context("create temporary data directory for Message-ID import")?;
        let mut b4 = tokio::process::Command::new(b4_program);
        b4.current_dir(worktree.path())
            .arg("--no-interactive")
            .arg("shazam")
            .arg("-H")
            .arg("--")
            .arg(message_id)
            // Avoid recording this temporary import as an applied series in b4's global data.
            .env("XDG_DATA_HOME", scratch_dir.path().join("xdg-data"));
        let out = crate::process::cancellable_output(
            b4,
            &format!("{} shazam", b4_program.display()),
            interrupt,
        )
        .await?;
        if !out.status.success() {
            anyhow::bail!("b4 shazam failed: {}", stderr_excerpt(&out.stderr));
        }
        if let Some(line) = String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|line| line.contains("Will use the latest revision"))
        {
            eprintln!("[boro] message-id: {}", line.trim());
        }

        let patch_count = String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|line| line.trim().strip_prefix("Total patches: "))
            .and_then(|count| count.parse::<usize>().ok())
            .filter(|count| *count > 0)
            .context("b4 shazam did not report a positive patch count")?;
        let tip = crate::git::rev_parse_commit(worktree.path(), "FETCH_HEAD")
            .context("resolve imported Message-ID tip")?;
        let base =
            crate::git::rev_parse_commit(worktree.path(), &format!("FETCH_HEAD~{patch_count}"))
                .context("resolve imported Message-ID base")?;
        let range = format!("{base}..{tip}");

        Ok(Self {
            _worktree: worktree,
            range,
        })
    }

    pub fn range(&self) -> &str {
        &self.range
    }
}

impl Worktree {
    /// Create `<main_repo>/.boro/worktrees/<command_label>/<sha>/` checked out at `sha`.
    /// `command_label` is the boro subcommand name (`"review"` / `"build"` / `"test"`); per-command
    /// subdirs let concurrent runs of different commands coexist on the same SHA.
    ///
    /// The main repo's working tree is unchanged. Concurrent calls from
    /// different worker tasks are safe: git serializes worktree-add via the
    /// main repo's index lock.
    pub fn create(
        main_repo: &Path,
        command_label: &str,
        sha: &str,
        dest: &VerboseDest,
    ) -> Result<Self> {
        let parent = main_repo.join(WORKTREES_SUBDIR).join(command_label);
        std::fs::create_dir_all(&parent).with_context(|| format!("create {}", parent.display()))?;
        let path = parent.join(sha);

        let out = Command::new("git")
            .current_dir(main_repo)
            .arg("worktree")
            .arg("add")
            .arg("--detach")
            .arg(&path)
            .arg(sha)
            .output()
            .with_context(|| format!("git worktree add for {sha}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "git worktree add {} {sha}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        dest.line(format!("worktree: created {} at {sha}", path.display(),));

        Ok(Self {
            main_repo: main_repo.to_path_buf(),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Best-effort cleanup. Drop must not panic and shouldn't bubble errors.
        let res = Command::new("git")
            .current_dir(&self.main_repo)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(&self.path)
            .output();
        match res {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                eprintln!(
                    "[boro] worktree: failed to remove {} ({}); leaving on disk",
                    self.path.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(e) => {
                eprintln!(
                    "[boro] worktree: failed to spawn `git worktree remove` for {}: {e}",
                    self.path.display()
                );
            }
        }
    }
}

/// Best-effort cleanup of `.boro/worktrees/<command_label>/*` left over from prior runs.
///
/// 1. `git worktree prune --expire=now` drops registry records whose dirs are
///    already gone (e.g. user `rm -rf`'d them). This is repo-wide and safe — it
///    only touches entries whose on-disk path is missing, so a concurrent boro
///    run with a still-existing worktree is not affected.
/// 2. For each `<main_repo>/.boro/worktrees/<command_label>/<sha>/` dir found
///    on disk: try `git worktree remove --force`; if that doesn't clear it,
///    fall back to `fs::remove_dir_all`. Failures are logged and ignored — a
///    concurrent boro run of *the same* command may legitimately own one of
///    these dirs. Other commands' subdirs are left alone so e.g. a startup
///    sweep for `boro build` won't disturb a running `boro test`.
/// 3. Append `.boro/` to `<main_repo>/.git/info/exclude` if not already
///    present, so the worktree dirs don't show up as untracked in
///    `git status` of the main checkout.
pub fn sweep_stale(main_repo: &Path, command_label: &str, dest: &VerboseDest) -> Result<()> {
    ensure_gitignore(main_repo, dest);

    let _ = Command::new("git")
        .current_dir(main_repo)
        .args(["worktree", "prune", "--expire=now"])
        .output();

    let parent = main_repo.join(WORKTREES_SUBDIR).join(command_label);
    let entries = match std::fs::read_dir(&parent) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("read_dir {}", parent.display()));
        }
    };

    let mut removed = 0u32;
    let mut kept = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let out = Command::new("git")
            .current_dir(main_repo)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(&path)
            .output();
        let removed_via_git = matches!(out, Ok(ref o) if o.status.success());
        if !removed_via_git && path.exists() {
            // Not a registered worktree — wipe the directory. If this races a
            // concurrent boro that just registered it, `remove_dir_all` will
            // either succeed (their files vanish) or fail; we tolerate either.
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(e) => {
                    dest.line(format!("worktree: sweep skipped {} ({e})", path.display(),));
                    kept += 1;
                    continue;
                }
            }
        }
        removed += 1;
    }

    if removed > 0 || kept > 0 {
        dest.line(format!(
            "worktree: startup sweep removed {removed} stale dir(s), skipped {kept}",
        ));
    }

    Ok(())
}

fn ensure_gitignore(main_repo: &Path, dest: &VerboseDest) {
    let exclude_path = main_repo.join(".git/info/exclude");
    let line = ".boro/";
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return;
    }
    let mut new_contents = existing;
    if !new_contents.is_empty() && !new_contents.ends_with('\n') {
        new_contents.push('\n');
    }
    new_contents.push_str(line);
    new_contents.push('\n');
    if let Err(e) = std::fs::write(&exclude_path, new_contents) {
        dest.line(format!(
            "worktree: could not append `.boro/` to {}: {e}",
            exclude_path.display(),
        ));
    } else {
        dest.line(format!(
            "worktree: appended `.boro/` to {}",
            exclude_path.display(),
        ));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["config", "user.name", "Boro Test"]);
        git(tmp.path(), &["config", "user.email", "boro@example.com"]);
        fs::write(tmp.path().join("README"), "base\n").unwrap();
        git(tmp.path(), &["add", "README"]);
        git(tmp.path(), &["commit", "-q", "-m", "base"]);
        let base = git(tmp.path(), &["rev-parse", "HEAD"]);
        fs::write(tmp.path().join("upstream.txt"), "upstream\n").unwrap();
        git(tmp.path(), &["add", "upstream.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "series base"]);
        fs::write(tmp.path().join("README"), "posted\n").unwrap();
        git(tmp.path(), &["add", "README"]);
        git(tmp.path(), &["commit", "-q", "-m", "imported"]);
        git(tmp.path(), &["branch", "series-tip"]);
        git(tmp.path(), &["reset", "--hard", &base]);
        tmp
    }

    fn fake_b4(dir: &Path) -> PathBuf {
        let path = dir.join("b4");
        fs::write(
            &path,
            "#!/bin/sh\n\
             set -eu\n\
             test \"$1\" = --no-interactive\n\
             test \"$2\" = shazam\n\
             test \"$3\" = -H\n\
             test \"$4\" = --\n\
             test \"$5\" = 20260620192214.923500-1-oliver@liuxiaozhen.dev\n\
             echo 'Total patches: 1' >&2\n\
             git fetch . series-tip\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn failing_b4(dir: &Path) -> PathBuf {
        let path = dir.join("b4-fail");
        fs::write(
            &path,
            "#!/bin/sh\n\
             echo 'test download failed' >&2\n\
             exit 42\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn blocking_b4(dir: &Path) -> PathBuf {
        let path = dir.join("b4-block");
        fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn worktree_list(repo: &Path) -> String {
        git(repo, &["worktree", "list", "--porcelain"])
    }

    fn interrupt_signal() -> tokio::signal::unix::Signal {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap()
    }

    #[tokio::test]
    async fn message_id_import_isolated_from_source_checkout() {
        let repo = init_repo();
        let tools = tempfile::tempdir().unwrap();
        let b4 = fake_b4(tools.path());
        fs::write(repo.path().join("README"), "target\n").unwrap();
        git(repo.path(), &["add", "README"]);
        git(repo.path(), &["commit", "-q", "-m", "target"]);
        let base = git(repo.path(), &["rev-parse", "HEAD"]);
        let series_base = git(repo.path(), &["rev-parse", "series-tip^"]);
        fs::write(repo.path().join("README"), "locally modified\n").unwrap();
        fs::write(repo.path().join("untracked.txt"), "keep me\n").unwrap();
        let status_before = git(repo.path(), &["status", "--short"]);
        let worktrees_before = worktree_list(repo.path());
        let mut interrupt = interrupt_signal();

        let range;
        {
            let imported = ImportedSeries::prepare_with_program(
                repo.path(),
                "20260620192214.923500-1-oliver@liuxiaozhen.dev",
                &b4,
                &VerboseDest::new(false),
                &mut interrupt,
            )
            .await
            .unwrap();
            range = imported.range().to_string();
            assert_ne!(worktree_list(repo.path()), worktrees_before);
        }

        assert!(range.starts_with(&format!("{series_base}..")));
        assert_eq!(git(repo.path(), &["rev-list", "--count", &range]), "1");
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), base);
        assert_eq!(worktree_list(repo.path()), worktrees_before);
        assert_eq!(git(repo.path(), &["status", "--short"]), status_before);
        assert_eq!(
            fs::read_to_string(repo.path().join("README")).unwrap(),
            "locally modified\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("untracked.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[tokio::test]
    async fn failed_message_id_import_cleans_up_temporary_worktree() {
        let repo = init_repo();
        let tools = tempfile::tempdir().unwrap();
        let b4 = failing_b4(tools.path());
        let base = git(repo.path(), &["rev-parse", "HEAD"]);
        let worktrees_before = worktree_list(repo.path());
        let mut interrupt = interrupt_signal();

        let error = match ImportedSeries::prepare_with_program(
            repo.path(),
            "20260620192214.923500-1-oliver@liuxiaozhen.dev",
            &b4,
            &VerboseDest::new(false),
            &mut interrupt,
        )
        .await
        {
            Ok(_) => panic!("failing b4 unexpectedly succeeded"),
            Err(error) => error,
        };

        assert!(format!("{error:#}").contains("test download failed"));
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), base);
        assert_eq!(worktree_list(repo.path()), worktrees_before);
        assert!(git(repo.path(), &["status", "--short"]).is_empty());
    }

    #[tokio::test]
    async fn cancelled_message_id_import_cleans_up_temporary_worktree() {
        let repo = init_repo();
        let tools = tempfile::tempdir().unwrap();
        let b4 = blocking_b4(tools.path());
        let base = git(repo.path(), &["rev-parse", "HEAD"]);
        let worktrees_before = worktree_list(repo.path());
        let mut interrupt = interrupt_signal();

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            ImportedSeries::prepare_with_program(
                repo.path(),
                "20260620192214.923500-1-oliver@liuxiaozhen.dev",
                &b4,
                &VerboseDest::new(false),
                &mut interrupt,
            ),
        )
        .await;

        assert!(result.is_err(), "blocking import was not cancelled");
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), base);
        assert_eq!(worktree_list(repo.path()), worktrees_before);
        assert!(git(repo.path(), &["status", "--short"]).is_empty());
    }

    #[tokio::test]
    async fn invalid_message_id_creates_no_worktree() {
        let repo = init_repo();
        let worktrees_before = worktree_list(repo.path());
        let mut interrupt = interrupt_signal();

        let result = ImportedSeries::prepare_with_program(
            repo.path(),
            "invalid\nmessage-id",
            Path::new("b4"),
            &VerboseDest::new(false),
            &mut interrupt,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(worktree_list(repo.path()), worktrees_before);
    }
}
