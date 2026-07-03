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
    /// - `--- a/<path>` / `+++ b/<path>` path lines (the `b/` path wins, since findings
    ///   reference the post-image file)
    /// - new files (`+++ /dev/null` is ignored; only the live side gets indexed)
    /// - multiple files in one diff
    /// - lines that do not match `+`, `-`, ` ` (treated as out-of-hunk and ignored)
    pub fn from_unified_diff(diff: &str) -> Self {
        let mut idx = Self::new();
        let mut path: Option<String> = None;
        let mut old_line: u64 = 0;
        let mut new_line: u64 = 0;
        let mut in_hunk = false;

        for line in diff.lines() {
            if let Some(rest) = line.strip_prefix("+++ ") {
                // +++ b/path  OR  +++ /dev/null
                path = parse_diff_path(rest);
                in_hunk = false;
                continue;
            }
            if line.starts_with("--- ") {
                // Ignore - +++ wins. Reset hunk state.
                in_hunk = false;
                continue;
            }
            if line.starts_with("diff --git ") {
                // New file boundary. Path will be set by the upcoming `+++` line.
                path = None;
                in_hunk = false;
                continue;
            }
            if let Some(rest) = line.strip_prefix("@@ ") {
                if let Some((o, n)) = parse_hunk_header(rest) {
                    old_line = o;
                    new_line = n;
                    in_hunk = true;
                } else {
                    in_hunk = false;
                }
                continue;
            }
            if !in_hunk {
                continue;
            }
            let Some(path_str) = path.as_deref() else {
                continue;
            };
            let first = line.chars().next();
            match first {
                Some('+') => {
                    idx.insert(path_str, new_line, Side::Right, &line[1..]);
                    new_line += 1;
                }
                Some('-') => {
                    idx.insert(path_str, old_line, Side::Left, &line[1..]);
                    old_line += 1;
                }
                Some(' ') | None => {
                    // Context (a leading space) or a bare empty line, which `git diff` emits for
                    // an entirely blank context line. Index under both sides.
                    let text = line.strip_prefix(' ').unwrap_or(line);
                    idx.insert(path_str, new_line, Side::Right, text);
                    idx.insert(path_str, old_line, Side::Left, text);
                    old_line += 1;
                    new_line += 1;
                }
                Some('\\') => {
                    // "\ No newline at end of file" - no counter update.
                }
                _ => {
                    // Anything else terminates the current hunk; wait for the next `@@`.
                    in_hunk = false;
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

/// Parse a unified-diff hunk header tail after `@@ `: `-OLD[,OCNT] +NEW[,NCNT] @@ context`.
/// Returns `(old_start, new_start)` or `None` if the header is malformed.
fn parse_hunk_header(rest: &str) -> Option<(u64, u64)> {
    // Format: "-O[,C] +N[,C] @@ ..."
    let mut parts = rest.split_whitespace();
    let old_tok = parts.next()?;
    let new_tok = parts.next()?;
    let old_num = old_tok.strip_prefix('-')?.split(',').next()?;
    let new_num = new_tok.strip_prefix('+')?.split(',').next()?;
    Some((old_num.parse().ok()?, new_num.parse().ok()?))
}

/// Extract the post-image path from a `+++ b/<path>` line. Returns `None` for `/dev/null`
/// (file deleted) so we don't index against a sentinel name. Tolerates the absence of the
/// `b/` prefix (some diff producers omit it).
fn parse_diff_path(rest: &str) -> Option<String> {
    let raw = rest.trim().split('\t').next()?.trim();
    if raw == "/dev/null" {
        return None;
    }
    let cleaned = raw.strip_prefix("b/").unwrap_or(raw);
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
    fn deleted_file_dev_null_right_drops_path() {
        let diff = "\
diff --git a/gone.c b/gone.c
deleted file mode 100644
--- a/gone.c
+++ /dev/null
@@ -1,2 +0,0 @@
-line1
-line2
";
        let idx = DiffIndex::from_unified_diff(diff);
        // We don't index the deletion side (the post-image path is /dev/null).
        // The viewer's b-side path defaulting means findings on a deleted file are
        // commit-level anyway, so this is fine.
        assert!(!idx.contains("gone.c", 1, Side::Left));
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
