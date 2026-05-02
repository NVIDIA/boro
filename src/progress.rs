// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! TTY-only spinner on stderr (stdout stays clean for JSON).
//!
//! While a request runs, one animated line shows the current stage (`{spinner} {msg}`). On
//! drop the spinner line is cleared only; the caller prints the final progress line (with token
//! summary) on stderr so stdout stays JSON-only.
//!
//! [`MultiPatchSpinner`] uses [`MultiProgress`]: one stderr row per commit (including a single-commit
//! run), so stages update in place. With multiple concurrent workers, each patch keeps its own line;
//! prompt and completion counts are shown only on the footer bar (updated from every HTTP completion
//! across workers).
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::{stderr, IsTerminal};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use owo_colors::OwoColorize;

/// Drop clears the spinner line (no print).
pub struct SpinnerGuard(Option<ProgressBar>);

impl SpinnerGuard {
    /// Shows a spinner on stderr if it is a TTY; no-op when piped (CI, scripts).
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        if !stderr().is_terminal() {
            return Self(None);
        }
        let pb = ProgressBar::new_spinner();
        pb.set_draw_target(ProgressDrawTarget::stderr());
        pb.set_style(spinner_line_style());
        pb.set_message(message);
        pb.enable_steady_tick(Duration::from_millis(80));
        Self(Some(pb))
    }

    /// Refresh the spinner line (e.g. prompt/token counts after each HTTP round-trip in a tool loop).
    pub fn set_message(&self, message: impl Into<String>) {
        if let Some(ref pb) = self.0 {
            pb.set_message(message.into());
        }
    }

    /// Print one line cleanly *above* the spinner (indicatif moves the spinner out
    /// of the way, prints the line, then redraws the spinner below). Falls back to
    /// a plain `eprintln!` when no spinner is active (piped / non-TTY).
    pub fn println(&self, message: impl AsRef<str>) {
        let m = message.as_ref();
        match &self.0 {
            Some(pb) => pb.println(m),
            None => eprintln!("{m}"),
        }
    }
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        if let Some(pb) = self.0.take() {
            pb.finish_and_clear();
        }
    }
}

fn spinner_line_style() -> ProgressStyle {
    // `{elapsed}` ticks live (every 80 ms) so a row that's stuck waiting on an HTTP round still
    // visibly progresses. Worker rows reset this between stages via `WorkerLineCtx::reset_stage`.
    ProgressStyle::with_template("{spinner:.cyan} {msg} {elapsed:.dim}")
        .unwrap()
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

/// Footer style: no spinner glyph — the footer is a totals line, not active work, and a leading
/// spinner there reads as "still computing the totals" which is misleading. `{elapsed}` ticks live
/// so it doubles as the run-wide wall clock.
fn footer_line_style() -> ProgressStyle {
    ProgressStyle::with_template("{msg} {elapsed:.dim}").unwrap()
}

/// Bracketed, colored phase tag for the worker spinner row — e.g. `[thinking]`, `[tool: ...]`.
///
/// Color picked from the prefix so callers don't need to thread an enum through:
/// - `thinking` → cyan
/// - `responding` → green
/// - `starting` / `initializing...` → dim (pre-work / setup phases)
/// - anything starting with `tool` → yellow
///
/// indicatif short-circuits its draw when stderr isn't a terminal (see `SpinnerGuard` /
/// `MultiPatchSpinner` gating), so ANSI codes here are only emitted to a real TTY.
pub fn phase_tag(phase: &str) -> String {
    let painted = if phase == "thinking" {
        phase.cyan().bold().to_string()
    } else if phase == "responding" {
        phase.green().bold().to_string()
    } else if phase == "starting" || phase.starts_with("initializing") {
        phase.bright_black().bold().to_string()
    } else if phase.starts_with("tool") {
        phase.yellow().bold().to_string()
    } else {
        phase.to_string()
    };
    format!("[{painted}]")
}

fn footer_usage_draw_message(
    prompt: u64,
    completion: u64,
    cache_creation: u64,
    cache_read: u64,
) -> String {
    let mut s = format!("prompt:{}", footer_fmt_tokens(prompt));
    if cache_read > 0 || cache_creation > 0 {
        s.push_str(&format!(
            "  cache_r:{}  cache_w:{}",
            footer_fmt_tokens(cache_read),
            footer_fmt_tokens(cache_creation),
        ));
    }
    s.push_str(&format!("  tokens:{}", footer_fmt_tokens(completion)));
    s
}

fn footer_usage_plain_line(
    prompt: u64,
    completion: u64,
    cache_creation: u64,
    cache_read: u64,
) -> String {
    footer_usage_draw_message(prompt, completion, cache_creation, cache_read)
}

/// Format the persistent usage footer line used by review/build/test/apply progress UIs.
///
/// Callers print this themselves after clearing the live UI (via
/// [`MultiPatchSpinner::finish_footer_clear`]) when they need the totals line to land at a
/// specific position in the output (e.g. after the human / JSON report).
pub fn usage_footer_line(
    prompt: u64,
    completion: u64,
    cache_creation: u64,
    cache_read: u64,
) -> String {
    footer_usage_plain_line(prompt, completion, cache_creation, cache_read)
}

/// Shared footer + one progress line for a concurrent patch worker (see [`MultiPatchSpinner`]).
#[derive(Clone)]
pub struct WorkerLineCtx {
    bar: ProgressBar,
    footer: ProgressBar,
    // (prompt, completion, cache_creation, cache_read) — single writer across workers.
    prompt_completion_sums: Arc<Mutex<(u64, u64, u64, u64)>>,
}

impl WorkerLineCtx {
    pub fn set_line_message(&self, msg: impl Into<String>) {
        self.bar.set_message(msg.into());
    }

    /// Restart the row's `{elapsed}` ticker — call at the start of each stage so the timer reflects
    /// just that stage, not the whole commit.
    pub fn reset_stage_elapsed(&self) {
        self.bar.reset_elapsed();
    }

    /// Add response usage into the run-wide prompt/token/cache sums and refresh the footer line
    /// (single writer across workers).
    pub fn record_tokens(
        &self,
        prompt: Option<u32>,
        completion: Option<u32>,
        cache_creation: Option<u32>,
        cache_read: Option<u32>,
    ) {
        let mut g = self.prompt_completion_sums.lock().unwrap();
        if let Some(p) = prompt {
            g.0 += u64::from(p);
        }
        if let Some(c) = completion {
            g.1 += u64::from(c);
        }
        if let Some(cw) = cache_creation {
            g.2 += u64::from(cw);
        }
        if let Some(cr) = cache_read {
            g.3 += u64::from(cr);
        }
        self.footer
            .set_message(footer_usage_draw_message(g.0, g.1, g.2, g.3));
    }

    pub fn finish_commit_line(&self) {
        self.bar.finish_and_clear();
    }

    /// Print one line cleanly *above* the multi-row spinner (indicatif moves the
    /// rows out of the way, prints the line, then redraws them below). Use this
    /// for one-time runtime notices that would otherwise corrupt the live UI.
    pub fn println(&self, message: impl AsRef<str>) {
        self.bar.println(message);
    }
}

/// Multi-line stderr UI: one spinner row per commit + prompt/token footer (no tokens on patch rows).
pub struct MultiPatchSpinner {
    multi: MultiProgress,
    worker_bars: Vec<ProgressBar>,
    footer_bar: ProgressBar,
    prompt_completion_sums: Arc<Mutex<(u64, u64, u64, u64)>>,
}

impl MultiPatchSpinner {
    /// Build UI when stderr is a TTY. Worker bars are ordered like the revision list; footer is last.
    pub fn try_new(num_commits: usize) -> Option<Self> {
        if !stderr().is_terminal() || num_commits == 0 {
            return None;
        }
        let multi = MultiProgress::new();
        multi.set_draw_target(ProgressDrawTarget::stderr());

        let style = spinner_line_style();
        let mut worker_bars = Vec::with_capacity(num_commits);
        for _ in 0..num_commits {
            let pb = multi.add(ProgressBar::new_spinner());
            pb.set_style(style.clone());
            pb.set_message("...");
            pb.enable_steady_tick(Duration::from_millis(80));
            worker_bars.push(pb);
        }

        let footer_bar = multi.add(ProgressBar::new_spinner());
        footer_bar.set_style(footer_line_style());
        // Tick to keep `{elapsed}` updating; no spinner glyph thanks to `footer_line_style`.
        footer_bar.enable_steady_tick(Duration::from_millis(80));
        footer_bar.set_message(footer_usage_draw_message(0, 0, 0, 0));

        Some(Self {
            multi,
            worker_bars,
            footer_bar,
            prompt_completion_sums: Arc::new(Mutex::new((0, 0, 0, 0))),
        })
    }

    pub fn worker_ctx(&self, idx: usize) -> WorkerLineCtx {
        WorkerLineCtx {
            bar: self.worker_bars[idx].clone(),
            footer: self.footer_bar.clone(),
            prompt_completion_sums: Arc::clone(&self.prompt_completion_sums),
        }
    }

    /// Add a temporary stage row above the footer. Used by post-commit phases
    /// so the shared prompt/token footer stays live after commit workers finish.
    pub fn stage_ctx(&self, message: impl Into<String>) -> WorkerLineCtx {
        let pb = ProgressBar::new_spinner();
        pb.set_style(spinner_line_style());
        pb.set_message(message.into());
        pb.enable_steady_tick(Duration::from_millis(80));
        let pb = self.multi.insert_before(&self.footer_bar, pb);
        WorkerLineCtx {
            bar: pb,
            footer: self.footer_bar.clone(),
            prompt_completion_sums: Arc::clone(&self.prompt_completion_sums),
        }
    }

    /// Clear the live footer bar and print the same prompt/token line as a normal stderr line (so it remains visible).
    pub fn finish_footer_eprintln(&self) {
        let line = {
            let g = self.prompt_completion_sums.lock().unwrap();
            footer_usage_plain_line(g.0, g.1, g.2, g.3)
        };
        self.footer_bar.finish_and_clear();
        if stderr().is_terminal() {
            eprintln!();
            eprintln!("{line}");
        }
    }

    /// Clear the live footer without printing a final line. Callers that need the totals line
    /// at a specific position (e.g. after the human/JSON report) render [`usage_footer_line`]
    /// from their own totals after the live UI is gone.
    pub fn finish_footer_clear(&self) {
        self.footer_bar.finish_and_clear();
    }
}

fn footer_fmt_tokens(n: u64) -> String {
    const K: f64 = 1000.0;
    if n < 1000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        return fmt_scaled_unit(n as f64 / K, "k");
    }
    if n < 1_000_000_000 {
        return fmt_scaled_unit(n as f64 / (K * K), "M");
    }
    fmt_scaled_unit(n as f64 / (K * K * K), "G")
}

fn fmt_scaled_unit(value: f64, suffix: &str) -> String {
    let t = (value * 10.0).round() / 10.0;
    if (t - t.floor()).abs() < 0.001 {
        format!("{}{}", t as u64, suffix)
    } else {
        format!("{:.1}{}", t, suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        // Tiny stripper: drop ESC[...m sequences. Good enough for assertions; we don't need a full
        // ANSI parser here. Tests run with whatever color setting cargo test happens to give us.
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for sk in chars.by_ref() {
                    if sk == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn phase_tag_brackets_thinking() {
        assert_eq!(strip_ansi(&phase_tag("thinking")), "[thinking]");
    }

    #[test]
    fn phase_tag_brackets_tool() {
        assert_eq!(
            strip_ansi(&phase_tag("tool: read_files(foo.c)")),
            "[tool: read_files(foo.c)]"
        );
    }

    #[test]
    fn phase_tag_brackets_responding() {
        assert_eq!(strip_ansi(&phase_tag("responding")), "[responding]");
    }

    #[test]
    fn phase_tag_unknown_falls_through() {
        assert_eq!(strip_ansi(&phase_tag("custom")), "[custom]");
    }

    #[test]
    fn usage_footer_line_matches_review_footer_shape() {
        assert_eq!(usage_footer_line(1234, 56, 0, 0), "prompt:1.2k  tokens:56");
        assert_eq!(
            usage_footer_line(1234, 56, 78, 90),
            "prompt:1.2k  cache_r:90  cache_w:78  tokens:56"
        );
    }
}
