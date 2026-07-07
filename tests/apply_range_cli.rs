use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn rev_parse(repo: &Path, revision: &str) -> String {
    String::from_utf8(git(repo, &["rev-parse", revision]).stdout)
        .unwrap()
        .trim()
        .to_string()
}

fn init_repo(repo: &Path) {
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test User"]);
}

fn commit_file(repo: &Path, path: &str, content: &str, subject: &str) -> String {
    fs::write(repo.join(path), content).unwrap();
    git(repo, &["add", path]);
    git(repo, &["commit", "-m", subject]);
    rev_parse(repo, "HEAD")
}

fn boro_command(repo: &Path, json: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_boro"));
    command
        .current_dir(repo.parent().unwrap())
        .args(["--source", repo.file_name().unwrap().to_str().unwrap()])
        .env("BORO_URL", "http://127.0.0.1:9/v1")
        .env("BORO_KEY", "")
        .env("BORO_MODEL", "test-model")
        .env("BORO_VALIDATION_URL", "http://127.0.0.1:9/v1")
        .env("BORO_VALIDATION_KEY", "")
        .env("BORO_VALIDATION_MODEL", "test-model");
    if json {
        command.arg("--json");
    }
    command
}

fn boro(repo: &Path) -> Command {
    boro_command(repo, true)
}

fn apply_message_id_series(patch_count: usize, json: bool) {
    let dir = tempfile::Builder::new()
        .prefix("boro message id ")
        .tempdir()
        .unwrap();
    let repo = dir.path();
    init_repo(repo);
    commit_file(repo, "base.txt", "base\n", "base");
    git(repo, &["tag", "apply-base"]);
    let expected = (1..=patch_count)
        .map(|index| {
            let subject = format!("series {index}");
            commit_file(
                repo,
                &format!("{index}.txt"),
                &format!("{index}\n"),
                &subject,
            );
            subject
        })
        .collect::<Vec<_>>();
    git(repo, &["branch", "series-tip"]);

    let tools = tempfile::tempdir().unwrap();
    let b4 = tools.path().join("b4");
    fs::write(
        &b4,
        "#!/bin/sh\n\
         set -eu\n\
         test \"$1\" = --no-interactive\n\
         test \"$2\" = shazam\n\
         test \"$3\" = -H\n\
         test \"$4\" = --\n\
         test \"$5\" = series@example.com\n\
         echo \"Total patches: $BORO_TEST_PATCH_COUNT\" >&2\n\
         git fetch . series-tip\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&b4).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&b4, permissions).unwrap();
    let path = std::env::join_paths(std::iter::once(tools.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();

    git(repo, &["checkout", "--detach", "apply-base"]);
    let base = rev_parse(repo, "HEAD");
    let worktrees_before =
        String::from_utf8(git(repo, &["worktree", "list", "--porcelain"]).stdout)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count();
    let output = boro_command(repo, json)
        .env("PATH", path)
        .env("BORO_TEST_PATCH_COUNT", patch_count.to_string())
        .args(["apply", "--message-id", "series@example.com"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let applied_range = if json {
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["commits"].as_array().unwrap().len(), patch_count);
        report["applied_range"].as_str().unwrap().to_string()
    } else {
        let stdout = String::from_utf8(output.stdout).unwrap();
        let prefix = format!("Review with: boro --source '{}' review ", repo.display());
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("missing quoted review command:\n{stdout}"))
            .to_string()
    };
    assert!(applied_range.starts_with(&format!("{base}..")));
    let subjects =
        String::from_utf8(git(repo, &["log", "--reverse", "--format=%s", &applied_range]).stdout)
            .unwrap();
    assert_eq!(subjects.lines().collect::<Vec<_>>(), expected);
    let worktrees_after =
        String::from_utf8(git(repo, &["worktree", "list", "--porcelain"]).stdout).unwrap();
    assert_eq!(
        worktrees_after
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        worktrees_before
    );

    let review = boro(repo)
        .arg("--dry-run")
        .args(["review", &applied_range])
        .output()
        .unwrap();
    assert!(
        review.status.success(),
        "{}",
        String::from_utf8_lossy(&review.stderr)
    );
    let review: serde_json::Value = serde_json::from_slice(&review.stdout).unwrap();
    assert_eq!(review["commits"].as_array().unwrap().len(), patch_count);
}

#[test]
fn apply_message_id_handles_one_patch() {
    apply_message_id_series(1, false);
}

#[test]
fn apply_message_id_handles_multiple_patches() {
    apply_message_id_series(2, true);
}

#[test]
fn apply_range_dry_runs_then_applies_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_repo(repo);
    commit_file(repo, "base.txt", "base\n", "base");
    git(repo, &["tag", "apply-base"]);
    let one = commit_file(repo, "one.txt", "one\n", "series one");
    let two = commit_file(repo, "two.txt", "two\n", "series two");
    git(repo, &["branch", "series-tip"]);
    git(repo, &["checkout", "--detach", "apply-base"]);

    fs::write(repo.join("scratch.txt"), "keep\n").unwrap();
    let head_before = rev_parse(repo, "HEAD");
    let output = boro(repo)
        .arg("--dry-run")
        .args(["apply", "apply-base..series-tip"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["commits"][0]["commit"], one);
    assert_eq!(report["commits"][1]["commit"], two);
    assert_eq!(rev_parse(repo, "HEAD"), head_before);
    assert!(repo.join("scratch.txt").exists());

    fs::remove_file(repo.join("scratch.txt")).unwrap();
    let output = boro(repo)
        .args(["apply", "apply-base..series-tip"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let subjects = String::from_utf8(
        git(
            repo,
            &["log", "--reverse", "--format=%s", "apply-base..HEAD"],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(
        subjects.lines().collect::<Vec<_>>(),
        ["series one", "series two"]
    );
}

#[test]
fn apply_range_stops_at_first_failure() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_repo(repo);
    commit_file(repo, "base.txt", "base\n", "base");
    git(repo, &["tag", "apply-base"]);
    commit_file(repo, "one.txt", "one\n", "series one");
    git(repo, &["commit", "--allow-empty", "-m", "series empty"]);
    commit_file(repo, "three.txt", "three\n", "series three");
    git(repo, &["branch", "series-tip"]);
    git(repo, &["checkout", "--detach", "apply-base"]);

    let output = boro(repo)
        .args(["apply", "apply-base..series-tip"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(repo.join("one.txt").exists());
    assert!(!repo.join("three.txt").exists());
    assert_eq!(
        String::from_utf8(git(repo, &["log", "-1", "--format=%s"]).stdout)
            .unwrap()
            .trim(),
        "series one"
    );
}

#[test]
fn apply_single_commit_keeps_schema_v1() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_repo(repo);
    let source = commit_file(repo, "source.txt", "source\n", "source root");
    git(repo, &["checkout", "--orphan", "target"]);
    git(repo, &["rm", "-rf", "."]);
    commit_file(repo, "target.txt", "target\n", "target root");

    let output = boro(repo).args(["apply", &source]).output().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["commit"], source);
    assert!(report.get("commits").is_none());
}
