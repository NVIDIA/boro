// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Runtime display preferences.
#[derive(Clone)]
pub struct VerboseDest {
    /// Legacy diagnostic logging sink. Kept as a field because some callers use
    /// it to decide whether to print fallback dry-run summaries, but CLI
    /// `--verbose` no longer enables these logs.
    pub stderr: bool,
    /// Stream model response text to stderr as it arrives.
    stream_model_responses: bool,
    /// Prepended to legacy verbose lines when diagnostics are explicitly enabled.
    line_prefix: String,
}

impl VerboseDest {
    pub fn new(stream_model_responses: bool) -> Self {
        Self {
            stderr: false,
            stream_model_responses,
            line_prefix: "[boro]".to_string(),
        }
    }

    /// Same stderr sink with a different per-line prefix (e.g. per-commit tag).
    pub fn with_prefix(&self, line_prefix: impl Into<String>) -> Self {
        Self {
            stderr: self.stderr,
            stream_model_responses: self.stream_model_responses,
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

    #[inline]
    pub fn stream_model_responses(&self) -> bool {
        self.stream_model_responses
    }

    /// Plain prefixed diagnostic line to stderr.
    pub fn line(&self, msg: impl std::fmt::Display) {
        if !self.stderr {
            return;
        }
        eprintln!("{} {}", self.line_prefix, msg);
    }
}
