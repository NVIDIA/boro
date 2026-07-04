use std::fs;
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

fn boro(repo: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_boro"));
    command
        .current_dir(repo.parent().unwrap())
        .args([
            "--json",
            "--source",
            repo.file_name().unwrap().to_str().unwrap(),
        ])
        .env("BORO_URL", "http://127.0.0.1:9/v1")
        .env("BORO_KEY", "")
        .env("BORO_MODEL", "test-model")
        .env("BORO_VALIDATION_URL", "http://127.0.0.1:9/v1")
        .env("BORO_VALIDATION_KEY", "")
        .env("BORO_VALIDATION_MODEL", "test-model");
    command
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
