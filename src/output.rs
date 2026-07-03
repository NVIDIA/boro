// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Human-readable report.
//!
//! `Usage & stats` (token / wall-time accounting) is written to **stderr** so that the stdout
//! capture from `boro` is just the model-derived signal — `Findings` and the LKML-style report —
//! suitable for piping into another agent. Everything else stays on stdout.

use owo_colors::OwoColorize;
use serde_json::Value;
use std::io::{self, IsTerminal};

use crate::api;

/// `pln!(true, "...")` writes to stderr; `pln!(false, "...")` writes to stdout. Used by the report
/// helpers so the same code path can be reused for the stderr-bound stats block and the
/// stdout-bound findings / LKML blocks.
macro_rules! pln {
    ($err:expr) => {
        if $err {
            eprintln!()
        } else {
            println!()
        }
    };
    ($err:expr, $($arg:tt)*) => {
        if $err {
            eprintln!($($arg)*)
        } else {
            println!($($arg)*)
        }
    };
}

/// Apply ANSI styling when `$color` is true; otherwise emit `$s` as a plain `String`.
///
/// Implemented as a macro because `owo-colors`'s wrapper types borrow their argument, and a
/// generic `Fn(&str) -> impl Display` closure can't express that lifetime relationship in
/// stable Rust. Inlining the style chain in the caller's scope sidesteps the HRTB inference
/// problem and keeps every call site readable.
macro_rules! paint {
    ($s:expr, $color:expr, |$arg:ident| $body:expr) => {{
        let $arg = $s;
        if $color {
            $body.to_string()
        } else {
            $arg.to_string()
        }
    }};
}

const WIDTH: usize = 72;
/// Max terminal width for wrapped finding lines (problem text and severity explanation).
const FINDING_LINE_MAX: usize = 100;
/// Indent for continuation lines of a finding (matches severity-explanation indent).
const FINDING_CONT_INDENT: &str = "         ";

fn use_color() -> bool {
    io::stdout().is_terminal()
}

fn use_color_stderr() -> bool {
    io::stderr().is_terminal()
}

/// Emit the run header (range + regular model, plus strong fast/validation model
/// when set and distinct from the main model) to stderr at startup so it's visible
/// before any review work.
pub fn eprint_run_header(range: &str, model: &str, validation_model: Option<&str>) {
    let c = use_color_stderr();
    let model_disp = if model.is_empty() {
        "(backend default)"
    } else {
        model
    };
    eprintln!(
        "  {} {}",
        paint!("Range:", c, |s| s.bold().dimmed()),
        paint!(range, c, |s| s.bright_white())
    );
    eprintln!(
        "  {} {}",
        paint!("Model:", c, |s| s.bold().dimmed()),
        paint!(model_disp, c, |s| s.bright_white())
    );
    if let Some(validation) = validation_model {
        let validation_disp = if validation.is_empty() {
            "(backend default)"
        } else {
            validation
        };
        eprintln!(
            "  {} {}",
            paint!("Fast/validation:", c, |s| s.bold().dimmed()),
            paint!(validation_disp, c, |s| s.bright_white())
        );
    }
    eprintln!();
}

pub fn print_report_human(out: &Value) {
    let commits = commits_slice(out);
    print_stats_block(out, &commits);
    print_quick_summary_block(out);
    let mode = out.get("validation_mode").and_then(|v| v.as_str());
    let fast = !commits.is_empty()
        && commits
            .iter()
            .all(|commit| commit.get("fast").and_then(Value::as_bool) == Some(true));
    // Both filter and findings modes populate `validated_findings[]`; the
    // Findings section prefers those over raw findings for both.
    let use_validated = matches!(mode, Some("findings") | Some("filter"));
    // Fast output is already the final prose. Its intentionally empty structured
    // findings array must not produce a misleading "no findings" section above it.
    if !fast {
        print_findings_block(&commits, use_validated);
    }

    // Findings mode skips the per-commit LKML pass entirely; the JSON
    // viewer renders validated_findings inline. Every other mode (off /
    // filter) emits per-commit lkml_report; print whatever's on each
    // commit (rendered from raw findings under `off`, from survivors
    // under `filter`).
    if mode != Some("findings") {
        print_lkml_block(&commits);
    }
}

/// Write `out` as a pretty-printed JSON document to `w`. Trailing newline included.
///
/// Factored out from [`print_report_json`] so tests can supply a `Vec<u8>` and inspect
/// the bytes. The serializer can only fail on a write error, not on the `Value` itself.
pub fn write_report_json<W: io::Write>(w: &mut W, out: &Value) -> io::Result<()> {
    let s = serde_json::to_string_pretty(out).unwrap_or_else(|_| "{}".to_string());
    writeln!(w, "{s}")
}

/// Stdout sink for `--json`. Silently swallows broken-pipe errors so
/// `boro --json | head` doesn't panic.
pub fn print_report_json(out: &Value) {
    let _ = write_report_json(&mut io::stdout(), out);
}

fn commits_slice(out: &Value) -> Vec<&Value> {
    out.get("commits")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// All token / usage figures first (run-wide summary, then each commit).
fn print_stats_block(out: &Value, commits: &[&Value]) {
    // Stats go to stderr — stdout is reserved for findings + LKML so the report can be piped
    // into another agent / LLM as a clean payload.
    const TO_ERR: bool = true;
    section_title("Usage & stats", use_color(), TO_ERR);

    if let Some(summary) = out.get("usage_summary") {
        let c = use_color();
        pln!(
            TO_ERR,
            "  {}",
            paint!("Run-wide (all commits)", c, |s| s.bold().bright_green())
        );
        print_usage_card(summary, "    ", c, TO_ERR);
        if let Some(ms) = out.get("wall_ms").and_then(|v| v.as_u64()) {
            pln!(
                TO_ERR,
                "    {} {}",
                paint!("Wall time:", c, |s| s.bold().dimmed()),
                paint!(&fmt_wall_ms(ms), c, |s| s.bright_cyan()),
            );
        }
        pln!(TO_ERR);
    }

    if let Some(validation) = out.get("validation_usage") {
        let c = use_color();
        let model = validation
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let header = match model {
            Some(m) => format!("Validation ({m})"),
            None => "Validation".to_string(),
        };
        pln!(
            TO_ERR,
            "  {}",
            paint!(&header, c, |s| s.bold().bright_cyan())
        );
        print_usage_card(validation, "    ", c, TO_ERR);
        if let Some(steps) = validation.get("usage_steps").and_then(|s| s.as_array()) {
            if !steps.is_empty() {
                pln!(
                    TO_ERR,
                    "    {}",
                    paint!("API steps", c, |s| s.bold().dimmed())
                );
                print_step_table(steps, "      ", c, TO_ERR);
            }
        }
        if let Some(ms) = validation.get("wall_ms").and_then(|v| v.as_u64()) {
            pln!(
                TO_ERR,
                "    {} {}",
                paint!("Wall time:", c, |s| s.bold().dimmed()),
                paint!(&fmt_wall_ms(ms), c, |s| s.bright_cyan()),
            );
        }
        pln!(TO_ERR);
    }

    let mut any_commit_stats = false;
    for c in commits {
        if c.get("dry_run").and_then(|v| v.as_bool()) == Some(true) {
            any_commit_stats = true;
            let co = use_color();
            let commit = commit_title(c);
            pln!(
                TO_ERR,
                "  {} {}",
                paint!("Commit", co, |s| s.bold().dimmed()),
                paint!(&commit, co, |s| s.yellow())
            );
            // Review commits stamp `reference_chars`/`patch_chars` on dry-run; build /
            // test don't (they have nothing of that shape). Show the appropriate line.
            if c.get("reference_chars").is_some() || c.get("patch_chars").is_some() {
                let rc = c
                    .get("reference_chars")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let pc = c.get("patch_chars").and_then(|v| v.as_u64()).unwrap_or(0);
                pln!(
                    TO_ERR,
                    "    {}  reference_chars={}  patch_chars={}",
                    paint!("(dry run)", co, |s| s.italic().dimmed()),
                    paint!(&rc.to_string(), co, |s| s.bright_white()),
                    paint!(&pc.to_string(), co, |s| s.bright_white())
                );
            } else {
                pln!(
                    TO_ERR,
                    "    {}  (no API call, no `vng` invocation)",
                    paint!("(dry run)", co, |s| s.italic().dimmed()),
                );
            }
            pln!(TO_ERR);
            continue;
        }

        if c.get("error").and_then(|v| v.as_str()).is_some() {
            any_commit_stats = true;
            let co = use_color();
            let commit = commit_title(c);
            pln!(
                TO_ERR,
                "  {} {}",
                paint!("Commit", co, |s| s.bold().dimmed()),
                paint!(&commit, co, |s| s.yellow())
            );
            pln!(
                TO_ERR,
                "    {}",
                paint!("(review failed — see Findings for details)", co, |s| s
                    .red())
            );
            pln!(TO_ERR);
            continue;
        }

        if c.get("usage").is_none() && c.get("usage_steps").is_none() {
            continue;
        }
        any_commit_stats = true;
        let co = use_color();
        let commit = commit_title(c);
        pln!(
            TO_ERR,
            "  {} {}",
            paint!("Commit", co, |s| s.bold().dimmed()),
            paint!(&commit, co, |s| s.yellow())
        );
        if let Some(u) = c.get("usage") {
            print_usage_card(u, "    ", co, TO_ERR);
        }
        if let Some(steps) = c.get("usage_steps").and_then(|s| s.as_array()) {
            if !steps.is_empty() {
                pln!(
                    TO_ERR,
                    "    {}",
                    paint!("API steps", co, |s| s.bold().dimmed())
                );
                print_step_table(steps, "      ", co, TO_ERR);
            }
        }
        if let Some(ms) = c.get("wall_ms").and_then(|v| v.as_u64()) {
            pln!(
                TO_ERR,
                "    {} {}",
                paint!("Wall time:", co, |s| s.bold().dimmed()),
                paint!(&fmt_wall_ms(ms), co, |s| s.bright_cyan()),
            );
        }
        pln!(TO_ERR);
    }

    if !any_commit_stats && out.get("usage_summary").is_none() {
        pln!(
            TO_ERR,
            "  {}",
            paint!("(no usage data)", use_color(), |s| s.dimmed())
        );
        pln!(TO_ERR);
    }

    subsep(use_color(), TO_ERR);
}

/// Token / usage figures for `boro apply` human output.
///
/// Kept in this module so `apply` reuses the same usage card and per-step table formatting as
/// `review` / `build` / `test`, while still keeping the stats block on stderr.
pub fn eprint_apply_stats(out: &Value) {
    const TO_ERR: bool = true;
    let co = use_color_stderr();
    section_title("Usage & stats", co, TO_ERR);

    let mut any_stats = false;
    if let Some(summary) = out.get("usage_summary") {
        any_stats = true;
        pln!(
            TO_ERR,
            "  {}",
            paint!("Run-wide (apply)", co, |s| s.bold().bright_green())
        );
        print_usage_card(summary, "    ", co, TO_ERR);
        if let Some(ms) = out.get("wall_ms").and_then(|v| v.as_u64()) {
            pln!(
                TO_ERR,
                "    {} {}",
                paint!("Wall time:", co, |s| s.bold().dimmed()),
                paint!(&fmt_wall_ms(ms), co, |s| s.bright_cyan()),
            );
        }
        pln!(TO_ERR);
    }

    if let Some(steps) = out.get("usage_steps").and_then(|s| s.as_array()) {
        if !steps.is_empty() {
            any_stats = true;
            pln!(
                TO_ERR,
                "  {}",
                paint!("API steps", co, |s| s.bold().bright_cyan())
            );
            print_step_table(steps, "    ", co, TO_ERR);
            pln!(TO_ERR);
        }
    }

    if !any_stats {
        pln!(
            TO_ERR,
            "  {}",
            paint!("(no usage data)", co, |s| s.dimmed())
        );
        pln!(TO_ERR);
    }

    subsep(co, TO_ERR);
}

/// Run-wide quick summary: AI prose + severity counts.
///
/// Renders on stdout (not stderr) so the same payload reaches a downstream agent piped from
/// `boro review` - a one-line TL;DR before the detailed Findings section is more useful than
/// burying it next to the stats block. Skipped silently when `out["summary"]` is absent (older
/// runs, non-review subcommands, dry-run, Ctrl-C).
fn print_quick_summary_block(out: &Value) {
    let Some(summary) = out.get("summary") else {
        return;
    };
    let text = summary
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let counts = summary.get("counts");
    let get_count = |k: &str| -> u64 {
        counts
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    let critical = get_count("Critical");
    let high = get_count("High");
    let medium = get_count("Medium");
    let low = get_count("Low");

    let co = use_color();
    section_title("Quick Summary", co, false);

    if !text.is_empty() {
        let lines = wrap_words(text, 76, 76);
        for line in &lines {
            println!("  {}", paint!(line, co, |s| s.bright_white()));
        }
        println!();
    }

    // Color each severity counter with the same palette used by individual findings, so a
    // glance at the line shows whether anything Critical/High showed up.
    let crit_cell = paint!(&format!("Critical:{critical}"), co, |s| s
        .bold()
        .bright_red());
    let high_cell = paint!(&format!("High:{high}"), co, |s| s.bold().red());
    let med_cell = paint!(&format!("Medium:{medium}"), co, |s| s.yellow());
    let low_cell = paint!(&format!("Low:{low}"), co, |s| s.green());
    println!(
        "  {}  {}  {}  {}  {}",
        paint!("[Issues]", co, |s| s.bold().bright_cyan()),
        crit_cell,
        high_cell,
        med_cell,
        low_cell,
    );
    println!();
    subsep(co, false);
}

fn print_findings_block(commits: &[&Value], use_validated: bool) {
    let has_reportable_commits = commits
        .iter()
        .any(|c| c.get("dry_run").and_then(|v| v.as_bool()) != Some(true));
    if !has_reportable_commits {
        return;
    }
    let all_plan_only = commits
        .iter()
        .filter(|c| c.get("dry_run").and_then(|v| v.as_bool()) != Some(true))
        .all(|c| c.get("plan").and_then(|v| v.as_bool()) == Some(true));

    let co = use_color();
    let title = if all_plan_only {
        "Test Plan"
    } else if use_validated {
        "Findings (validated)"
    } else {
        "Findings"
    };
    section_title(title, co, false);

    for commit in commits {
        if commit.get("dry_run").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if let Some(err) = commit.get("error").and_then(|v| v.as_str()) {
            let sha = short_sha(commit);
            println!(
                "  {}  {}{}",
                paint!(&sha, co, |s| s.bold().yellow()),
                paint!("(review failed)", co, |s| s.bold().red()),
                commit_subject(commit)
                    .map(|s| format!("  {s}"))
                    .unwrap_or_default()
            );
            for line in err.lines() {
                println!("    {}", paint!(line, co, |s| s.red()));
            }
            println!();
            continue;
        }
        let sha = short_sha(commit);
        let findings = commit_effective_findings(commit, use_validated);
        let n = findings.map(|a| a.len()).unwrap_or(0);
        let plan_only = commit.get("plan").and_then(|v| v.as_bool()) == Some(true);
        let status = if plan_only {
            "(plan only)".to_string()
        } else {
            format!("({n} finding{})", if n == 1 { "" } else { "s" })
        };
        if let Some(subject) = commit_subject(commit) {
            println!(
                "  {}  {}  {}",
                paint!(&sha, co, |s| s.bold().yellow()),
                paint!(&status, co, |s| s.dimmed()),
                paint!(&subject, co, |s| s.bright_white())
            );
        } else {
            println!(
                "  {}  {}",
                paint!(&sha, co, |s| s.bold().yellow()),
                paint!(&status, co, |s| s.dimmed())
            );
        }

        print_test_summary(commit, co);

        if let Some(arr) = findings {
            if arr.is_empty() {
                if !plan_only {
                    println!("    {}", paint!("(none)", co, |s| s.dimmed()));
                }
            } else {
                for (i, f) in arr.iter().enumerate() {
                    print_finding(i + 1, f, co);
                }
            }
        } else {
            println!(
                "    {}",
                paint!("(missing findings array)", co, |s| s.red())
            );
        }
        println!();
    }
    subsep(co, false);
}

/// Pick which findings array to render for this commit. In `validation_mode
/// == "findings"` we prefer `validated_findings` (the filter step's output);
/// when that's absent (e.g. the validator dropped the commit or the call
/// failed) we fall back to the raw `findings` so the user still sees
/// something.
fn commit_effective_findings(commit: &Value, use_validated: bool) -> Option<&Vec<Value>> {
    if use_validated {
        if let Some(arr) = commit.get("validated_findings").and_then(|f| f.as_array()) {
            return Some(arr);
        }
    }
    commit.get("findings").and_then(|f| f.as_array())
}

/// Print the `boro test`-only block: which command was run inside virtme-ng and the model's
/// summary of what it observed. Skipped silently when both fields are absent (so `boro review`
/// commits, which never populate them, are unaffected) or when the test was skipped because the
/// build failed (no useful test info to report — the build-failure finding speaks for itself).
fn print_test_summary(commit: &Value, color: bool) {
    if commit
        .get("boot_status")
        .and_then(|v| v.as_str())
        .map(|s| s == "skipped")
        .unwrap_or(false)
    {
        return;
    }
    let cmd = commit.get("test_command").and_then(|v| v.as_str());
    let summary = commit.get("test_summary").and_then(|v| v.as_str());
    let plan_only = commit.get("plan").and_then(|v| v.as_bool()) == Some(true);
    if cmd.is_none() && summary.is_none() && !plan_only {
        return;
    }
    if let Some(c) = cmd {
        println!(
            "    {} {}",
            paint!("test:", color, |s| s.dimmed()),
            paint!(c, color, |s| s.cyan()),
        );
    }
    if plan_only {
        print_test_plan_details(commit, color);
        println!(
            "    {}",
            paint!("(plan only; test not run)", color, |s| s.dimmed())
        );
        return;
    }
    if let Some(s) = summary {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            // 76-col first/continuation width, indented by 4 spaces — same body width budget as
            // findings text above.
            let lines = wrap_words(trimmed, 76, 76);
            for line in &lines {
                println!("    {}", paint!(line, color, |s| s));
            }
        }
    }
}

fn print_test_plan_details(commit: &Value, color: bool) {
    let description = test_plan_string(commit, "description", "test_description");
    if let Some(text) = description.as_deref() {
        print_wrapped_label("plan", text, color);
    }
    if let Some(script) = test_plan_string(commit, "script", "test_script") {
        print_plan_script(&script, color);
    }

    print_plan_list(
        "requirements",
        &test_plan_array(commit, "requirements"),
        false,
        color,
    );
    print_plan_list("steps", &test_plan_array(commit, "steps"), true, color);
    print_plan_list(
        "expected",
        &test_plan_array(commit, "expected_results"),
        false,
        color,
    );

    if let Some(rationale) = test_plan_string(commit, "rationale", "test_rationale") {
        let is_duplicate = description
            .as_deref()
            .map(|d| d.trim() == rationale.trim())
            .unwrap_or(false);
        if !is_duplicate {
            print_wrapped_label("why", &rationale, color);
        }
    }
}

fn test_plan_string(commit: &Value, key: &str, fallback_key: &str) -> Option<String> {
    commit
        .get("test_plan")
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_str())
        .or_else(|| commit.get(fallback_key).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn test_plan_array(commit: &Value, key: &str) -> Vec<String> {
    commit
        .get("test_plan")
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn print_wrapped_label(label: &str, text: &str, color: bool) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let lines = wrap_words(trimmed, 70, 76);
    for (idx, line) in lines.iter().enumerate() {
        if idx == 0 {
            println!(
                "    {} {}",
                paint!(&format!("{label}:"), color, |s| s.dimmed()),
                paint!(line, color, |s| s)
            );
        } else {
            println!("    {}", paint!(line, color, |s| s));
        }
    }
}

fn print_plan_list(label: &str, items: &[String], numbered: bool, color: bool) {
    if items.is_empty() {
        return;
    }
    println!(
        "    {}",
        paint!(&format!("{label}:"), color, |s| s.dimmed())
    );
    for (idx, item) in items.iter().enumerate() {
        let marker = if numbered {
            format!("{}.", idx + 1)
        } else {
            "-".to_string()
        };
        let first_width = 76usize.saturating_sub(marker.chars().count() + 1);
        let lines = wrap_words(item.trim(), first_width, first_width);
        for (line_idx, line) in lines.iter().enumerate() {
            if line_idx == 0 {
                println!("      {} {}", marker, paint!(line, color, |s| s));
            } else {
                println!(
                    "      {} {}",
                    " ".repeat(marker.chars().count()),
                    paint!(line, color, |s| s)
                );
            }
        }
    }
}

fn print_plan_script(script: &str, color: bool) {
    if script.trim().is_empty() {
        return;
    }
    println!("    {}", paint!("script:", color, |s| s.dimmed()));
    for line in script.lines() {
        println!("      {}", paint!(line, color, |s| s));
    }
}

fn print_finding(idx: usize, f: &Value, color: bool) {
    let problem = f
        .get("problem")
        .and_then(|v| v.as_str())
        .unwrap_or("(no problem text)");
    let sev = f.get("severity").and_then(|v| v.as_str()).unwrap_or("?");
    let tag = format!("[{sev}]");
    let tag_str = if color {
        match sev {
            "Critical" => tag.bold().bright_red().to_string(),
            "High" => tag.bold().red().to_string(),
            "Medium" => tag.yellow().to_string(),
            "Low" => tag.green().to_string(),
            _ => tag.dimmed().to_string(),
        }
    } else {
        tag.clone()
    };
    let idx_prefix_len = format!("{idx}.").chars().count();
    let prefix_len = "    ".chars().count()
        + idx_prefix_len
        + "  ".chars().count()
        + tag.chars().count()
        + " ".chars().count();
    let first_budget = FINDING_LINE_MAX.saturating_sub(prefix_len).max(1);
    let cont_budget = FINDING_LINE_MAX
        .saturating_sub(FINDING_CONT_INDENT.chars().count())
        .max(1);
    let problem_lines = wrap_finding_body(problem, first_budget, cont_budget);

    println!();
    if problem_lines.is_empty() {
        println!(
            "    {}  {} {}",
            paint!(&format!("{idx}."), color, |s| s.bold().dimmed()),
            tag_str,
            paint!(problem, color, |s| s.bright_white())
        );
    } else {
        for (li, pl) in problem_lines.iter().enumerate() {
            if li == 0 {
                println!(
                    "    {}  {} {}",
                    paint!(&format!("{idx}."), color, |s| s.bold().dimmed()),
                    tag_str,
                    paint!(pl, color, |s| s.bright_white())
                );
            } else {
                println!(
                    "{}{}",
                    FINDING_CONT_INDENT,
                    paint!(pl, color, |s| s.bright_white())
                );
            }
        }
    }
    if let Some(arr) = f.get("voters").and_then(|v| v.as_array()) {
        let names: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        if !names.is_empty() {
            let suffix = format!("(voters: {})", names.join(", "));
            println!(
                "         {}",
                paint!(&suffix, color, |s| s.dimmed().italic())
            );
        }
    }
    if let Some(exp) = f.get("severity_explanation").and_then(|v| v.as_str()) {
        if !exp.is_empty() {
            let expl_budget = FINDING_LINE_MAX
                .saturating_sub(FINDING_CONT_INDENT.chars().count())
                .max(1);
            for line in exp.lines() {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                let wrapped = wrap_words(trimmed, expl_budget, expl_budget);
                if wrapped.is_empty() {
                    continue;
                }
                for wl in wrapped {
                    println!(
                        "{}{}",
                        FINDING_CONT_INDENT,
                        paint!(&wl, color, |s| s.dimmed())
                    );
                }
            }
        }
    }
}

/// Non-empty paragraphs from `text` (split on newline), each word-wrapped.
fn wrap_finding_body(text: &str, first_budget: usize, cont_budget: usize) -> Vec<String> {
    let paras: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if paras.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, para) in paras.iter().enumerate() {
        let fb = if i == 0 { first_budget } else { cont_budget };
        out.extend(wrap_words(para.trim(), fb, cont_budget));
    }
    out
}

/// Word-wrap at spaces; lines are at most `first_width` / `cont_width` Unicode characters.
fn wrap_words(text: &str, first_width: usize, cont_width: usize) -> Vec<String> {
    let first_width = first_width.max(1);
    let cont_width = cont_width.max(1);
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut max_w = first_width;

    let mut i = 0usize;
    while i < words.len() {
        let w = words[i];
        let wl = w.chars().count();

        if wl > max_w && cur.is_empty() {
            for chunk in split_char_chunks(w, max_w) {
                lines.push(chunk);
            }
            max_w = cont_width;
            i += 1;
            continue;
        }

        let sep = if cur.is_empty() { 0 } else { 1 };
        let would_be = cur.chars().count() + sep + wl;

        if would_be > max_w && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            max_w = cont_width;
            continue;
        }

        if sep == 1 {
            cur.push(' ');
        }
        cur.push_str(w);
        i += 1;
    }

    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn split_char_chunks(s: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut n = 0usize;
    for ch in s.chars() {
        if n >= max_chars && !cur.is_empty() {
            out.push(cur);
            cur = String::new();
            n = 0;
        }
        cur.push(ch);
        n += 1;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// LKML bodies last so the narrative reply is easy to read / copy after metrics.
fn print_lkml_block(commits: &[&Value]) {
    let any = commits.iter().any(|c| {
        c.get("lkml_report")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    });
    if !any {
        return;
    }

    let co = use_color();
    section_title("LKML-style report", co, false);
    for commit in commits {
        let Some(text) = commit.get("lkml_report").and_then(|v| v.as_str()) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let sha = short_sha(commit);
        let subject = commit_subject(commit);
        let header = match subject.as_deref() {
            Some(s) => format!("Commit {sha} - {s}"),
            None => format!("Commit {sha}"),
        };
        println!("{}", paint!(&header, co, |s| s.bold().bright_magenta()));
        let bar = "·".repeat(WIDTH.min(60));
        println!("{}", paint!(&bar, co, |s| s.dimmed()));
        println!();
        for line in text.lines() {
            // LKML reply bodies are intended to be copy/pasted into emails as-is.
            // Avoid indentation and ANSI coloring in the body.
            println!("{line}");
        }
        println!();
    }
}

fn section_title(title: &str, color: bool, to_stderr: bool) {
    pln!(to_stderr);
    pln!(
        to_stderr,
        "{}",
        paint!(&format!(" {title} "), color, |s| s.bold().bright_cyan())
    );
    pln!(
        to_stderr,
        "{}",
        paint!(&"-".repeat(WIDTH), color, |s| s.dimmed())
    );
}

fn subsep(color: bool, to_stderr: bool) {
    pln!(
        to_stderr,
        "{}",
        paint!(&"·".repeat(WIDTH), color, |s| s.dimmed())
    );
    pln!(to_stderr);
}

fn short_sha(c: &Value) -> String {
    let full = c.get("sha").and_then(|v| v.as_str()).unwrap_or("?");
    if full.len() > 12 {
        full[..12].to_string()
    } else {
        full.to_string()
    }
}

fn commit_subject(c: &Value) -> Option<String> {
    c.get("subject")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn commit_title(c: &Value) -> String {
    let sha = short_sha(c);
    match commit_subject(c) {
        Some(subject) => format!("{sha}  {subject}"),
        None => sha,
    }
}

fn print_usage_card(u: &Value, indent: &str, color: bool, to_stderr: bool) {
    let pt = u.get("prompt_tokens").and_then(json_u64);
    let ct = u.get("completion_tokens").and_then(json_u64);
    let cw = u
        .get("cache_creation_tokens")
        .and_then(json_u64)
        .unwrap_or(0);
    let cr = u.get("cache_read_tokens").and_then(json_u64).unwrap_or(0);
    let calls = u.get("api_calls").and_then(|v| v.as_u64()).unwrap_or(0);
    let ps = pt
        .map(api::fmt_tokens_short)
        .unwrap_or_else(|| "—".to_string());
    let cs = ct
        .map(api::fmt_tokens_short)
        .unwrap_or_else(|| "—".to_string());
    if cw > 0 || cr > 0 {
        let prompt_total = pt.unwrap_or(0);
        let pct = |part: u64| -> String {
            if prompt_total == 0 {
                return String::new();
            }
            format!(" ({} % of prompt)", part * 100 / prompt_total)
        };
        let cr_s = format!("{}{}", api::fmt_tokens_short(cr), pct(cr));
        let cw_s = format!("{}{}", api::fmt_tokens_short(cw), pct(cw));
        pln!(
            to_stderr,
            "{}{} {}  {} {}  {} {}  {} {}  {} {}",
            indent,
            paint!("prompt:", color, |s| s.bold().dimmed()),
            paint!(&ps, color, |s| s.bright_green()),
            paint!("cache_r:", color, |s| s.bold().dimmed()),
            paint!(&cr_s, color, |s| s.bright_cyan()),
            paint!("cache_w:", color, |s| s.bold().dimmed()),
            paint!(&cw_s, color, |s| s.bright_cyan()),
            paint!("tokens:", color, |s| s.bold().dimmed()),
            paint!(&cs, color, |s| s.bright_green()),
            paint!("api_calls:", color, |s| s.bold().dimmed()),
            paint!(&calls.to_string(), color, |s| s.bright_white())
        );
    } else {
        pln!(
            to_stderr,
            "{}{} {}  {} {}  {} {}",
            indent,
            paint!("prompt:", color, |s| s.bold().dimmed()),
            paint!(&ps, color, |s| s.bright_green()),
            paint!("tokens:", color, |s| s.bold().dimmed()),
            paint!(&cs, color, |s| s.bright_green()),
            paint!("api_calls:", color, |s| s.bold().dimmed()),
            paint!(&calls.to_string(), color, |s| s.bright_white())
        );
    }
}

fn truncate_step_name(name: &str, max: usize) -> String {
    let count = name.chars().count();
    if count <= max {
        return name.to_string();
    }
    let take = max.saturating_sub(1);
    let end = name
        .char_indices()
        .nth(take)
        .map(|(i, _)| i)
        .unwrap_or(name.len());
    format!("{}...", &name[..end])
}

/// One row of the per-step usage table. Kept as a struct to keep the
/// per-step accumulator readable when the `cache_w` / `cache_r` columns
/// are present.
struct UsageRow {
    name: String,
    prompt: String,
    tokens: String,
    cache_w: String,
    cache_r: String,
    wall: String,
    error: Option<String>,
}

/// Token columns use [`api::fmt_tokens_short`] (k / M / G); `wall_ms` when present.
fn print_step_table(steps: &[Value], indent: &str, color: bool, to_stderr: bool) {
    let mut rows: Vec<UsageRow> = Vec::new();
    let mut any_cache = false;
    for step in steps {
        let name = step.get("step").and_then(|v| v.as_str()).unwrap_or("?");
        let err = step
            .get("error")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let name_disp = truncate_step_name(name, 22);
        let pt = step.get("prompt_tokens").and_then(json_u64);
        let ct = step.get("completion_tokens").and_then(json_u64);
        let cw = step.get("cache_creation_tokens").and_then(json_u64);
        let cr = step.get("cache_read_tokens").and_then(json_u64);
        let wall_ms = step.get("wall_ms").and_then(json_u64);
        if cw.unwrap_or(0) > 0 || cr.unwrap_or(0) > 0 {
            any_cache = true;
        }
        let ps = pt
            .map(api::fmt_tokens_short)
            .unwrap_or_else(|| "—".to_string());
        let cs = ct
            .map(api::fmt_tokens_short)
            .unwrap_or_else(|| "—".to_string());
        let cws = cw.map(api::fmt_tokens_short).unwrap_or_default();
        let crs = cr.map(api::fmt_tokens_short).unwrap_or_default();
        let tw = wall_ms.map(fmt_wall_ms).unwrap_or_else(|| "—".to_string());
        rows.push(UsageRow {
            name: name_disp,
            prompt: ps,
            tokens: cs,
            cache_w: cws,
            cache_r: crs,
            wall: tw,
            error: err,
        });
    }

    let wn = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(4)
        .max("step".chars().count())
        .clamp(8, 24);
    let wp = rows
        .iter()
        .map(|r| r.prompt.chars().count())
        .max()
        .unwrap_or(1)
        .max("prompt".len());
    let wc = rows
        .iter()
        .map(|r| r.tokens.chars().count())
        .max()
        .unwrap_or(1)
        .max("tokens".len());
    let wcw = if any_cache {
        rows.iter()
            .map(|r| r.cache_w.chars().count())
            .max()
            .unwrap_or(1)
            .max("cache_w".len())
    } else {
        0
    };
    let wcr = if any_cache {
        rows.iter()
            .map(|r| r.cache_r.chars().count())
            .max()
            .unwrap_or(1)
            .max("cache_r".len())
    } else {
        0
    };
    let ww = rows
        .iter()
        .map(|r| r.wall.chars().count())
        .max()
        .unwrap_or(1)
        .max("time".len());

    let hdr_step = format!("{:<w$}", "step", w = wn);
    let hdr_prompt = format!("{:>w$}", "prompt", w = wp);
    let hdr_token = format!("{:>w$}", "tokens", w = wc);
    let hdr_time = format!("{:>w$}", "time", w = ww);
    if any_cache {
        let hdr_cw = format!("{:>w$}", "cache_w", w = wcw);
        let hdr_cr = format!("{:>w$}", "cache_r", w = wcr);
        pln!(
            to_stderr,
            "{}{}{}{}{}{}{}{}{}{}{}{}",
            indent,
            paint!(&hdr_step, color, |s| s.dimmed()),
            paint!("  ", color, |s| s.dimmed()),
            paint!(&hdr_prompt, color, |s| s.dimmed()),
            paint!("  ", color, |s| s.dimmed()),
            paint!(&hdr_cr, color, |s| s.dimmed()),
            paint!("  ", color, |s| s.dimmed()),
            paint!(&hdr_cw, color, |s| s.dimmed()),
            paint!("  ", color, |s| s.dimmed()),
            paint!(&hdr_token, color, |s| s.dimmed()),
            paint!("  ", color, |s| s.dimmed()),
            paint!(&hdr_time, color, |s| s.dimmed()),
        );
    } else {
        pln!(
            to_stderr,
            "{}{}{}{}{}{}{}{}",
            indent,
            paint!(&hdr_step, color, |s| s.dimmed()),
            paint!("  ", color, |s| s.dimmed()),
            paint!(&hdr_prompt, color, |s| s.dimmed()),
            paint!("  ", color, |s| s.dimmed()),
            paint!(&hdr_token, color, |s| s.dimmed()),
            paint!("  ", color, |s| s.dimmed()),
            paint!(&hdr_time, color, |s| s.dimmed()),
        );
    }

    for row in rows {
        let name_cell = format!("{:<w$}", row.name, w = wn);
        let p_cell = format!("{:>w$}", row.prompt, w = wp);
        let c_cell = format!("{:>w$}", row.tokens, w = wc);
        let w_cell = format!("{:>w$}", row.wall, w = ww);
        if any_cache {
            let cw_cell = format!("{:>w$}", row.cache_w, w = wcw);
            let cr_cell = format!("{:>w$}", row.cache_r, w = wcr);
            pln!(
                to_stderr,
                "{}{}{}{}{}{}{}{}{}{}{}{}",
                indent,
                paint!(&name_cell, color, |s| s.dimmed()),
                paint!("  ", color, |s| s.dimmed()),
                paint!(&p_cell, color, |s| s.bright_green()),
                paint!("  ", color, |s| s.dimmed()),
                paint!(&cr_cell, color, |s| s.bright_cyan()),
                paint!("  ", color, |s| s.dimmed()),
                paint!(&cw_cell, color, |s| s.bright_cyan()),
                paint!("  ", color, |s| s.dimmed()),
                paint!(&c_cell, color, |s| s.bright_green()),
                paint!("  ", color, |s| s.dimmed()),
                paint!(&w_cell, color, |s| s.bright_cyan()),
            );
        } else {
            pln!(
                to_stderr,
                "{}{}{}{}{}{}{}{}",
                indent,
                paint!(&name_cell, color, |s| s.dimmed()),
                paint!("  ", color, |s| s.dimmed()),
                paint!(&p_cell, color, |s| s.bright_green()),
                paint!("  ", color, |s| s.dimmed()),
                paint!(&c_cell, color, |s| s.bright_green()),
                paint!("  ", color, |s| s.dimmed()),
                paint!(&w_cell, color, |s| s.bright_cyan()),
            );
        }
        if let Some(e) = row.error {
            let cont = format!("{indent}  ");
            let budget = FINDING_LINE_MAX
                .saturating_sub(cont.chars().count() + "error: ".len())
                .max(20);
            let lines = wrap_words(&e, budget, budget);
            for (li, line) in lines.iter().enumerate() {
                if li == 0 {
                    pln!(
                        to_stderr,
                        "{}{}{}",
                        cont,
                        paint!("error: ", color, |s| s.bold().red()),
                        paint!(line, color, |s| s.red())
                    );
                } else {
                    pln!(
                        to_stderr,
                        "{}{}{}",
                        cont,
                        "       ",
                        paint!(line, color, |s| s.red())
                    );
                }
            }
        }
    }
}

fn fmt_wall_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 600_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let s = ms / 1000;
        let m = s / 60;
        let rs = s % 60;
        format!("{m}m {rs}s")
    }
}

fn json_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_i64().map(|i| i as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_words_short_unsplit() {
        let l = wrap_words("hello world", 80, 80);
        assert_eq!(l, vec!["hello world"]);
    }

    #[test]
    fn wrap_words_two_lines() {
        let l = wrap_words("aa bb cc dd ee", 8, 8);
        assert_eq!(l, vec!["aa bb cc", "dd ee"]);
    }

    #[test]
    fn wrap_words_long_word_split() {
        let l = wrap_words("abcdefghij", 4, 4);
        assert_eq!(l, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_finding_body_second_paragraph_uses_cont_width() {
        let t = "first words here\n\nmore after blank";
        let w = wrap_finding_body(t, 12, 12);
        assert!(w.len() >= 2);
    }

    #[test]
    fn write_report_json_round_trips_top_level_and_finding_keys() {
        // Synthetic `out` shape: top-level schema_version + range + commits with a finding
        // that has a location and one without one. The pretty JSON should preserve both.
        let out = serde_json::json!({
            "schema_version": 1,
            "range": "HEAD~1..HEAD",
            "subcommand": "review",
            "model": "test-model",
            "commits": [{
                "sha": "deadbeef",
                "subject": "test commit",
                "patch": "diff --git a/x b/x\n",
                "changed_paths": ["x"],
                "findings": [
                    {
                        "problem": "located issue",
                        "severity": "Medium",
                        "severity_explanation": "why",
                        "location": {"file": "x", "line": 1, "side": "RIGHT"}
                    },
                    {
                        "problem": "unanchored issue",
                        "severity": "Low",
                        "severity_explanation": "why"
                    }
                ]
            }]
        });
        let mut buf = Vec::new();
        write_report_json(&mut buf, &out).expect("write");
        let text = String::from_utf8(buf).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["commits"][0]["sha"], "deadbeef");
        assert_eq!(parsed["commits"][0]["findings"][0]["location"]["file"], "x");
        assert!(parsed["commits"][0]["findings"][1]
            .get("location")
            .is_none());
    }

    #[test]
    fn write_report_json_round_trips_quick_summary_highlights() {
        // The additive highlights field must not disturb the legacy summary text or counts.
        let out = serde_json::json!({
            "schema_version": 1,
            "range": "HEAD~1..HEAD",
            "commits": [],
            "summary": {
                "text": "Most serious issue: UAF in driver foo.",
                "counts": {"Critical": 1, "High": 0, "Medium": 2, "Low": 0},
                "highlights": [{
                    "finding_ref": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef:0",
                    "commit": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                    "severity": "Critical",
                    "title": "UAF in driver foo",
                    "question": "Can this object be released before the callback runs?",
                    "location": {"file": "drivers/foo.c", "line": 42, "side": "RIGHT"}
                }],
            },
        });
        let mut buf = Vec::new();
        write_report_json(&mut buf, &out).expect("write");
        let text = String::from_utf8(buf).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["summary"]["counts"]["Critical"], 1);
        assert_eq!(parsed["summary"]["counts"]["Medium"], 2);
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(
            parsed["summary"]["text"],
            "Most serious issue: UAF in driver foo."
        );
        assert_eq!(parsed["summary"]["highlights"][0]["severity"], "Critical");
        assert_eq!(
            parsed["summary"]["highlights"][0]["location"]["file"],
            "drivers/foo.c"
        );
    }
}
