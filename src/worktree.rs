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

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::verbose::VerboseDest;

const WORKTREES_SUBDIR: &str = ".boro/worktrees";
const RUN_LOCK_FILE: &str = ".boro-run.lock";

fn command_dir_path(main_repo: &Path, command_label: &str) -> PathBuf {
    main_repo.join(WORKTREES_SUBDIR).join(command_label)
}

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

fn run_dir_path(main_repo: &Path, command_label: &str) -> PathBuf {
    command_dir_path(main_repo, command_label).join(process_run_id())
}

fn worktree_path(main_repo: &Path, command_label: &str, sha: &str) -> PathBuf {
    run_dir_path(main_repo, command_label).join(sha)
}

fn ensure_no_symlink_components(root: &Path, path: &Path) -> Result<()> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("{} not under {}", path.display(), root.display()))?;
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                anyhow::bail!("refusing symlinked path {}", current.display());
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).with_context(|| format!("symlink_metadata {}", current.display()));
            }
        }
    }
    Ok(())
}

fn validate_cleanup_dir(main_repo: &Path, expected_parent: &Path, path: &Path) -> Result<()> {
    if path.parent() != Some(expected_parent) {
        anyhow::bail!(
            "refusing path outside expected directory tree: {}",
            path.display()
        );
    }
    ensure_no_symlink_components(main_repo, path)?;
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("symlink_metadata {}", path.display()))?;
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        anyhow::bail!("refusing symlinked directory {}", path.display());
    }
    if !file_type.is_dir() {
        anyhow::bail!("expected directory at {}", path.display());
    }
    Ok(())
}

fn remove_empty_dir_if_possible(path: &Path) {
    let _ = fs::remove_dir(path);
}

struct RunLease {
    run_dir: PathBuf,
    lock_path: PathBuf,
    file: File,
}

impl RunLease {
    fn create(run_dir: &Path) -> Result<Self> {
        fs::create_dir_all(run_dir).with_context(|| format!("create {}", run_dir.display()))?;
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
        let _ = fs::remove_file(&self.lock_path);
        let _ = fs::remove_dir(&self.run_dir);
        if let Some(command_dir) = self.run_dir.parent() {
            remove_empty_dir_if_possible(command_dir);
        }
    }
}

fn run_dir_has_entries_other_than_lock(run_dir: &Path) -> bool {
    let entries = match fs::read_dir(run_dir) {
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
    let command_dir = command_dir_path(main_repo, command_label);
    ensure_no_symlink_components(main_repo, &command_dir)?;
    let run_dir = run_dir_path(main_repo, command_label);
    ensure_no_symlink_components(main_repo, &run_dir)?;
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
    _lease: Option<Arc<RunLease>>,
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
            _lease: None,
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
        ensure_no_symlink_components(main_repo, parent)?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

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
            _lease: Some(lease),
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

    let command_dir = command_dir_path(main_repo, command_label);
    if let Err(e) = ensure_no_symlink_components(main_repo, &command_dir) {
        dest.line(format!(
            "worktree: startup sweep skipped {} ({e:#})",
            command_dir.display()
        ));
        return Ok(());
    }

    let _ = Command::new("git")
        .current_dir(main_repo)
        .args(["worktree", "prune", "--expire=now"])
        .output();

    let entries = match fs::read_dir(&command_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("read_dir {}", command_dir.display()));
        }
    };

    let mut removed = 0u32;
    let mut kept = 0u32;
    for entry in entries.flatten() {
        let run_dir = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                dest.line(format!(
                    "worktree: sweep skipped {} ({e})",
                    run_dir.display()
                ));
                kept += 1;
                continue;
            }
        };
        if file_type.is_symlink() {
            dest.line(format!(
                "worktree: sweep skipped {} (refusing symlinked run dir)",
                run_dir.display()
            ));
            kept += 1;
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        match sweep_run_dir(main_repo, &command_dir, &run_dir, dest) {
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

fn sweep_run_dir(
    main_repo: &Path,
    command_dir: &Path,
    run_dir: &Path,
    dest: &VerboseDest,
) -> SweepOutcome {
    if let Err(e) = validate_cleanup_dir(main_repo, command_dir, run_dir) {
        dest.line(format!(
            "worktree: sweep skipped {} ({e:#})",
            run_dir.display()
        ));
        return SweepOutcome::Kept;
    }

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

    let entries = match fs::read_dir(run_dir) {
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
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                dest.line(format!("worktree: sweep skipped {} ({e})", path.display()));
                all_removed = false;
                continue;
            }
        };
        if file_type.is_symlink() {
            dest.line(format!(
                "worktree: sweep skipped {} (refusing symlinked cleanup path)",
                path.display()
            ));
            all_removed = false;
            continue;
        }
        if file_type.is_dir() {
            if !remove_stale_dir(main_repo, run_dir, &path, dest) {
                all_removed = false;
            }
        } else if let Err(e) = fs::remove_file(&path) {
            dest.line(format!("worktree: sweep skipped {} ({e})", path.display()));
            all_removed = false;
        }
    }

    if !all_removed || run_dir_has_entries_other_than_lock(run_dir) {
        return SweepOutcome::Kept;
    }

    drop(file);
    let lock_path = run_dir.join(RUN_LOCK_FILE);
    let _ = fs::remove_file(lock_path);
    match fs::remove_dir(run_dir) {
        Ok(()) => {
            remove_empty_dir_if_possible(command_dir);
            SweepOutcome::Removed
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            remove_empty_dir_if_possible(command_dir);
            SweepOutcome::Removed
        }
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

fn remove_stale_dir(main_repo: &Path, run_dir: &Path, path: &Path, dest: &VerboseDest) -> bool {
    if let Err(e) = validate_cleanup_dir(main_repo, run_dir, path) {
        dest.line(format!(
            "worktree: sweep skipped {} ({e:#})",
            path.display()
        ));
        return false;
    }

    let out = Command::new("git")
        .current_dir(main_repo)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(path)
        .output();
    let removed_via_git = matches!(out, Ok(ref o) if o.status.success());
    if !removed_via_git && path.exists() {
        if let Err(e) = fs::remove_dir_all(path) {
            dest.line(format!("worktree: sweep skipped {} ({e})", path.display()));
        }
    }
    !path.exists()
}

fn ensure_gitignore(main_repo: &Path, dest: &VerboseDest) {
    let exclude_path = main_repo.join(".git/info/exclude");
    let line = ".boro/";
    let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return;
    }
    let mut new_contents = existing;
    if !new_contents.is_empty() && !new_contents.ends_with('\n') {
        new_contents.push('\n');
    }
    new_contents.push_str(line);
    new_contents.push('\n');
    if let Err(e) = fs::write(&exclude_path, new_contents) {
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

    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};

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
        let tmp = TempDir::new().unwrap();
        run_git_test(tmp.path(), &["init", "-q"]);
        run_git_test(tmp.path(), &["config", "user.name", "Boro Test"]);
        run_git_test(tmp.path(), &["config", "user.email", "boro@example.com"]);
        fs::write(tmp.path().join("README"), "base\n").unwrap();
        run_git_test(tmp.path(), &["add", "README"]);
        run_git_test(tmp.path(), &["commit", "-q", "-m", "base"]);
        let base = run_git_test(tmp.path(), &["rev-parse", "HEAD"]);
        fs::write(tmp.path().join("upstream.txt"), "upstream\n").unwrap();
        run_git_test(tmp.path(), &["add", "upstream.txt"]);
        run_git_test(tmp.path(), &["commit", "-q", "-m", "series base"]);
        fs::write(tmp.path().join("README"), "posted\n").unwrap();
        run_git_test(tmp.path(), &["add", "README"]);
        run_git_test(tmp.path(), &["commit", "-q", "-m", "imported"]);
        run_git_test(tmp.path(), &["branch", "series-tip"]);
        run_git_test(tmp.path(), &["reset", "--hard", &base]);
        tmp
    }

    fn repo_with_commit() -> (TempDir, String) {
        let d = init_repo();
        fs::write(d.path().join("f.c"), "int x = 1;\n").unwrap();
        run_git_test(d.path(), &["add", "f.c"]);
        run_git_test(d.path(), &["commit", "-q", "-m", "extra"]);
        let sha = run_git_test(d.path(), &["rev-parse", "HEAD"]);
        (d, sha)
    }

    fn write_unlocked_run_lock(run_dir: &Path) {
        fs::create_dir_all(run_dir).unwrap();
        fs::write(
            run_dir.join(RUN_LOCK_FILE),
            format!("pid={}\nrun_id=test\n", std::process::id()),
        )
        .unwrap();
    }

    fn lock_run_dir(run_dir: &Path) -> File {
        fs::create_dir_all(run_dir).unwrap();
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    fn blocking_b4(dir: &Path) -> PathBuf {
        let path = dir.join("b4-block");
        fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    fn worktree_list(repo: &Path) -> String {
        run_git_test(repo, &["worktree", "list", "--porcelain"])
    }

    #[cfg(unix)]
    fn interrupt_signal() -> tokio::signal::unix::Signal {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap()
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
        let command_dir = run_dir.parent().unwrap().to_path_buf();

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
        assert!(!command_dir.exists(), "drop left {}", command_dir.display());
    }

    #[test]
    fn sweep_stale_removes_unlocked_registered_run_dir() {
        let (d, sha) = repo_with_commit();
        let command_dir = d.path().join(WORKTREES_SUBDIR).join("review");
        let run_dir = command_dir.join("stale-run");
        write_unlocked_run_lock(&run_dir);
        let worktree = run_dir.join(&sha);
        git_worktree_add(d.path(), &worktree, &sha);
        let listed = run_git_test(d.path(), &["worktree", "list", "--porcelain"]);
        assert!(listed.contains(&worktree.to_string_lossy().to_string()));

        sweep_stale(d.path(), "review", &VerboseDest::new(false)).unwrap();

        assert!(!worktree.exists(), "sweep left {}", worktree.display());
        assert!(!run_dir.exists(), "sweep left {}", run_dir.display());
        assert!(
            !command_dir.exists(),
            "sweep left {}",
            command_dir.display()
        );
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
        fs::create_dir_all(live_file.parent().unwrap()).unwrap();
        fs::write(&live_file, "still in use\n").unwrap();
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
        fs::create_dir_all(live_file.parent().unwrap()).unwrap();
        fs::write(&live_file, "still in use\n").unwrap();

        sweep_stale(d.path(), "review", &VerboseDest::new(false)).unwrap();

        assert!(live_file.exists(), "sweep removed {}", live_file.display());
    }

    #[cfg(unix)]
    #[test]
    fn sweep_stale_skips_symlinked_run_dirs() {
        let d = init_repo();
        let command_dir = d.path().join(WORKTREES_SUBDIR).join("review");
        fs::create_dir_all(&command_dir).unwrap();
        let target = d.path().join("outside-run");
        fs::create_dir_all(&target).unwrap();
        let target_file = target.join("keep.txt");
        fs::write(&target_file, "keep me\n").unwrap();
        let run_dir = command_dir.join("link-run");
        symlink(&target, &run_dir).unwrap();

        sweep_stale(d.path(), "review", &VerboseDest::new(false)).unwrap();

        assert!(run_dir.exists(), "sweep removed {}", run_dir.display());
        assert!(
            target_file.exists(),
            "sweep removed {}",
            target_file.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn sweep_stale_skips_symlinked_worktree_dirs() {
        let d = init_repo();
        let run_dir = d
            .path()
            .join(WORKTREES_SUBDIR)
            .join("review")
            .join("stale-run");
        write_unlocked_run_lock(&run_dir);
        let target = d.path().join("outside-worktree");
        fs::create_dir_all(&target).unwrap();
        let target_file = target.join("keep.txt");
        fs::write(&target_file, "keep me\n").unwrap();
        let worktree = run_dir.join("deadbeef");
        symlink(&target, &worktree).unwrap();

        sweep_stale(d.path(), "review", &VerboseDest::new(false)).unwrap();

        assert!(run_dir.exists(), "sweep removed {}", run_dir.display());
        assert!(worktree.exists(), "sweep removed {}", worktree.display());
        assert!(
            target_file.exists(),
            "sweep removed {}",
            target_file.display()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn message_id_import_isolated_from_source_checkout() {
        let repo = init_repo();
        let tools = tempfile::tempdir().unwrap();
        let b4 = fake_b4(tools.path());
        fs::write(repo.path().join("README"), "target\n").unwrap();
        run_git_test(repo.path(), &["add", "README"]);
        run_git_test(repo.path(), &["commit", "-q", "-m", "target"]);
        let base = run_git_test(repo.path(), &["rev-parse", "HEAD"]);
        let series_base = run_git_test(repo.path(), &["rev-parse", "series-tip^"]);
        fs::write(repo.path().join("README"), "locally modified\n").unwrap();
        fs::write(repo.path().join("untracked.txt"), "keep me\n").unwrap();
        let status_before = run_git_test(repo.path(), &["status", "--short"]);
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
        assert_eq!(
            run_git_test(repo.path(), &["rev-list", "--count", &range]),
            "1"
        );
        assert_eq!(run_git_test(repo.path(), &["rev-parse", "HEAD"]), base);
        assert_eq!(worktree_list(repo.path()), worktrees_before);
        assert_eq!(
            run_git_test(repo.path(), &["status", "--short"]),
            status_before
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("README")).unwrap(),
            "locally modified\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("untracked.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_message_id_import_cleans_up_temporary_worktree() {
        let repo = init_repo();
        let tools = tempfile::tempdir().unwrap();
        let b4 = failing_b4(tools.path());
        let base = run_git_test(repo.path(), &["rev-parse", "HEAD"]);
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
        assert_eq!(run_git_test(repo.path(), &["rev-parse", "HEAD"]), base);
        assert_eq!(worktree_list(repo.path()), worktrees_before);
        assert!(run_git_test(repo.path(), &["status", "--short"]).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_message_id_import_cleans_up_temporary_worktree() {
        let repo = init_repo();
        let tools = tempfile::tempdir().unwrap();
        let b4 = blocking_b4(tools.path());
        let base = run_git_test(repo.path(), &["rev-parse", "HEAD"]);
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
        for _ in 0..20 {
            if worktree_list(repo.path()) == worktrees_before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(run_git_test(repo.path(), &["rev-parse", "HEAD"]), base);
        assert_eq!(worktree_list(repo.path()), worktrees_before);
        assert!(run_git_test(repo.path(), &["status", "--short"]).is_empty());
    }

    #[cfg(unix)]
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
