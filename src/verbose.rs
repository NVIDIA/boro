// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Where detailed diagnostics go: stderr (when `--verbose`) or nowhere.
///
/// Users who want a log file just redirect `2>file.log` from the shell.
#[derive(Clone)]
pub struct VerboseDest {
    /// Print detailed lines to stderr (with optional colors when stderr is a TTY).
    pub stderr: bool,
    /// Prepended to every verbose line (e.g. `[boro]` or `[PATCH 2/10] deadbeef]`).
    line_prefix: String,
}

impl VerboseDest {
    pub fn new(stderr: bool) -> Self {
        Self::with_line_prefix(stderr, "[boro]")
    }

    pub fn with_line_prefix(stderr: bool, line_prefix: impl Into<String>) -> Self {
        Self {
            stderr,
            line_prefix: line_prefix.into(),
        }
    }

    /// Same stderr sink with a different per-line prefix (e.g. per-commit tag).
    pub fn with_prefix(&self, line_prefix: impl Into<String>) -> Self {
        Self {
            stderr: self.stderr,
            line_prefix: line_prefix.into(),
        }
    }

    #[inline]
    pub(crate) fn line_prefix(&self) -> &str {
        self.line_prefix.as_str()
    }

    #[inline]
    pub fn active(&self) -> bool {
        self.stderr
    }

    /// Plain prefixed line to stderr (when `--verbose`).
    pub fn line(&self, msg: impl std::fmt::Display) {
        if !self.stderr {
            return;
        }
        eprintln!("{} {}", self.line_prefix, msg);
    }
}
