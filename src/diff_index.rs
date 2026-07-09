// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Parsed-hunk lookup for unified diffs.
//!
//! Built once per commit from the patch text. Exposes `contains(file, line, side)`
//! so the findings sanitizer can verify a model-supplied `location.{file,line,side}`
//! actually lands on a line that appears in the diff (added, removed, or context).
//! Anchors that don't land on a real hunk line get dropped - the finding survives
//! but the misleading inline placement does not.
//!
//! Context lines belong to both sides: every context line is indexed once under
//! `RIGHT` (using the new file's line number) and once under `LEFT` (using the old
//! file's line number), matching the viewer's anchoring semantics.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn from_str(s: &str) -> Self {
        match s {
            "LEFT" => Side::Left,
            _ => Side::Right,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct DiffIndex {
    keys: HashSet<(String, u64, Side)>,
    text: HashMap<(String, u64, Side), String>,
}

impl DiffIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a unified diff (output of `git show --no-color` or `git diff`) and index every
    /// hunk line. Robust against:
    /// - extended header lines (`diff --git`, `index ...`, `similarity index ...`, etc.)
    /// - separate `--- old/path` / `+++ new/path` names for renames and deleted files
    /// - header-like payload lines (`--- body`, `+++ body`) by consuming the exact `@@` ranges
    /// - new files or deleted files where only one side has a live path
    /// - multiple files in one diff, including traditional unified diffs without `diff --git`
    pub fn from_unified_diff(diff: &str) -> Self {
        let mut idx = Self::new();
        let mut old_path: Option<String> = None;
        let mut new_path: Option<String> = None;
        let mut iter = diff.lines().peekable();

        while let Some(line) = iter.next() {
            if line.starts_with("diff --git ") {
                old_path = None;
                new_path = None;
                continue;
            }
            if let Some(rest) = line.strip_prefix("--- ") {
                old_path = parse_diff_path(rest);
                continue;
            }
            if let Some(rest) = line.strip_prefix("+++ ") {
                new_path = parse_diff_path(rest);
                continue;
            }
            let Some((mut old_line, mut old_remaining, mut new_line, mut new_remaining)) =
                parse_hunk_header(line)
            else {
                continue;
            };

            while let Some(peek) = iter.peek() {
                let Some(kind) = classify_hunk_body_line(peek, old_remaining, new_remaining) else {
                    break;
                };
                let body = iter.next().unwrap();
                match kind {
                    HunkBodyLine::Addition => {
                        if let Some(path) = new_path.as_deref() {
                            idx.insert(path, new_line, Side::Right, &body[1..]);
                        }
                        new_line += 1;
                        new_remaining -= 1;
                    }
                    HunkBodyLine::Deletion => {
                        if let Some(path) = old_path.as_deref() {
                            idx.insert(path, old_line, Side::Left, &body[1..]);
                        }
                        old_line += 1;
                        old_remaining -= 1;
                    }
                    HunkBodyLine::Context => {
                        let text = body.strip_prefix(' ').unwrap_or(body);
                        if let Some(path) = new_path.as_deref() {
                            idx.insert(path, new_line, Side::Right, text);
                        }
                        if let Some(path) = old_path.as_deref() {
                            idx.insert(path, old_line, Side::Left, text);
                        }
                        old_line += 1;
                        new_line += 1;
                        old_remaining -= 1;
                        new_remaining -= 1;
                    }
                    HunkBodyLine::NoNewline => {
                        // "\ No newline at end of file" belongs to the current hunk but does
                        // not consume an old/new line number.
                    }
                }
            }
        }
        idx
    }

    pub fn contains(&self, file: &str, line: u64, side: Side) -> bool {
        self.keys.contains(&(file.to_string(), line, side))
    }

    fn insert(&mut self, file: &str, line: u64, side: Side, text: &str) {
        let key = (file.to_string(), line, side);
        self.keys.insert(key.clone());
        self.text.insert(key, text.to_string());
    }

    /// True when at least one named identifier occurs in the anchored range. This catches
    /// locations that are syntactically valid hunk coordinates but point at unrelated code.
    pub fn range_contains_identifier(
        &self,
        file: &str,
        start: u64,
        end: u64,
        side: Side,
        identifiers: &[String],
    ) -> bool {
        (start..=end).any(|line| {
            self.text
                .get(&(file.to_string(), line, side))
                .is_some_and(|text| identifiers.iter().any(|id| contains_c_identifier(text, id)))
        })
    }
}

fn contains_c_identifier(text: &str, ident: &str) -> bool {
    text.match_indices(ident).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + ident.len()..].chars().next();
        before.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
            && after.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkBodyLine {
    Addition,
    Deletion,
    Context,
    NoNewline,
}

fn classify_hunk_body_line(
    line: &str,
    old_remaining: u64,
    new_remaining: u64,
) -> Option<HunkBodyLine> {
    match line.as_bytes().first().copied() {
        Some(b'+') if new_remaining > 0 => Some(HunkBodyLine::Addition),
        Some(b'-') if old_remaining > 0 => Some(HunkBodyLine::Deletion),
        Some(b' ') if old_remaining > 0 && new_remaining > 0 => Some(HunkBodyLine::Context),
        None if old_remaining > 0 && new_remaining > 0 => Some(HunkBodyLine::Context),
        Some(b'\\') => Some(HunkBodyLine::NoNewline),
        _ => None,
    }
}

/// Parse `@@ -A[,B] +C[,D] @@ ...` into `(A, B, C, D)`. B and D default to 1 when omitted.
fn parse_hunk_header(line: &str) -> Option<(u64, u64, u64, u64)> {
    let rest = line.strip_prefix("@@ ")?;
    let mut parts = rest.splitn(3, ' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    parts.next()?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    Some((old_start, old_len, new_start, new_len))
}

fn parse_range(s: &str) -> Option<(u64, u64)> {
    let mut it = s.splitn(2, ',');
    let start = it.next()?.parse().ok()?;
    let len = match it.next() {
        Some(n) => n.parse().ok()?,
        None => 1,
    };
    Some((start, len))
}

/// Extract the live path from a `---` or `+++` header. Returns `None` for `/dev/null`.
fn parse_diff_path(rest: &str) -> Option<String> {
    let raw = rest.trim().split('\t').next()?.trim();
    if raw == "/dev/null" {
        return None;
    }
    let cleaned = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_DIFF: &str = "\
diff --git a/foo.c b/foo.c
index 1111111..2222222 100644
--- a/foo.c
+++ b/foo.c
@@ -10,5 +10,7 @@
 ctx1
 ctx2
-removed_line
+added_one
+added_two
 ctx3
 ctx4
";

    const RENAME_DIFF: &str = "\
diff --git a/app/main.rs b/app/main_renamed.rs
similarity index 89%
rename from app/main.rs
rename to app/main_renamed.rs
--- a/app/main.rs
+++ b/app/main_renamed.rs
@@ -10,4 +10,4 @@ main flow
 trace!(\"before\");
-println!(\"old behavior\");
+println!(\"new behavior\");
 trace!(\"after\");
";

    const DELETE_DIFF: &str = "\
diff --git a/app/removed.rs b/app/removed.rs
deleted file mode 100644
index 8f3..000
--- a/app/removed.rs
+++ /dev/null
@@ -5,2 +0,0 @@
-console!(\"remove me\");
-cleanup();
";

    const HEADER_LIKE_BODY_DIFF: &str = "\
diff --git a/scripts/example.txt b/scripts/example.txt
index 123..456 100644
--- a/scripts/example.txt
+++ b/scripts/example.txt
@@ -1,3 +1,4 @@
 alpha
--- looks like a header but is body
+++ still body, not a header
+tail line that must stay in the hunk
 omega
@@ -10,1 +11,1 @@
-old second line
+new second line
";

    const MULTI_FILE_UNIFIED_DIFF: &str = "\
--- a/foo.c
+++ b/foo.c
@@ -1,1 +1,1 @@
-old
+new
--- a/bar.c
+++ b/bar.c
@@ -10,1 +10,1 @@
-old_bar
+new_bar
";

    #[test]
    fn indexes_added_lines_on_right() {
        let idx = DiffIndex::from_unified_diff(SIMPLE_DIFF);
        // ctx1 at old=10, new=10; ctx2 at 11/11; removed at old=12; added_one at new=12,
        // added_two at new=13; ctx3 at old=13, new=14; ctx4 at old=14, new=15.
        assert!(idx.contains("foo.c", 12, Side::Right));
        assert!(idx.contains("foo.c", 13, Side::Right));
    }

    #[test]
    fn indexes_removed_lines_on_left() {
        let idx = DiffIndex::from_unified_diff(SIMPLE_DIFF);
        assert!(idx.contains("foo.c", 12, Side::Left));
    }

    #[test]
    fn context_indexed_on_both_sides() {
        let idx = DiffIndex::from_unified_diff(SIMPLE_DIFF);
        // ctx1: old=10, new=10
        assert!(idx.contains("foo.c", 10, Side::Left));
        assert!(idx.contains("foo.c", 10, Side::Right));
        // ctx3: old=13, new=14
        assert!(idx.contains("foo.c", 13, Side::Left));
        assert!(idx.contains("foo.c", 14, Side::Right));
    }

    #[test]
    fn rejects_lines_outside_hunk() {
        let idx = DiffIndex::from_unified_diff(SIMPLE_DIFF);
        // Line 9 is before the hunk starts; line 20 is after.
        assert!(!idx.contains("foo.c", 9, Side::Right));
        assert!(!idx.contains("foo.c", 20, Side::Right));
        // Wrong file.
        assert!(!idx.contains("bar.c", 10, Side::Right));
    }

    #[test]
    fn multiple_files_in_one_diff() {
        let diff = "\
diff --git a/foo.c b/foo.c
--- a/foo.c
+++ b/foo.c
@@ -1,2 +1,2 @@
 ctx
-old
+new
diff --git a/bar.c b/bar.c
--- a/bar.c
+++ b/bar.c
@@ -100,1 +100,2 @@
 ctx
+inserted
";
        let idx = DiffIndex::from_unified_diff(diff);
        assert!(idx.contains("foo.c", 2, Side::Right));
        assert!(idx.contains("foo.c", 2, Side::Left));
        assert!(idx.contains("bar.c", 101, Side::Right));
        // bar.c line 2 must not bleed in from foo.c's counters.
        assert!(!idx.contains("bar.c", 2, Side::Right));
    }

    #[test]
    fn new_file_ignores_dev_null_left() {
        let diff = "\
diff --git a/new.c b/new.c
new file mode 100644
--- /dev/null
+++ b/new.c
@@ -0,0 +1,2 @@
+line1
+line2
";
        let idx = DiffIndex::from_unified_diff(diff);
        assert!(idx.contains("new.c", 1, Side::Right));
        assert!(idx.contains("new.c", 2, Side::Right));
    }

    #[test]
    fn rename_indexes_left_and_right_paths_separately() {
        let idx = DiffIndex::from_unified_diff(RENAME_DIFF);
        assert!(idx.contains("app/main.rs", 11, Side::Left));
        assert!(idx.contains("app/main_renamed.rs", 11, Side::Right));
    }

    #[test]
    fn deleted_file_indexes_left_side_under_old_path() {
        let idx = DiffIndex::from_unified_diff(DELETE_DIFF);
        assert!(idx.contains("app/removed.rs", 5, Side::Left));
        assert!(!idx.contains("app/removed.rs", 5, Side::Right));
    }

    #[test]
    fn header_like_hunk_body_lines_do_not_break_later_hunks() {
        let idx = DiffIndex::from_unified_diff(HEADER_LIKE_BODY_DIFF);
        assert!(idx.contains("scripts/example.txt", 11, Side::Right));
        assert!(idx.contains("scripts/example.txt", 10, Side::Left));
    }

    #[test]
    fn declared_hunk_counts_keep_traditional_multifile_diffs_separate() {
        let idx = DiffIndex::from_unified_diff(MULTI_FILE_UNIFIED_DIFF);
        assert!(idx.contains("foo.c", 1, Side::Left));
        assert!(idx.contains("bar.c", 10, Side::Right));
        assert!(!idx.contains("bar.c", 1, Side::Right));
    }

    #[test]
    fn handles_blank_context_lines() {
        // git diff emits a bare empty line for an all-blank context line (no leading space).
        let diff = "\
diff --git a/foo.c b/foo.c
--- a/foo.c
+++ b/foo.c
@@ -1,3 +1,4 @@
 first

+inserted
 last
";
        let idx = DiffIndex::from_unified_diff(diff);
        // Blank context line: old=2, new=2; inserted: new=3; last: old=3, new=4.
        assert!(idx.contains("foo.c", 2, Side::Right));
        assert!(idx.contains("foo.c", 2, Side::Left));
        assert!(idx.contains("foo.c", 3, Side::Right));
        assert!(idx.contains("foo.c", 4, Side::Right));
        assert!(idx.contains("foo.c", 3, Side::Left));
    }

    #[test]
    fn empty_diff_indexes_nothing() {
        let idx = DiffIndex::from_unified_diff("");
        assert!(!idx.contains("anything", 1, Side::Right));
    }

    #[test]
    fn side_from_str_defaults_to_right() {
        assert_eq!(Side::from_str("LEFT"), Side::Left);
        assert_eq!(Side::from_str("RIGHT"), Side::Right);
        assert_eq!(Side::from_str(""), Side::Right);
        assert_eq!(Side::from_str("weird"), Side::Right);
    }

    #[test]
    fn semantic_range_requires_named_identifier() {
        let idx = DiffIndex::from_unified_diff(SIMPLE_DIFF);
        assert!(idx.range_contains_identifier(
            "foo.c",
            12,
            13,
            Side::Right,
            &["added_two".to_string()]
        ));
        assert!(!idx.range_contains_identifier(
            "foo.c",
            10,
            11,
            Side::Right,
            &["added_two".to_string()]
        ));
    }
}
