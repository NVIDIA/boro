// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Local execution of `grep_repo`, `rg`, `read_files`, `read_symbol`, `git_blame`, `git_diff`,
//! `git_show`, `run_git`, and (gated) `edit_file` for chat tool calls.
//!
//! Every tool uses a single directory: the git checkout boro was started with (`--source` / `-s`,
//! default current directory, resolved to the repository root). `main` also `chdir`s to that root so
//! the process cwd matches. That path is still passed explicitly as `current_dir` for subprocesses;
//! file paths in arguments are relative to that root. `..` and absolute paths are rejected.
//!
//! `edit_file` is the only write-capable tool; it is gated behind an explicit `allow_edit_file`
//! flag carried from `ToolLoopConfig`. Read-only review pipelines never see it.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const MAX_TOOL_OUTPUT: usize = 24_000;
const MAX_GIT_ARGS: usize = 48;
const MAX_ARG_LEN: usize = 256;

pub(crate) fn rg_available() -> bool {
    static RG_AVAILABLE: OnceLock<bool> = OnceLock::new();
    *RG_AVAILABLE.get_or_init(|| {
        Command::new("rg")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    })
}

pub fn openai_tools_json(include_edit_file: bool) -> Value {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "grep_repo",
                "description": "Run `git grep` in the `--source` / `-s` repo root and return file:line matches. Use this FIRST to locate symbols, identifiers, or strings — it is far cheaper than reading whole files. Follow up with read_files only on the specific lines grep found.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Search pattern. Treated as a literal string by default; set fixed_string=false to use POSIX BRE." },
                        "path_glob": { "type": "string", "description": "Optional pathspec (e.g. 'fs/*.c', 'kernel/sched/'). Empty or omitted searches the whole tree." },
                        "fixed_string": { "type": "boolean", "description": "If false, treat pattern as a regex. Defaults to true (literal)." },
                        "context_lines": { "type": "integer", "description": "Lines of context around each match (0–3). Default 0." },
                        "max_matches_per_file": { "type": "integer", "description": "Stop after N matches per file (1–50). Default 10." }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_files",
                "description": "Read files only inside the `--source` / `-s` git tree (repo root). Paths are relative to that root. Prefer calling grep_repo first to locate the lines you need, then pass tight start_line/end_line bounds here — reading whole files is expensive and rarely necessary.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string", "description": "Path relative to the `--source` repo root." },
                                    "start_line": { "type": "integer", "description": "1-based start line (optional)." },
                                    "end_line": { "type": "integer", "description": "1-based end line (optional)." },
                                    "mode": { "type": "string", "enum": ["raw", "smart"], "description": "raw = line slice; smart = key-hole view: returns the file's top-level skeleton (signatures, types, globals) with non-focus function bodies collapsed into `{ /* ... lines collapsed ... */ }` stubs. With `start_line`/`end_line`, the function(s) covering that range are kept in full." }
                                },
                                "required": ["path"]
                            }
                        }
                    },
                    "required": ["files"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_blame",
                "description": "Run `git blame` in the `--source` / `-s` checkout only; `path` is relative to that repo root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "start_line": { "type": "integer" },
                        "end_line": { "type": "integer" }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_diff",
                "description": "Run git diff with extra arguments (e.g. [\"HEAD^\", \"HEAD\", \"--\", \"path\"]). Uses --diff-algorithm=histogram.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Arguments after `git diff`; command runs in the `--source` / `-s` repo only (cwd = that root)."
                        }
                    },
                    "required": ["args"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_show",
                "description": "Run `git show` in the `--source` / `-s` repo only (cwd = that root). `object` is a commit/tag/`HEAD` or `<rev>:path`; `path` must exist in that revision's tree (repo-relative). If wrong at HEAD, use the patch commit; if still wrong, use paths from the patch or `rev^:path`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "object": {
                            "type": "string",
                            "description": "Single `git show` argument, e.g. `a1b2c3d` or `a1b2c3d:kernel/foo.c` where `kernel/foo.c` exists in that commit's tree."
                        },
                        "suppress_diff": { "type": "boolean", "description": "If true, pass --no-patch (metadata only for commits)." },
                        "start_line": { "type": "integer" },
                        "end_line": { "type": "integer" }
                    },
                    "required": ["object"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "run_git",
                "description": "Run a read-only git subcommand (log, ls-files, ls-tree, cat-file, shortlog, rev-parse, rev-list, name-rev, describe, diff-tree, whatchanged, for-each-ref, reflog, tag, branch, config --get) in the repo root. Use for compact history (`subcommand=log args=[\"--oneline\",\"-n\",\"20\",\"--\",\"<path>\"]`), tree listing (`subcommand=ls-files args=[\"<dir>/\"]`), or any plumbing query. Cheaper than git_blame for history and cheaper than grep_repo for tree navigation. Write subcommands (commit, push, fetch, merge, reset, checkout, etc.) and `-c key=value` config overrides are rejected.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "subcommand": {
                            "type": "string",
                            "description": "Single git subcommand from the read-only allowlist. Do not include the leading `git`."
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Arguments after the subcommand. May be omitted or empty."
                        }
                    },
                    "required": ["subcommand"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_symbol",
                "description": "Extract a single function / struct / enum / union / macro definition by name from a file in the `--source` repo. Returns only the definition body (~200–2000 chars typically) — cheaper than grep_repo + read_files when you already know the symbol name and file. If you do not know which file contains the symbol, call grep_repo first.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to the `--source` repo root." },
                        "symbol": { "type": "string", "description": "C identifier (max 128 chars; letters, digits, underscore)." }
                    },
                    "required": ["path", "symbol"]
                }
            }
        }),
    ];

    if rg_available() {
        tools.insert(1, json!({
            "type": "function",
            "function": {
                "name": "rg",
                "description": "Run ripgrep (`rg`) in the `--source` / `-s` repo root and return file:line matches. Prefer grep_repo for normal tracked source symbol lookups; use rg when you need ripgrep regex behavior, --glob-style filtering, or to search files git grep would not see.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Search pattern. Treated as a ripgrep regex by default; set fixed_string=true for a literal search." },
                        "path": { "type": "string", "description": "Optional repo-relative file or directory to search. Empty or omitted searches the whole tree." },
                        "glob": { "type": "string", "description": "Optional ripgrep --glob filter, e.g. '*.c', 'kernel/sched/**', or '!*.md'. Must stay repo-relative." },
                        "fixed_string": { "type": "boolean", "description": "If true, pass -F and treat pattern as a literal string. Defaults to false (regex)." },
                        "ignore_case": { "type": "boolean", "description": "If true, pass -i. Defaults to false." },
                        "context_lines": { "type": "integer", "description": "Lines of context around each match (0-3). Default 0." },
                        "max_matches_per_file": { "type": "integer", "description": "Stop after N matches per file (1-50). Default 10." }
                    },
                    "required": ["pattern"]
                }
            }
        }));
    }

    if include_edit_file {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Rewrite a file inside the `--source` repo root by replacing an exact substring. Use this when you have identified a bug in the just-applied commit and want the host to amend the commit with the fix. Path is relative to the repo root. `old_string` must occur exactly once unless `replace_all=true`. The replacement is written in place; the host detects the edit via `git status` and stages + amends the commit after your final answer.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the `--source` repo root. Absolute paths and `..` are rejected." },
                        "old_string": { "type": "string", "description": "Exact substring to replace. Must occur exactly once unless replace_all=true." },
                        "new_string": { "type": "string", "description": "Replacement substring. Must differ from old_string." },
                        "replace_all": { "type": "boolean", "description": "If true, replace every occurrence of old_string. Default false." }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        }));
    }

    Value::Array(tools)
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_TOOL_OUTPUT {
        return s.to_string();
    }
    let mut t: String = s.chars().take(MAX_TOOL_OUTPUT.saturating_sub(80)).collect();
    t.push_str("\n\n[... output truncated by boro ...]\n");
    t
}

/// Diff-aware truncation: keep the head AND the tail of `s` so the model
/// sees both ends of a `git diff` / `git show` / `git blame` output, with a
/// single line-count marker bridging the gap. Falls back to the head-only
/// [`truncate`] when the input is too pathological for line-based splitting
/// (e.g. one mega-line). Same 24 000-char hard cap.
fn truncate_diff(s: &str) -> String {
    if s.len() <= MAX_TOOL_OUTPUT {
        return s.to_string();
    }

    const MARKER_RESERVE: usize = 96;
    let budget = MAX_TOOL_OUTPUT.saturating_sub(MARKER_RESERVE);

    let lines: Vec<&str> = s.lines().collect();
    let total = lines.len();

    // Pathological inputs (one mega-line or a handful of huge lines) cannot
    // be cleanly split end-to-end: fall back to the head-only cap so we
    // don't return a near-empty string.
    if total <= 4 || s.len() / total.max(1) > budget / 4 {
        return truncate(s);
    }

    let half = budget / 2;

    let mut head_count = 0usize;
    let mut head_chars = 0usize;
    for line in &lines {
        let inc = line.len() + 1;
        if head_chars + inc > half {
            break;
        }
        head_chars += inc;
        head_count += 1;
    }
    let mut tail_count = 0usize;
    let mut tail_chars = 0usize;
    for line in lines.iter().rev() {
        let inc = line.len() + 1;
        if tail_chars + inc > half {
            break;
        }
        tail_chars += inc;
        tail_count += 1;
    }

    if head_count == 0 || tail_count == 0 {
        return truncate(s);
    }

    // Head and tail covered everything; input wasn't actually too big.
    if head_count + tail_count >= total {
        return s.to_string();
    }

    let dropped = total - head_count - tail_count;
    let mut out = String::with_capacity(head_chars + tail_chars + MARKER_RESERVE);
    for line in &lines[..head_count] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "\n[... {dropped} lines truncated by boro (head/tail kept) ...]\n\n"
    ));
    for line in &lines[total - tail_count..] {
        out.push_str(line);
        out.push('\n');
    }

    // Belt-and-braces: should never trigger given the budget math above,
    // but guarantees we never exceed the hard cap.
    if out.len() > MAX_TOOL_OUTPUT {
        return truncate(&out);
    }
    out
}

pub fn validate_repo_relative(repo_root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.contains("..") || relative.starts_with('/') {
        anyhow::bail!("invalid path: {relative}");
    }
    let base = repo_root
        .canonicalize()
        .with_context(|| format!("canonicalize repo {}", repo_root.display()))?;
    let full = base.join(relative);
    if full.exists() {
        let c = full
            .canonicalize()
            .with_context(|| format!("canonicalize {}", full.display()))?;
        if !c.starts_with(&base) {
            anyhow::bail!("path escapes repository root: {relative}");
        }
        return Ok(c);
    }
    if !full.starts_with(&base) {
        anyhow::bail!("path escapes repository root: {relative}");
    }
    Ok(full)
}

fn read_files(repo: &Path, args: &Value) -> Result<Value> {
    let files = args
        .get("files")
        .and_then(|x| x.as_array())
        .ok_or_else(|| anyhow!("read_files: missing files array"))?;
    let mut results = Vec::new();
    for f in files {
        let path_str = f.get("path").and_then(|x| x.as_str()).unwrap_or("");
        if path_str.is_empty() {
            results.push(json!({"error": "missing path"}));
            continue;
        }
        let start_line = f
            .get("start_line")
            .and_then(|x| x.as_u64())
            .map(|n| n as usize);
        let end_line = f
            .get("end_line")
            .and_then(|x| x.as_u64())
            .map(|n| n as usize);
        let mode = f.get("mode").and_then(|x| x.as_str()).unwrap_or("raw");
        match read_one_file(repo, path_str, start_line, end_line, mode) {
            Ok(v) => results.push(v),
            Err(e) => results.push(json!({"path": path_str, "error": e.to_string()})),
        }
    }
    Ok(json!({"results": results}))
}

fn read_one_file(
    repo: &Path,
    path_str: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    mode: &str,
) -> Result<Value> {
    let path = validate_repo_relative(repo, path_str)?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read_files: read {}", path.display()))?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if mode == "smart" {
        // 1-based start/end_line → 0-based end-exclusive range.
        let focus = match (start_line, end_line) {
            (Some(s), Some(e)) if s >= 1 && e >= s => Some(s.saturating_sub(1)..e.min(total_lines)),
            (Some(s), None) if s >= 1 => Some(s.saturating_sub(1)..s.min(total_lines)),
            (None, Some(e)) if e >= 1 => Some(0..e.min(total_lines)),
            _ => None,
        };
        let stripped = strip_for_braces(&lines);
        let spans = find_top_level_function_spans(&lines, &stripped);
        let (rendered, collapsed) = render_keyhole(&lines, &spans, focus.clone());
        let content = truncate(&rendered);
        let mut result = json!({
            "path": path_str,
            "content": content,
            "total_lines": total_lines,
            "mode": "smart",
            "collapsed_functions": collapsed,
        });
        if let Some(f) = focus {
            result["focus"] = json!({
                "start_line": f.start + 1,
                "end_line": f.end,
            });
        }
        return Ok(result);
    }

    let (start, end) = match (start_line, end_line) {
        (Some(s), Some(e)) if s >= 1 && e >= s => (s - 1, e.min(total_lines)),
        (Some(s), None) if s >= 1 => (s - 1, total_lines),
        (None, Some(e)) if e >= 1 => (0, e.min(total_lines)),
        (None, None) => (0, total_lines),
        _ => (0, total_lines),
    };
    let start = start.min(total_lines);
    let end = end.max(start).min(total_lines);
    if start >= total_lines {
        return Ok(json!({
            "path": path_str,
            "content": "",
            "lines_read": 0,
            "total_lines": total_lines
        }));
    }
    let slice = lines[start..end].join("\n");
    Ok(json!({
        "path": path_str,
        "content": truncate(&slice),
        "lines_read": end - start,
        "total_lines": total_lines,
        "start_line": start + 1,
        "end_line": end
    }))
}

fn grep_repo(repo: &Path, args: &Value) -> Result<Value> {
    let pattern = args
        .get("pattern")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("grep_repo: missing pattern"))?;
    if pattern.is_empty() {
        anyhow::bail!("grep_repo: empty pattern");
    }
    if pattern.len() > MAX_ARG_LEN {
        anyhow::bail!("grep_repo: pattern too long (max {MAX_ARG_LEN} chars)");
    }
    if pattern.contains('\n') || pattern.contains('\0') {
        anyhow::bail!("grep_repo: pattern contains control characters");
    }
    let path_glob = args.get("path_glob").and_then(|x| x.as_str()).unwrap_or("");
    if !path_glob.is_empty() {
        if path_glob.len() > MAX_ARG_LEN {
            anyhow::bail!("grep_repo: path_glob too long");
        }
        if path_glob.contains('\n') || path_glob.contains('\0') {
            anyhow::bail!("grep_repo: path_glob contains control characters");
        }
        if path_glob.starts_with('/') || path_glob.contains("..") {
            anyhow::bail!("grep_repo: path_glob must be repo-relative without '..'");
        }
    }
    let fixed = args
        .get("fixed_string")
        .and_then(|x| x.as_bool())
        .unwrap_or(true);
    let context_lines = args
        .get("context_lines")
        .and_then(|x| x.as_u64())
        .map(|n| n.min(3) as usize)
        .unwrap_or(0);
    let max_matches_per_file = args
        .get("max_matches_per_file")
        .and_then(|x| x.as_u64())
        .map(|n| n.clamp(1, 50) as usize)
        .unwrap_or(10);

    let mut cmd = Command::new("git");
    cmd.current_dir(repo)
        .arg("grep")
        .arg("-n")
        .arg("-I")
        .arg(format!("--max-count={max_matches_per_file}"));
    if fixed {
        cmd.arg("-F");
    } else {
        cmd.arg("-E");
    }
    if context_lines > 0 {
        cmd.arg(format!("-C{context_lines}"));
    }
    cmd.arg("-e").arg(pattern);
    if !path_glob.is_empty() {
        cmd.arg("--").arg(path_glob);
    }

    let out = cmd.output().context("git grep spawn")?;
    // git grep exits 1 when there are no matches — that's a normal "no hits" outcome, not an error.
    if !out.status.success() && out.status.code() != Some(1) {
        anyhow::bail!("git grep: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let match_count = raw
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .count();
    Ok(json!({
        "content": truncate(&raw),
        "match_count": match_count,
        "no_matches": match_count == 0,
    }))
}

fn rg(repo: &Path, args: &Value) -> Result<Value> {
    let pattern = args
        .get("pattern")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("rg: missing pattern"))?;
    if pattern.is_empty() {
        anyhow::bail!("rg: empty pattern");
    }
    if pattern.len() > MAX_ARG_LEN {
        anyhow::bail!("rg: pattern too long (max {MAX_ARG_LEN} chars)");
    }
    if pattern.contains('\n') || pattern.contains('\0') {
        anyhow::bail!("rg: pattern contains control characters");
    }

    let path = args.get("path").and_then(|x| x.as_str()).unwrap_or("");
    if !path.is_empty() {
        if path.len() > MAX_ARG_LEN {
            anyhow::bail!("rg: path too long");
        }
        if path.contains('\n') || path.contains('\0') {
            anyhow::bail!("rg: path contains control characters");
        }
        validate_repo_relative(repo, path)?;
    }

    let glob = args.get("glob").and_then(|x| x.as_str()).unwrap_or("");
    if !glob.is_empty() {
        if glob.len() > MAX_ARG_LEN {
            anyhow::bail!("rg: glob too long");
        }
        if glob.contains('\n') || glob.contains('\0') {
            anyhow::bail!("rg: glob contains control characters");
        }
        let scoped = glob.strip_prefix('!').unwrap_or(glob);
        if scoped.starts_with('/') || scoped.contains("..") {
            anyhow::bail!("rg: glob must be repo-relative without '..'");
        }
    }

    if !rg_available() {
        anyhow::bail!("rg: binary not available");
    }

    let fixed = args
        .get("fixed_string")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let ignore_case = args
        .get("ignore_case")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let context_lines = args
        .get("context_lines")
        .and_then(|x| x.as_u64())
        .map(|n| n.min(3) as usize)
        .unwrap_or(0);
    let max_matches_per_file = args
        .get("max_matches_per_file")
        .and_then(|x| x.as_u64())
        .map(|n| n.clamp(1, 50) as usize)
        .unwrap_or(10);

    let mut cmd = Command::new("rg");
    cmd.current_dir(repo)
        .arg("--line-number")
        .arg("--with-filename")
        .arg("--no-heading")
        .arg("--color")
        .arg("never")
        .arg("--max-count")
        .arg(max_matches_per_file.to_string());
    if fixed {
        cmd.arg("-F");
    }
    if ignore_case {
        cmd.arg("-i");
    }
    if context_lines > 0 {
        cmd.arg(format!("-C{context_lines}"));
    }
    if !glob.is_empty() {
        cmd.arg("--glob").arg(glob);
    }
    cmd.arg("--").arg(pattern);
    if !path.is_empty() {
        cmd.arg(path);
    }

    let out = cmd.output().context("rg spawn")?;
    // rg exits 1 when there are no matches; regex and I/O failures exit 2.
    if !out.status.success() && out.status.code() != Some(1) {
        anyhow::bail!("rg: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let match_count = raw
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .count();
    Ok(json!({
        "content": truncate(&raw),
        "match_count": match_count,
        "no_matches": match_count == 0,
    }))
}

fn git_blame(repo: &Path, args: &Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("git_blame: missing path"))?;
    validate_repo_relative(repo, path_str)?;
    let mut cmd = Command::new("git");
    cmd.current_dir(repo).arg("blame");
    if let (Some(s), Some(e)) = (
        args.get("start_line").and_then(|x| x.as_u64()),
        args.get("end_line").and_then(|x| x.as_u64()),
    ) {
        cmd.arg(format!("-L{},{}", s, e));
    }
    cmd.arg("--").arg(path_str);
    let out = cmd.output().context("git blame spawn")?;
    if !out.status.success() {
        anyhow::bail!("git blame: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    Ok(json!({"content": truncate_diff(&s)}))
}

fn validate_git_args(args: &[String]) -> Result<()> {
    if args.len() > MAX_GIT_ARGS {
        anyhow::bail!("too many git arguments (max {MAX_GIT_ARGS})");
    }
    for a in args {
        if a.len() > MAX_ARG_LEN {
            anyhow::bail!("git argument too long (max {MAX_ARG_LEN} chars)");
        }
        if a.contains('\n') || a.contains('\0') {
            anyhow::bail!("invalid git argument");
        }
    }
    Ok(())
}

fn git_diff(repo: &Path, args: &Value) -> Result<Value> {
    let raw: Vec<String> = args
        .get("args")
        .and_then(|x| x.as_array())
        .ok_or_else(|| anyhow!("git_diff: missing args"))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    validate_git_args(&raw)?;
    let out = Command::new("git")
        .current_dir(repo)
        .arg("diff")
        .arg("--diff-algorithm=histogram")
        .args(&raw)
        .output()
        .context("git diff spawn")?;
    if !out.status.success() {
        anyhow::bail!("git diff: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    Ok(json!({"content": truncate_diff(&s)}))
}

fn git_show(repo: &Path, args: &Value) -> Result<Value> {
    let object = args
        .get("object")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("git_show: missing object"))?;
    if object.len() > 512
        || object.contains('\n')
        || object.contains('\0')
        || object.starts_with('-')
    {
        anyhow::bail!("git_show: invalid object");
    }
    let suppress = args
        .get("suppress_diff")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let start_line = args
        .get("start_line")
        .and_then(|x| x.as_u64())
        .map(|n| n as usize);
    let end_line = args
        .get("end_line")
        .and_then(|x| x.as_u64())
        .map(|n| n as usize);

    let mut cmd = Command::new("git");
    cmd.current_dir(repo).arg("show");
    if suppress {
        cmd.arg("--no-patch");
    }
    cmd.arg(object);
    let out = cmd.output().context("git show spawn")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let hint = if err.contains("does not exist") {
            " Hint: the path after `:` must exist in the tree at that revision (repo-relative to `--source`). If it still fails, the path may be wrong for this commit: use paths from the patch, try the parent (`rev^:path`), or read_files once you know the correct path."
        } else {
            ""
        };
        anyhow::bail!("git show: {err}{hint}");
    }
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    if start_line.is_some() || end_line.is_some() {
        let lines: Vec<&str> = s.lines().collect();
        let total = lines.len();
        let (s0, e0) = match (start_line, end_line) {
            (Some(a), Some(b)) if a >= 1 && b >= a => (a - 1, b.min(total)),
            (Some(a), None) if a >= 1 => (a - 1, total),
            (None, Some(b)) if b >= 1 => (0, b.min(total)),
            _ => (0, total),
        };
        let s0 = s0.min(total);
        let e0 = e0.max(s0).min(total);
        s = lines[s0..e0].join("\n");
    }
    Ok(json!({"content": truncate_diff(&s)}))
}

/// Rewrite a file inside the repo by replacing an exact substring.
///
/// Path validation goes through [`validate_repo_relative`] (rejects absolute paths and `..`).
/// `old_string` must occur exactly once unless `replace_all=true`. `new_string` must differ from
/// `old_string`. The replacement is written in place; the host (apply.rs) detects tracked
/// working-tree changes via `git status` and amends them.
fn edit_file(repo: &Path, args: &Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("edit_file: missing path"))?
        .trim();
    if path_str.is_empty() {
        anyhow::bail!("edit_file: empty path");
    }
    let old = args
        .get("old_string")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("edit_file: missing old_string"))?;
    let new = args
        .get("new_string")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("edit_file: missing new_string"))?;
    let replace_all = args
        .get("replace_all")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    if old.is_empty() {
        anyhow::bail!("edit_file: old_string must not be empty");
    }
    if old == new {
        anyhow::bail!("edit_file: new_string must differ from old_string");
    }

    let path = validate_repo_relative(repo, path_str)?;
    if !path.is_file() {
        anyhow::bail!("edit_file: {path_str} is not a regular file");
    }
    let original = std::fs::read_to_string(&path)
        .with_context(|| format!("edit_file: read {}", path.display()))?;

    let occurrences = count_non_overlapping(&original, old);
    if occurrences == 0 {
        anyhow::bail!("edit_file: old_string not found in {path_str}");
    }
    if !replace_all && occurrences > 1 {
        anyhow::bail!(
            "edit_file: old_string occurs {occurrences} times in {path_str}; pass replace_all=true or extend old_string with more context to make it unique"
        );
    }

    let updated = if replace_all {
        original.replace(old, new)
    } else {
        // Single-occurrence path: use replacen to guarantee exactly one substitution.
        original.replacen(old, new, 1)
    };
    let replaced = if replace_all { occurrences } else { 1 };

    let bytes_written = updated.len();
    std::fs::write(&path, &updated)
        .with_context(|| format!("edit_file: write {}", path.display()))?;

    Ok(json!({
        "path": path_str,
        "replaced": replaced,
        "bytes_written": bytes_written,
    }))
}

/// Count non-overlapping occurrences of `needle` in `haystack`. `needle` must not be empty.
fn count_non_overlapping(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut idx = 0usize;
    while let Some(pos) = haystack[idx..].find(needle) {
        count += 1;
        idx += pos + needle.len();
        if idx >= haystack.len() {
            break;
        }
    }
    count
}

/// Run a single tool call; returns JSON text for the chat `tool` message `content`.
///
/// `allow_edit_file` gates the write-capable `edit_file` tool. Every read-only call site
/// passes `false`; only the post-apply review stage (see `apply.rs`) passes `true`.
pub fn execute_tool(
    repo: &Path,
    name: &str,
    arguments_json: &str,
    allow_edit_file: bool,
) -> Result<String> {
    let args: Value = serde_json::from_str(arguments_json.trim()).unwrap_or(json!({}));
    let out = match name.trim() {
        "read_files" => read_files(repo, &args)?,
        "grep_repo" => grep_repo(repo, &args)?,
        "rg" => rg(repo, &args)?,
        "git_blame" => git_blame(repo, &args)?,
        "git_diff" => git_diff(repo, &args)?,
        "git_show" => git_show(repo, &args)?,
        "run_git" => run_git(repo, &args)?,
        "read_symbol" => read_symbol(repo, &args)?,
        "edit_file" if allow_edit_file => edit_file(repo, &args)?,
        _ => anyhow::bail!("unknown tool: {name}"),
    };
    Ok(out.to_string())
}

/// Allowlist of read-only git subcommands callable via `run_git`. Anything not on this list
/// (notably: commit, push, merge, reset, checkout, am, apply, stash, cherry-pick, revert,
/// gc, prune, update-ref, update-index, worktree, remote, init, clone, mv, rm,
/// filter-branch, replace, notes, submodule, hash-object, fetch, pull) is rejected.
const RUN_GIT_ALLOWLIST: &[&str] = &[
    "log",
    "shortlog",
    "reflog",
    "ls-files",
    "ls-tree",
    "cat-file",
    "rev-parse",
    "rev-list",
    "name-rev",
    "describe",
    "diff-tree",
    "whatchanged",
    "for-each-ref",
    "tag",
    "branch",
    "config",
];

fn run_git(repo: &Path, args: &Value) -> Result<Value> {
    let sub = args
        .get("subcommand")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("run_git: missing subcommand"))?
        .trim();
    if sub.is_empty() {
        anyhow::bail!("run_git: empty subcommand");
    }
    if !RUN_GIT_ALLOWLIST.contains(&sub) {
        anyhow::bail!(
            "run_git: subcommand '{sub}' is not in the read-only allowlist ({})",
            RUN_GIT_ALLOWLIST.join(", ")
        );
    }
    let raw: Vec<String> = args
        .get("args")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    validate_git_args(&raw)?;
    // Reject `-c key=value` overrides — those let the caller inject arbitrary commands
    // via e.g. `core.fsmonitor=...` or `core.sshCommand=...`.
    if raw.iter().any(|a| a == "-c" || a.starts_with("--exec")) {
        anyhow::bail!("run_git: -c overrides and --exec are not allowed");
    }
    // For inherently-listing subcommands, refuse destructive flags.
    match sub {
        "tag" | "branch" => {
            for a in &raw {
                if matches!(
                    a.as_str(),
                    "-d" | "-D"
                        | "--delete"
                        | "-f"
                        | "--force"
                        | "-m"
                        | "-M"
                        | "--move"
                        | "-c"
                        | "-C"
                        | "--copy"
                ) {
                    anyhow::bail!("run_git: '{sub} {a}' is a write operation, not allowed");
                }
            }
        }
        "config" => {
            // Only allow read forms: --get / --get-all / --get-regexp / --list / -l.
            let has_read = raw.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "--get" | "--get-all" | "--get-regexp" | "--list" | "-l"
                )
            });
            if !has_read {
                anyhow::bail!(
                    "run_git: 'config' requires --get / --get-all / --get-regexp / --list"
                );
            }
            for a in &raw {
                if matches!(
                    a.as_str(),
                    "--unset"
                        | "--unset-all"
                        | "--add"
                        | "--replace-all"
                        | "--rename-section"
                        | "--remove-section"
                        | "-e"
                        | "--edit"
                ) {
                    anyhow::bail!("run_git: 'config {a}' is a write operation, not allowed");
                }
            }
        }
        _ => {}
    }

    let out = Command::new("git")
        .current_dir(repo)
        .arg(sub)
        .args(&raw)
        .output()
        .with_context(|| format!("git {sub} spawn"))?;
    if !out.status.success() {
        anyhow::bail!("git {sub}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    Ok(json!({"content": truncate(&s)}))
}

/// Extract a single C definition (function, struct, enum, union, macro) by name.
/// Returns just the definition body, typically a few hundred to a few thousand bytes —
/// much cheaper than `read_files` on a whole file or wide line range.
fn read_symbol(repo: &Path, args: &Value) -> Result<Value> {
    let path_str = args
        .get("path")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("read_symbol: missing path"))?;
    let symbol = args
        .get("symbol")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("read_symbol: missing symbol"))?;
    if symbol.is_empty() || symbol.len() > 128 {
        anyhow::bail!("read_symbol: symbol must be 1..=128 chars");
    }
    if !symbol
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        anyhow::bail!("read_symbol: symbol must be a C identifier ([A-Za-z0-9_])");
    }
    let path = validate_repo_relative(repo, path_str)?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read_symbol: read {}", path.display()))?;
    let lines: Vec<&str> = content.lines().collect();

    let Some(start_idx) = find_definition_line(&lines, symbol) else {
        return Ok(json!({
            "path": path_str,
            "symbol": symbol,
            "error": format!("no definition for '{symbol}' found in {path_str}; try grep_repo to locate it"),
        }));
    };

    const MAX_SPAN: usize = 400;
    let header = lines[start_idx];
    let (end_idx, kind) = if is_macro_definition(header, symbol) {
        // #define: stop at first line not ending in '\'.
        let mut i = start_idx;
        while i < lines.len() && i - start_idx < MAX_SPAN && lines[i].trim_end().ends_with('\\') {
            i += 1;
        }
        (i.min(lines.len() - 1), "macro")
    } else {
        // Brace-balanced span. Strip line comments and string/char literals so braces inside
        // them don't throw off the depth count.
        let mut depth: i64 = 0;
        let mut saw_open = false;
        let mut end = start_idx;
        for (offset, line) in lines.iter().enumerate().skip(start_idx).take(MAX_SPAN) {
            let stripped = strip_strings_and_line_comment(line);
            for ch in stripped.chars() {
                if ch == '{' {
                    depth += 1;
                    saw_open = true;
                } else if ch == '}' {
                    depth -= 1;
                }
            }
            end = offset;
            if saw_open && depth <= 0 {
                break;
            }
        }
        let kind = if header.contains("struct ") {
            "struct"
        } else if header.contains("enum ") {
            "enum"
        } else if header.contains("union ") {
            "union"
        } else {
            "function"
        };
        (end, kind)
    };

    let body = lines[start_idx..=end_idx].join("\n");
    Ok(json!({
        "path": path_str,
        "symbol": symbol,
        "kind": kind,
        "start_line": start_idx + 1,
        "end_line": end_idx + 1,
        "content": truncate(&body),
    }))
}

/// Find the line index (0-based) where `symbol` is defined. A qualifying line:
///   * starts at column 0 (not indented — rules out call sites and member accesses),
///   * matches one of the definition shapes for functions / aggregates / macros,
///   * does not end in `;` (rules out forward declarations and prototypes).
fn find_definition_line(lines: &[&str], symbol: &str) -> Option<usize> {
    let macro_open = format!("#define {symbol}(");
    let macro_value = format!("#define {symbol} ");
    let macro_bare = format!("#define {symbol}");
    let struct_open = format!("struct {symbol} ");
    let struct_brace = format!("struct {symbol}{{");
    let enum_open = format!("enum {symbol} ");
    let enum_brace = format!("enum {symbol}{{");
    let union_open = format!("union {symbol} ");
    let union_brace = format!("union {symbol}{{");

    for (i, raw) in lines.iter().enumerate() {
        // Reject indentation: definitions live at column 0.
        if raw.starts_with(' ') || raw.starts_with('\t') {
            continue;
        }
        let line = *raw;
        let trimmed_end = line.trim_end();

        // Macros — single-line or backslash-continued.
        if line.starts_with(&macro_open)
            || line.starts_with(&macro_value)
            || line == macro_bare
            || line.starts_with(&format!("{macro_bare}\t"))
        {
            return Some(i);
        }

        // Aggregates (must reach a `{` either on this line or rule out `;`).
        if line.starts_with(&struct_open)
            || line.starts_with(&struct_brace)
            || line.starts_with(&enum_open)
            || line.starts_with(&enum_brace)
            || line.starts_with(&union_open)
            || line.starts_with(&union_brace)
        {
            if trimmed_end.ends_with(';') {
                continue;
            }
            return Some(i);
        }

        // Function-like: `<symbol>(` appears, and the line doesn't end with `;`.
        // We additionally require that the token preceding `(` is exactly `symbol`
        // (not e.g. `foo_bar` when searching for `bar`).
        if let Some(pos) = line.find(&format!("{symbol}(")) {
            let before = &line[..pos];
            let after_ok = pos + symbol.len() < line.len();
            let boundary_ok = before
                .chars()
                .last()
                .map(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(true);
            if !after_ok || !boundary_ok {
                continue;
            }
            // Forward declaration / prototype: skip.
            if trimmed_end.ends_with(';') {
                continue;
            }
            // Looks like a call expression: `foo(` preceded by `=` or operator and ending
            // with `;` was caught above; also skip lines that are clearly not signatures
            // (e.g. inside a string). The column-0 rule already filters most of these.
            return Some(i);
        }
    }
    None
}

fn is_macro_definition(line: &str, symbol: &str) -> bool {
    line.starts_with(&format!("#define {symbol}("))
        || line.starts_with(&format!("#define {symbol} "))
        || line.starts_with(&format!("#define {symbol}\t"))
        || line == format!("#define {symbol}")
}

/// 0-based, inclusive line span of a top-level function definition. `body_open` is
/// the line that first contains `{` (it may also contain the tail of the signature
/// like `int foo(void) {`); `body_end` is the line of the matching `}`.
#[derive(Clone, Copy, Debug)]
struct FuncSpan {
    sig_start: usize,
    body_open: usize,
    body_end: usize,
}

const KEYHOLE_MAX_FUNC_LINES: usize = 2000;
const KEYHOLE_MAX_SIGNATURE_LOOKAHEAD: usize = 20;

/// Whole-file cousin of [`strip_strings_and_line_comment`] - emits brace-safe
/// per-line copies that track multi-line `/* ... */` block comments. Used by
/// the key-hole renderer so a `{` inside a comment or string literal does not
/// confuse the function-span detector.
fn strip_for_braces(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut in_block = false;
    for &line in lines {
        let mut s = String::with_capacity(line.len());
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if in_block {
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    in_block = false;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            let c = bytes[i];
            if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                break;
            }
            if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                in_block = true;
                i += 2;
                continue;
            }
            if c == b'"' || c == b'\'' {
                let quote = c;
                i += 1;
                while i < bytes.len() {
                    let ch = bytes[i];
                    if ch == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if ch == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            if c.is_ascii() {
                s.push(c as char);
            } else {
                s.push(' ');
            }
            i += 1;
        }
        out.push(s);
    }
    out
}

/// Locate top-level function-definition spans. Detection is conservative
/// (kernel C style) - if anything looks ambiguous we skip the candidate
/// rather than risk collapsing the wrong region.
fn find_top_level_function_spans(lines: &[&str], stripped: &[String]) -> Vec<FuncSpan> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with(' ') || line.starts_with('\t') {
            i += 1;
            continue;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
            || trimmed.starts_with('}')
            || trimmed.starts_with('{')
            || !trimmed.contains('(')
        {
            i += 1;
            continue;
        }

        let look_end = lines.len().min(i + KEYHOLE_MAX_SIGNATURE_LOOKAHEAD);
        let mut body_open: Option<usize> = None;
        let mut is_prototype = false;
        for (k, sk) in stripped.iter().enumerate().take(look_end).skip(i) {
            if sk.contains('{') {
                body_open = Some(k);
                break;
            }
            if sk.trim_end().ends_with(';') {
                is_prototype = true;
                break;
            }
        }
        if is_prototype || body_open.is_none() {
            i += 1;
            continue;
        }
        let body_open = body_open.unwrap();
        if has_assignment_before_body_open(stripped, i, body_open) {
            i += 1;
            continue;
        }

        let mut depth: i64 = 0;
        let mut saw_open = false;
        let mut end = body_open;
        let mut closed = false;
        let span_end = lines.len().min(body_open + KEYHOLE_MAX_FUNC_LINES);
        for (k, sk) in stripped.iter().enumerate().take(span_end).skip(body_open) {
            for ch in sk.chars() {
                if ch == '{' {
                    depth += 1;
                    saw_open = true;
                } else if ch == '}' {
                    depth -= 1;
                }
            }
            end = k;
            if saw_open && depth <= 0 {
                closed = true;
                break;
            }
        }
        if !closed {
            i = end + 1;
            continue;
        }

        out.push(FuncSpan {
            sig_start: i,
            body_open,
            body_end: end,
        });
        i = end + 1;
    }
    out
}

fn has_assignment_before_body_open(
    stripped: &[String],
    sig_start: usize,
    body_open: usize,
) -> bool {
    for (k, line) in stripped
        .iter()
        .enumerate()
        .take(body_open + 1)
        .skip(sig_start)
    {
        let before_brace = if k == body_open {
            line.split_once('{')
                .map(|(before, _)| before)
                .unwrap_or(line)
        } else {
            line.as_str()
        };
        if before_brace.contains('=') {
            return true;
        }
    }
    false
}

/// Render `lines` with every function span that does NOT overlap `focus`
/// replaced by a one-line `{ /* ... N lines collapsed by boro ... */ }` stub.
/// `focus` is 0-based, end-exclusive. `None` collapses every function body
/// (skeleton-only view). Returns the rendered text plus the number of
/// bodies that were collapsed.
fn render_keyhole(
    lines: &[&str],
    spans: &[FuncSpan],
    focus: Option<std::ops::Range<usize>>,
) -> (String, usize) {
    let mut out = String::with_capacity(lines.len() * 32);
    let mut collapsed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        if let Some(span) = spans.iter().find(|s| s.sig_start == i) {
            let overlaps = match &focus {
                Some(f) => span.body_end >= f.start && span.sig_start < f.end,
                None => false,
            };
            if overlaps {
                for line in &lines[span.sig_start..=span.body_end] {
                    out.push_str(line);
                    out.push('\n');
                }
            } else {
                for line in &lines[span.sig_start..span.body_open] {
                    out.push_str(line);
                    out.push('\n');
                }
                let body_open_line = lines[span.body_open];
                if let Some(brace_pos) = body_open_line.find('{') {
                    out.push_str(&body_open_line[..brace_pos]);
                }
                let body_lines = span.body_end - span.body_open + 1;
                out.push_str("{ /* ... ");
                out.push_str(&body_lines.to_string());
                out.push_str(" lines collapsed by boro ... */ }\n");
                collapsed += 1;
            }
            i = span.body_end + 1;
        } else {
            out.push_str(lines[i]);
            out.push('\n');
            i += 1;
        }
    }
    (out, collapsed)
}

/// Strip `//` line comments and the interior of string / char literals so that the brace
/// counter in `read_symbol` is not confused by `'{'`, `"}"`, or `// {` inside source code.
/// Block comments (`/* ... */`) are not handled across multiple calls; in kernel C they
/// are rarely opened inside a function signature/body without closing on the same line,
/// and the brace span has a 400-line safety ceiling either way.
fn strip_strings_and_line_comment(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        // Line comment: drop the rest.
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            break;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            out.push(' ');
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if ch == '\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if ch == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, content: &str) {
        fs::write(dir.path().join(name), content).unwrap();
    }

    fn run_git_test(dir: &TempDir, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_repo_with_commit() -> TempDir {
        let d = TempDir::new().unwrap();
        run_git_test(&d, &["init"]);
        run_git_test(&d, &["config", "user.email", "test@example.com"]);
        run_git_test(&d, &["config", "user.name", "Test User"]);
        write_file(&d, "f.c", "int x = 1;\n");
        run_git_test(&d, &["add", "f.c"]);
        run_git_test(&d, &["commit", "-m", "base"]);
        d
    }

    fn smart(dir: &TempDir, name: &str, start: Option<usize>, end: Option<usize>) -> Value {
        let args = json!({
            "files": [{
                "path": name,
                "start_line": start,
                "end_line": end,
                "mode": "smart",
            }]
        });
        let v = read_files(dir.path(), &args).unwrap();
        v["results"][0].clone()
    }

    const THREE_FUNCS: &str = "\
#include <linux/kernel.h>

int alpha(int x)
{
\tint y = x + 1;
\treturn y;
}

int beta(int x)
{
\tint y = x * 2;
\treturn y;
}

int gamma(int x)
{
\tint y = x - 3;
\treturn y;
}
";

    #[test]
    fn smart_collapses_unrelated_function_bodies() {
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", THREE_FUNCS);
        // line 10 is inside `beta`.
        let r = smart(&d, "f.c", Some(10), Some(10));
        let c = r["content"].as_str().unwrap();
        assert_eq!(r["collapsed_functions"].as_u64().unwrap(), 2);
        assert!(c.contains("int alpha(int x)"), "alpha sig kept: {c}");
        assert!(c.contains("int beta(int x)"), "beta sig kept: {c}");
        assert!(c.contains("int gamma(int x)"), "gamma sig kept: {c}");
        assert!(
            c.contains("int y = x * 2;"),
            "beta body preserved in full: {c}"
        );
        assert!(
            !c.contains("int y = x + 1;"),
            "alpha body should be collapsed: {c}"
        );
        assert!(
            !c.contains("int y = x - 3;"),
            "gamma body should be collapsed: {c}"
        );
        assert!(c.contains("lines collapsed by boro"));
    }

    #[test]
    fn smart_with_no_focus_collapses_all() {
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", THREE_FUNCS);
        let r = smart(&d, "f.c", None, None);
        let c = r["content"].as_str().unwrap();
        assert_eq!(r["collapsed_functions"].as_u64().unwrap(), 3);
        assert!(c.contains("int alpha(int x)"));
        assert!(c.contains("int beta(int x)"));
        assert!(c.contains("int gamma(int x)"));
        assert!(!c.contains("int y = x + 1;"));
        assert!(!c.contains("int y = x * 2;"));
        assert!(!c.contains("int y = x - 3;"));
        assert!(c.matches("lines collapsed by boro").count() == 3);
    }

    #[test]
    fn smart_keeps_top_level_structs() {
        let d = TempDir::new().unwrap();
        let src = "\
struct widget {
\tint id;
\tconst char *name;
};

int widget_init(struct widget *w)
{
\tw->id = 0;
\treturn 0;
}
";
        write_file(&d, "w.c", src);
        let r = smart(&d, "w.c", None, None);
        let c = r["content"].as_str().unwrap();
        assert!(c.contains("struct widget {"));
        assert!(c.contains("int id;"));
        assert!(c.contains("const char *name;"));
        assert!(c.contains("int widget_init(struct widget *w)"));
        assert!(!c.contains("w->id = 0;"));
        assert_eq!(r["collapsed_functions"].as_u64().unwrap(), 1);
    }

    #[test]
    fn smart_preserves_function_pointer_initializer_tables() {
        let d = TempDir::new().unwrap();
        let src = "\
static int alpha(int x)
{
\treturn x + 1;
}

static int beta(int x)
{
\treturn x + 2;
}

static int (*handlers[])(int) = {
\talpha,
\tbeta,
};

int dispatch(int x)
{
\treturn handlers[0](x);
}
";
        write_file(&d, "table.c", src);
        let r = smart(&d, "table.c", None, None);
        let c = r["content"].as_str().unwrap();
        assert_eq!(r["collapsed_functions"].as_u64().unwrap(), 3);
        assert!(c.contains("static int (*handlers[])(int) = {"));
        assert!(c.contains("\talpha,"));
        assert!(c.contains("\tbeta,"));
        assert!(!c.contains("return handlers[0](x);"));
    }

    #[test]
    fn smart_handles_brace_in_string_and_block_comment() {
        let d = TempDir::new().unwrap();
        let src = "\
int tricky(void)
{
\tconst char *s = \"}\";
\t/* not a brace: { */
\treturn 0;
}

int after(void)
{
\treturn 1;
}
";
        write_file(&d, "t.c", src);
        // Focus on `tricky` — `after` must be detected as its own span and collapsed.
        let r = smart(&d, "t.c", Some(2), Some(2));
        let c = r["content"].as_str().unwrap();
        assert_eq!(r["collapsed_functions"].as_u64().unwrap(), 1);
        assert!(c.contains("int tricky(void)"));
        assert!(c.contains("const char *s = \"}\";"));
        assert!(c.contains("int after(void)"));
        assert!(!c.contains("return 1;"));
    }

    #[test]
    fn smart_preserves_headers_only_file() {
        let d = TempDir::new().unwrap();
        let src = "\
#include <linux/types.h>
#include <linux/list.h>

#define FOO 1
#define BAR(x) ((x) + 1)

struct opaque;
";
        write_file(&d, "h.h", src);
        let r = smart(&d, "h.h", None, None);
        let c = r["content"].as_str().unwrap();
        assert_eq!(r["collapsed_functions"].as_u64().unwrap(), 0);
        assert!(c.contains("#include <linux/types.h>"));
        assert!(c.contains("#define FOO 1"));
        assert!(c.contains("#define BAR(x) ((x) + 1)"));
        assert!(c.contains("struct opaque;"));
        assert!(!c.contains("lines collapsed by boro"));
    }

    #[test]
    fn smart_output_respects_max_tool_output() {
        let d = TempDir::new().unwrap();
        // Many short functions whose uncollapsed form would exceed MAX_TOOL_OUTPUT.
        // No focus → every body collapses; but the signatures alone must still fit.
        // Use enough functions that the rendered output exceeds the cap.
        let mut src = String::new();
        for i in 0..2000 {
            src.push_str(&format!(
                "int func_{i}_with_a_pretty_long_name_to_burn_chars(int x)\n{{\n\treturn x + {i};\n}}\n\n"
            ));
        }
        write_file(&d, "big.c", &src);
        let r = smart(&d, "big.c", None, None);
        let c = r["content"].as_str().unwrap();
        assert!(
            c.len() <= MAX_TOOL_OUTPUT,
            "content len {} exceeds cap",
            c.len()
        );
        assert!(c.contains("[... output truncated by boro ...]"));
    }

    #[test]
    fn truncate_diff_passthrough_when_small() {
        let s = "diff --git a/foo b/foo\n@@ -1 +1 @@\n-old\n+new\n";
        let out = truncate_diff(s);
        assert_eq!(out, s);
        assert!(!out.contains("truncated by boro"));
    }

    #[test]
    fn truncate_diff_keeps_head_and_tail() {
        // Build an oversized diff with distinctive markers at both ends.
        // Filler is short enough that line-based head/tail kicks in.
        let mut s = String::new();
        s.push_str("HEAD_LINE_A\n");
        s.push_str("HEAD_LINE_B\n");
        // ~30k chars of filler in 60-char lines.
        for i in 0..600 {
            s.push_str(&format!("+ filler row {i:04} {x}\n", x = "x".repeat(40)));
        }
        s.push_str("TAIL_LINE_Y\n");
        s.push_str("TAIL_LINE_Z\n");
        assert!(s.len() > MAX_TOOL_OUTPUT);

        let out = truncate_diff(&s);
        assert!(
            out.len() <= MAX_TOOL_OUTPUT,
            "len {} exceeds cap",
            out.len()
        );
        assert!(out.contains("HEAD_LINE_A"), "head missing: {out:.200}");
        assert!(out.contains("HEAD_LINE_B"));
        assert!(out.contains("TAIL_LINE_Y"));
        assert!(out.contains("TAIL_LINE_Z"), "tail missing");
        assert!(out.contains("lines truncated by boro (head/tail kept)"));
        // The fallback head-only marker must NOT appear.
        assert!(!out.contains("[... output truncated by boro ...]"));
    }

    #[test]
    fn truncate_diff_falls_back_on_huge_single_line() {
        // 200 000 bytes, one line: line-based split is impossible.
        let s = "a".repeat(200_000);
        let out = truncate_diff(&s);
        assert!(out.len() <= MAX_TOOL_OUTPUT);
        assert!(out.contains("[... output truncated by boro ...]"));
        assert!(!out.contains("head/tail kept"));
    }

    #[test]
    fn truncate_diff_falls_back_when_one_side_cannot_fit_a_line() {
        let mut s = String::new();
        s.push_str(&"h".repeat(MAX_TOOL_OUTPUT / 2 + 500));
        s.push('\n');
        for i in 0..40 {
            s.push_str(&format!("+ middle {i:02} {x}\n", x = "x".repeat(300)));
        }
        s.push_str("TAIL_MARKER\n");
        assert!(s.len() > MAX_TOOL_OUTPUT);

        let out = truncate_diff(&s);
        assert!(out.len() <= MAX_TOOL_OUTPUT);
        assert!(out.contains("[... output truncated by boro ...]"));
        assert!(!out.contains("head/tail kept"));
    }

    #[test]
    fn truncate_diff_respects_max_tool_output() {
        // Many short diff-ish lines so the result is definitely capped.
        let mut s = String::new();
        for i in 0..100_000 {
            s.push_str(&format!("+ line {i:06}\n"));
        }
        let out = truncate_diff(&s);
        assert!(out.len() <= MAX_TOOL_OUTPUT);
        assert!(out.contains("lines truncated by boro (head/tail kept)"));
    }

    #[test]
    fn openai_tools_json_omits_edit_file_by_default() {
        let v = openai_tools_json(false);
        let arr = v.as_array().unwrap();
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(!names.contains(&"edit_file"), "names: {names:?}");
    }

    #[test]
    fn openai_tools_json_includes_edit_file_when_gated_on() {
        let v = openai_tools_json(true);
        let arr = v.as_array().unwrap();
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"edit_file"), "names: {names:?}");
    }

    #[test]
    fn openai_tools_json_advertises_rg_only_when_available() {
        let v = openai_tools_json(false);
        let arr = v.as_array().unwrap();
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert_eq!(names.contains(&"rg"), rg_available(), "names: {names:?}");
    }

    #[test]
    fn execute_tool_runs_rg_when_available() {
        if !rg_available() {
            return;
        }
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", "int alpha;\nint needle;\n");
        let args = json!({
            "pattern": "needle",
            "fixed_string": true,
        })
        .to_string();
        let out = execute_tool(d.path(), "rg", &args, false).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(content.contains("f.c:2:int needle;"), "content: {content}");
        assert!(!v["no_matches"].as_bool().unwrap());
    }

    #[test]
    fn rg_rejects_dotdot_path() {
        let d = TempDir::new().unwrap();
        let args = json!({
            "pattern": "needle",
            "path": "../escape.c",
        });
        let err = rg(d.path(), &args).unwrap_err();
        assert!(err.to_string().contains("invalid path"), "err: {err}");
    }

    #[test]
    fn git_show_rejects_option_object_before_spawning_git() {
        let d = git_repo_with_commit();
        let leak = d.path().join("leak.diff");
        let args = json!({
            "object": "--output=leak.diff",
        });
        let err = git_show(d.path(), &args).unwrap_err();
        assert!(err.to_string().contains("invalid object"), "err: {err}");
        assert!(!leak.exists(), "git_show created {}", leak.display());
    }

    #[test]
    fn execute_tool_refuses_edit_file_without_gate() {
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", "int x = 1;\n");
        let args = json!({
            "path": "f.c",
            "old_string": "int x = 1;",
            "new_string": "int x = 2;",
        })
        .to_string();
        let err = execute_tool(d.path(), "edit_file", &args, false).unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "err: {err}");
        // File untouched.
        let after = std::fs::read_to_string(d.path().join("f.c")).unwrap();
        assert_eq!(after, "int x = 1;\n");
    }

    #[test]
    fn edit_file_rewrites_unique_match() {
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", "int x = 1;\nint y = 2;\n");
        let args = json!({
            "path": "f.c",
            "old_string": "int x = 1;",
            "new_string": "int x = 42;",
        });
        let res = edit_file(d.path(), &args).unwrap();
        assert_eq!(res["replaced"].as_u64().unwrap(), 1);
        assert_eq!(res["path"].as_str().unwrap(), "f.c");
        let after = std::fs::read_to_string(d.path().join("f.c")).unwrap();
        assert_eq!(after, "int x = 42;\nint y = 2;\n");
    }

    #[test]
    fn edit_file_rejects_non_unique_match() {
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", "int x = 1;\nint x = 1;\n");
        let args = json!({
            "path": "f.c",
            "old_string": "int x = 1;",
            "new_string": "int x = 2;",
        });
        let err = edit_file(d.path(), &args).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("occurs 2 times"), "err: {msg}");
        // File untouched.
        let after = std::fs::read_to_string(d.path().join("f.c")).unwrap();
        assert_eq!(after, "int x = 1;\nint x = 1;\n");
    }

    #[test]
    fn edit_file_replace_all_replaces_every_occurrence() {
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", "FOO\nbar\nFOO\nbaz\nFOO\n");
        let args = json!({
            "path": "f.c",
            "old_string": "FOO",
            "new_string": "QUUX",
            "replace_all": true,
        });
        let res = edit_file(d.path(), &args).unwrap();
        assert_eq!(res["replaced"].as_u64().unwrap(), 3);
        let after = std::fs::read_to_string(d.path().join("f.c")).unwrap();
        assert_eq!(after, "QUUX\nbar\nQUUX\nbaz\nQUUX\n");
    }

    #[test]
    fn edit_file_rejects_absolute_path() {
        let d = TempDir::new().unwrap();
        let args = json!({
            "path": "/etc/passwd",
            "old_string": "root",
            "new_string": "evil",
        });
        let err = edit_file(d.path(), &args).unwrap_err();
        assert!(err.to_string().contains("invalid path"), "err: {err}");
    }

    #[test]
    fn edit_file_rejects_dotdot_path() {
        let d = TempDir::new().unwrap();
        let args = json!({
            "path": "../escape.c",
            "old_string": "x",
            "new_string": "y",
        });
        let err = edit_file(d.path(), &args).unwrap_err();
        assert!(err.to_string().contains("invalid path"), "err: {err}");
    }

    #[test]
    fn edit_file_rejects_missing_substring() {
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", "int x = 1;\n");
        let args = json!({
            "path": "f.c",
            "old_string": "int y = 1;",
            "new_string": "int y = 2;",
        });
        let err = edit_file(d.path(), &args).unwrap_err();
        assert!(err.to_string().contains("not found"), "err: {err}");
    }

    #[test]
    fn edit_file_rejects_identical_strings() {
        let d = TempDir::new().unwrap();
        write_file(&d, "f.c", "int x = 1;\n");
        let args = json!({
            "path": "f.c",
            "old_string": "int x = 1;",
            "new_string": "int x = 1;",
        });
        let err = edit_file(d.path(), &args).unwrap_err();
        assert!(err.to_string().contains("must differ"), "err: {err}");
    }

    #[test]
    fn count_non_overlapping_handles_overlap_candidates() {
        assert_eq!(count_non_overlapping("aaaa", "aa"), 2);
        assert_eq!(count_non_overlapping("hello hello", "hello"), 2);
        assert_eq!(count_non_overlapping("", "x"), 0);
    }
}
