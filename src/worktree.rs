// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-commit git worktrees so each parallel review sees the working tree
//! pinned to its commit's SHA, not the latest state of the main checkout.
//!
//! Layout: `<main_repo>/.boro/worktrees/<command>/<run-id>/<sha>/` — a
//! `git worktree add --detach` checkout of `<sha>`, where `<command>` is
//! `review` / `build` / `test`. Per-command and per-run subdirs let you run
//! multiple commands, or even multiple instances of the same command, against
//! the same commit at the same time without their worktree paths colliding. The
//! main repo is left untouched, so the user can keep editing / building /
//! switching branches there while reviews are in flight.
//!
//! Cleanup: a [`Worktree`] is an RAII handle whose `Drop` runs
//! `git worktree remove --force`. Tokio aborts (e.g. on Ctrl-C) drop the
//! task's locals, so the same path runs on cancellation. A best-effort
//! [`sweep_stale`] runs once at startup to clean up orphans from prior
//! crashes — scoped to the current command's subdir so it doesn't disturb
//! a concurrent boro run that's using a different command.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::verbose::VerboseDest;

const WORKTREES_SUBDIR: &str = ".boro/worktrees";
const RUN_LOCK_FILE: &str = ".boro-run.lock";

fn process_run_id() -> &'static str {
    static RUN_ID: OnceLock<String> = OnceLock::new();
    RUN_ID.get_or_init(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{now}", std::process::id())
    })
}

fn worktree_path(main_repo: &Path, command_label: &str, sha: &str) -> PathBuf {
    run_dir_path(main_repo, command_label).join(sha)
}

fn run_dir_path(main_repo: &Path, command_label: &str) -> PathBuf {
    main_repo
        .join(WORKTREES_SUBDIR)
        .join(command_label)
        .join(process_run_id())
}

struct RunLease {
    run_dir: PathBuf,
    lock_path: PathBuf,
    file: File,
}

impl RunLease {
    fn create(run_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(run_dir)
            .with_context(|| format!("create {}", run_dir.display()))?;
        let lock_path = run_dir.join(RUN_LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        file.try_lock()
            .with_context(|| format!("lock {}", lock_path.display()))?;
        file.set_len(0)
            .with_context(|| format!("truncate {}", lock_path.display()))?;
        writeln!(file, "pid={}", std::process::id())
            .with_context(|| format!("write {}", lock_path.display()))?;
        writeln!(file, "run_id={}", process_run_id())
            .with_context(|| format!("write {}", lock_path.display()))?;
        let _ = file.sync_all();
        Ok(Self {
            run_dir: run_dir.to_path_buf(),
            lock_path,
            file,
        })
    }
}

impl Drop for RunLease {
    fn drop(&mut self) {
        if run_dir_has_entries_other_than_lock(&self.run_dir) {
            return;
        }
        let _ = self.file.unlock();
        let _ = std::fs::remove_file(&self.lock_path);
        let _ = std::fs::remove_dir(&self.run_dir);
        if let Some(command_dir) = self.run_dir.parent() {
            let _ = std::fs::remove_dir(command_dir);
        }
    }
}

fn run_dir_has_entries_other_than_lock(run_dir: &Path) -> bool {
    let entries = match std::fs::read_dir(run_dir) {
        Ok(entries) => entries,
        Err(_) => return true,
    };
    for entry in entries.flatten() {
        if entry.file_name() != RUN_LOCK_FILE {
            return true;
        }
    }
    false
}

fn run_leases() -> &'static Mutex<Vec<Weak<RunLease>>> {
    static RUN_LEASES: OnceLock<Mutex<Vec<Weak<RunLease>>>> = OnceLock::new();
    RUN_LEASES.get_or_init(|| Mutex::new(Vec::new()))
}

fn run_lease(main_repo: &Path, command_label: &str) -> Result<Arc<RunLease>> {
    let run_dir = run_dir_path(main_repo, command_label);
    let mut leases = run_leases()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    leases.retain(|lease| lease.strong_count() > 0);
    for lease in leases.iter().filter_map(Weak::upgrade) {
        if lease.run_dir == run_dir {
            return Ok(lease);
        }
    }
    let lease = Arc::new(RunLease::create(&run_dir)?);
    leases.push(Arc::downgrade(&lease));
    Ok(lease)
}

/// RAII handle for a `git worktree add --detach <path> <sha>` checkout.
///
/// On drop, runs `git worktree remove --force` against the main repo
/// (best-effort: failures are logged, never panic).
pub struct Worktree {
    main_repo: PathBuf,
    path: PathBuf,
    _lease: Arc<RunLease>,
}

impl Worktree {
    /// Create `<main_repo>/.boro/worktrees/<command_label>/<run-id>/<sha>/` checked out at `sha`.
    /// `command_label` is the boro subcommand name (`"review"` / `"build"` / `"test"`); per-command
    /// and per-run subdirs let concurrent runs coexist on the same SHA.
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
        let lease = run_lease(main_repo, command_label)?;
        let path = worktree_path(main_repo, command_label, sha);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid worktree path {}", path.display()))?;
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

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
            _lease: lease,
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

/// Best-effort cleanup of stale Git worktree registry records and dead boro run dirs.
///
/// 1. `git worktree prune --expire=now` drops registry records whose dirs are
///    already gone (e.g. user `rm -rf`'d them). This is repo-wide and safe — it
///    only touches entries whose on-disk path is missing, so a concurrent boro
///    run with a still-existing worktree is not affected.
/// 2. Each run dir has a lock file held by its owning process. If the lock is
///    held, the run is live and left alone. If the lock can be acquired, the
///    owner is gone, so its registered worktrees and files can be removed.
///    Dirs without a lock file are left alone because ownership is unknown.
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
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("read_dir {}", parent.display()));
        }
    };

    let mut removed = 0u32;
    let mut kept = 0u32;
    for entry in entries.flatten() {
        let run_dir = entry.path();
        if !run_dir.is_dir() {
            continue;
        }
        match sweep_run_dir(main_repo, &run_dir, dest) {
            SweepOutcome::Removed => removed += 1,
            SweepOutcome::Kept => kept += 1,
        }
    }

    let _ = Command::new("git")
        .current_dir(main_repo)
        .args(["worktree", "prune", "--expire=now"])
        .output();

    if removed > 0 || kept > 0 {
        dest.line(format!(
            "worktree: startup sweep removed {removed} stale run(s), skipped {kept}",
        ));
    }

    Ok(())
}

enum SweepOutcome {
    Removed,
    Kept,
}

enum RunLock {
    Stale(File),
    Live,
    Unknown,
}

fn sweep_run_dir(main_repo: &Path, run_dir: &Path, dest: &VerboseDest) -> SweepOutcome {
    let lock = match try_lock_run(run_dir) {
        Ok(lock) => lock,
        Err(e) => {
            dest.line(format!(
                "worktree: sweep skipped {} ({e:#})",
                run_dir.display()
            ));
            return SweepOutcome::Kept;
        }
    };
    let RunLock::Stale(file) = lock else {
        return SweepOutcome::Kept;
    };

    let entries = match std::fs::read_dir(run_dir) {
        Ok(entries) => entries,
        Err(e) => {
            dest.line(format!(
                "worktree: sweep skipped {} ({e})",
                run_dir.display()
            ));
            return SweepOutcome::Kept;
        }
    };
    let mut all_removed = true;
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name() == RUN_LOCK_FILE {
            continue;
        }
        if path.is_dir() {
            if !remove_stale_dir(main_repo, &path, dest) {
                all_removed = false;
            }
        } else if let Err(e) = std::fs::remove_file(&path) {
            dest.line(format!("worktree: sweep skipped {} ({e})", path.display()));
            all_removed = false;
        }
    }

    if !all_removed || run_dir_has_entries_other_than_lock(run_dir) {
        return SweepOutcome::Kept;
    }

    drop(file);
    let lock_path = run_dir.join(RUN_LOCK_FILE);
    let _ = std::fs::remove_file(lock_path);
    match std::fs::remove_dir(run_dir) {
        Ok(()) => SweepOutcome::Removed,
        Err(e) if e.kind() == ErrorKind::NotFound => SweepOutcome::Removed,
        Err(e) => {
            dest.line(format!(
                "worktree: sweep skipped {} ({e})",
                run_dir.display()
            ));
            SweepOutcome::Kept
        }
    }
}

fn try_lock_run(run_dir: &Path) -> Result<RunLock> {
    let lock_path = run_dir.join(RUN_LOCK_FILE);
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(RunLock::Unknown),
        Err(e) => return Err(e).with_context(|| format!("open {}", lock_path.display())),
    };
    match file.try_lock() {
        Ok(()) => Ok(RunLock::Stale(file)),
        Err(TryLockError::WouldBlock) => Ok(RunLock::Live),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("lock {}", lock_path.display()))
        }
    }
}

fn remove_stale_dir(main_repo: &Path, path: &Path, dest: &VerboseDest) -> bool {
    let out = Command::new("git")
        .current_dir(main_repo)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(path)
        .output();
    let removed_via_git = matches!(out, Ok(ref o) if o.status.success());
    if !removed_via_git && path.exists() {
        if let Err(e) = std::fs::remove_dir_all(path) {
            dest.line(format!("worktree: sweep skipped {} ({e})", path.display()));
        }
    }
    !path.exists()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn run_git_test(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let d = TempDir::new().unwrap();
        run_git_test(d.path(), &["init"]);
        d
    }

    fn repo_with_commit() -> (TempDir, String) {
        let d = init_repo();
        run_git_test(d.path(), &["config", "user.email", "test@example.com"]);
        run_git_test(d.path(), &["config", "user.name", "Test User"]);
        std::fs::write(d.path().join("f.c"), "int x = 1;\n").unwrap();
        run_git_test(d.path(), &["add", "f.c"]);
        run_git_test(d.path(), &["commit", "-m", "base"]);
        let sha = run_git_test(d.path(), &["rev-parse", "HEAD"]);
        (d, sha)
    }

    fn write_unlocked_run_lock(run_dir: &Path) {
        std::fs::create_dir_all(run_dir).unwrap();
        std::fs::write(
            run_dir.join(RUN_LOCK_FILE),
            format!("pid={}\nrun_id=test\n", std::process::id()),
        )
        .unwrap();
    }

    fn lock_run_dir(run_dir: &Path) -> File {
        std::fs::create_dir_all(run_dir).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(run_dir.join(RUN_LOCK_FILE))
            .unwrap();
        file.try_lock().unwrap();
        file
    }

    fn git_worktree_add(repo: &Path, path: &Path, sha: &str) {
        let out = Command::new("git")
            .current_dir(repo)
            .arg("worktree")
            .arg("add")
            .arg("--detach")
            .arg(path)
            .arg(sha)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree add {} {sha} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn worktree_path_uses_process_run_namespace() {
        let d = TempDir::new().unwrap();
        let path = worktree_path(d.path(), "review", "abc123");
        assert_eq!(
            path,
            d.path()
                .join(WORKTREES_SUBDIR)
                .join("review")
                .join(process_run_id())
                .join("abc123")
        );
    }

    #[test]
    fn create_uses_process_run_namespace() {
        let (d, sha) = repo_with_commit();
        let worktree =
            Worktree::create(d.path(), "review", &sha, &VerboseDest::new(false)).unwrap();
        let path = worktree.path().to_path_buf();
        let run_dir = path.parent().unwrap().to_path_buf();

        assert!(path.starts_with(
            d.path()
                .join(WORKTREES_SUBDIR)
                .join("review")
                .join(process_run_id())
        ));
        assert!(path.join("f.c").is_file(), "missing {}", path.display());

        drop(worktree);
        assert!(!path.exists(), "drop left {}", path.display());
        assert!(!run_dir.exists(), "drop left {}", run_dir.display());
    }

    #[test]
    fn sweep_stale_removes_unlocked_registered_run_dir() {
        let (d, sha) = repo_with_commit();
        let run_dir = d
            .path()
            .join(WORKTREES_SUBDIR)
            .join("review")
            .join("stale-run");
        write_unlocked_run_lock(&run_dir);
        let worktree = run_dir.join(&sha);
        git_worktree_add(d.path(), &worktree, &sha);
        let listed = run_git_test(d.path(), &["worktree", "list", "--porcelain"]);
        assert!(listed.contains(&worktree.to_string_lossy().to_string()));

        sweep_stale(d.path(), "review", &VerboseDest::new(false)).unwrap();

        assert!(!worktree.exists(), "sweep left {}", worktree.display());
        assert!(!run_dir.exists(), "sweep left {}", run_dir.display());
        let listed = run_git_test(d.path(), &["worktree", "list", "--porcelain"]);
        assert!(!listed.contains(&worktree.to_string_lossy().to_string()));
    }

    #[test]
    fn sweep_stale_keeps_live_locked_run_dirs() {
        let d = init_repo();
        let run_dir = d
            .path()
            .join(WORKTREES_SUBDIR)
            .join("review")
            .join("live-run");
        let live_file = run_dir.join("deadbeef").join("live.txt");
        std::fs::create_dir_all(live_file.parent().unwrap()).unwrap();
        std::fs::write(&live_file, "still in use\n").unwrap();
        let _lock = lock_run_dir(&run_dir);

        sweep_stale(d.path(), "review", &VerboseDest::new(false)).unwrap();

        assert!(live_file.exists(), "sweep removed {}", live_file.display());
    }

    #[test]
    fn sweep_stale_keeps_existing_command_worktree_dirs() {
        let d = init_repo();
        let live_file = d
            .path()
            .join(WORKTREES_SUBDIR)
            .join("review")
            .join("other-run")
            .join("deadbeef")
            .join("live.txt");
        std::fs::create_dir_all(live_file.parent().unwrap()).unwrap();
        std::fs::write(&live_file, "still in use\n").unwrap();

        sweep_stale(d.path(), "review", &VerboseDest::new(false)).unwrap();

        assert!(live_file.exists(), "sweep removed {}", live_file.display());
    }
}
