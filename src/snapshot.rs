// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Per-commit progress snapshot shared between the review pipeline and the Ctrl-C dumper.
//!
//! Each commit task owns a [`SnapshotPublisher`] that mirrors stage results into a
//! [`CommitSnapshot`] guarded by an `Arc<Mutex<...>>`. `main` keeps the read end so a
//! Ctrl-C handler can render whatever has been published so far instead of losing the
//! whole run.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::api::StageUsage;
use crate::git::CommitMeta;

#[derive(Debug)]
pub struct CommitSnapshot {
    pub sha: String,
    /// Best-effort `{"findings": [...]}`. Starts empty, then holds fallback findings
    /// derived from merged concerns, then the consolidated findings, etc.
    pub findings: Value,
    pub phase0_selected_prompts: Option<Vec<String>>,
    pub usage_steps: Vec<StageUsage>,
    pub completed: bool,
    pub error: Option<String>,
    /// Commit metadata + diff, captured once near the top of `commit_review_inner`.
    /// `None` until `set_metadata` lands; partial runs (Ctrl-C before metadata
    /// fetch) emit JSON without these fields.
    pub metadata: Option<CommitMeta>,
    pub patch_diff: Option<String>,
    pub changed_paths: Option<Vec<String>>,
}

#[derive(Clone)]
pub struct SnapshotPublisher {
    inner: Arc<Mutex<CommitSnapshot>>,
}

impl SnapshotPublisher {
    pub fn new(sha: &str) -> (Arc<Mutex<CommitSnapshot>>, Self) {
        let snap = CommitSnapshot {
            sha: sha.to_string(),
            findings: json!({ "findings": [] }),
            phase0_selected_prompts: None,
            usage_steps: Vec::new(),
            completed: false,
            error: None,
            metadata: None,
            patch_diff: None,
            changed_paths: None,
        };
        let arc = Arc::new(Mutex::new(snap));
        (Arc::clone(&arc), Self { inner: arc })
    }

    pub fn add_stage(&self, stage: StageUsage) {
        self.inner.lock().unwrap().usage_steps.push(stage);
    }

    pub fn set_phase0(&self, guides: Option<Vec<String>>) {
        self.inner.lock().unwrap().phase0_selected_prompts = guides;
    }

    pub fn set_findings(&self, findings: Value) {
        self.inner.lock().unwrap().findings = findings;
    }

    pub fn set_metadata(&self, meta: CommitMeta, patch_diff: String, changed_paths: Vec<String>) {
        let mut s = self.inner.lock().unwrap();
        s.metadata = Some(meta);
        s.patch_diff = Some(patch_diff);
        s.changed_paths = Some(changed_paths);
    }

    pub fn mark_complete(&self) {
        self.inner.lock().unwrap().completed = true;
    }

    pub fn set_error(&self, e: String) {
        self.inner.lock().unwrap().error = Some(e);
    }
}

/// Render a snapshot to the same JSON shape `commit_review_inner` produces, with an
/// extra `"partial": true` marker when the commit didn't run to completion.
pub fn snapshot_to_value(s: &CommitSnapshot) -> Value {
    let findings_arr = s
        .findings
        .get("findings")
        .cloned()
        .unwrap_or_else(|| json!([]));

    let mut prompt: u64 = 0;
    let mut completion: u64 = 0;
    for st in &s.usage_steps {
        if let Some(p) = st.usage.prompt {
            prompt += u64::from(p);
        }
        if let Some(c) = st.usage.completion {
            completion += u64::from(c);
        }
    }
    let usage = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "api_calls": s.usage_steps.len(),
    });
    let usage_steps: Vec<Value> = s
        .usage_steps
        .iter()
        .map(|st| {
            json!({
                "step": st.step,
                "prompt_tokens": st.usage.prompt,
                "completion_tokens": st.usage.completion,
                "wall_ms": st.wall.as_millis() as u64,
                "error": st.error,
            })
        })
        .collect();

    let mut obj = json!({
        "sha": s.sha,
        "findings": findings_arr,
        "usage": usage,
        "usage_steps": Value::Array(usage_steps),
    });
    if let Some(meta) = &s.metadata {
        obj["subject"] = json!(meta.subject);
        obj["author"] = json!(meta.author);
        obj["date"] = json!(meta.date);
        obj["parents"] = json!(meta.parents);
    }
    if let Some(p) = &s.patch_diff {
        obj["patch"] = json!(p);
    }
    if let Some(cp) = &s.changed_paths {
        obj["changed_paths"] = json!(cp);
    }
    if !s.completed {
        obj["partial"] = json!(true);
    }
    if let Some(p0) = &s.phase0_selected_prompts {
        if !p0.is_empty() {
            obj["phase0_selected_prompts"] = json!(p0);
        }
    }
    if let Some(e) = &s.error {
        obj["error"] = json!(e);
    }
    obj
}

/// Sum (api_calls, prompt_tokens, completion_tokens, cache_creation, cache_read) across the
/// published stages.
pub fn snapshot_run_totals(s: &CommitSnapshot) -> (u32, u64, u64, u64, u64) {
    let mut p: u64 = 0;
    let mut c: u64 = 0;
    let mut cw: u64 = 0;
    let mut cr: u64 = 0;
    for st in &s.usage_steps {
        if let Some(pp) = st.usage.prompt {
            p += u64::from(pp);
        }
        if let Some(cc) = st.usage.completion {
            c += u64::from(cc);
        }
        if let Some(ccw) = st.usage.cache_creation {
            cw += u64::from(ccw);
        }
        if let Some(ccr) = st.usage.cache_read {
            cr += u64::from(ccr);
        }
    }
    (s.usage_steps.len() as u32, p, c, cw, cr)
}
