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
