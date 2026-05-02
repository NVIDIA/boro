// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Local clustering primitives for deduplicating LLM-emitted text findings without an extra
//! model call. Used to pre-cluster specialist concerns before consolidation so the consolidator
//! sees one row per distinct concern instead of N near-duplicates from 5 stages.

use serde_json::Value;

/// Cluster a list of specialist concerns by `description`, dropping near-duplicates.
///
/// First-seen concern of each cluster is kept (preserves stage order); subsequent
/// near-duplicates are discarded. "Near-duplicate" means either the first 10 normalized
/// tokens match exactly or the trigram Jaccard similarity exceeds 0.6.
///
/// Concerns with an empty/missing `description` are passed through unchanged so that
/// the consolidator still sees them; only entries with usable descriptions participate
/// in clustering.
pub fn cluster_concerns(concerns: &[Value]) -> Vec<Value> {
    if concerns.len() <= 1 {
        return concerns.to_vec();
    }

    // Signature for each concern: (trigrams, first-10 tokens). Empty description → keep,
    // skip clustering.
    let mut sigs: Vec<Option<(Vec<String>, Vec<String>)>> = Vec::with_capacity(concerns.len());
    for c in concerns {
        let desc = c
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        if desc.is_empty() {
            sigs.push(None);
        } else {
            sigs.push(Some((trigrams(desc), first_n_tokens(desc, 10))));
        }
    }

    let mut kept: Vec<Value> = Vec::with_capacity(concerns.len());
    let mut kept_sigs: Vec<(Vec<String>, Vec<String>)> = Vec::with_capacity(concerns.len());

    for (i, sig) in sigs.iter().enumerate() {
        match sig {
            None => kept.push(concerns[i].clone()),
            Some(s) => {
                if kept_sigs.iter().any(|k| similar(k, s)) {
                    continue;
                }
                kept.push(concerns[i].clone());
                kept_sigs.push(s.clone());
            }
        }
    }
    kept
}

fn similar(a: &(Vec<String>, Vec<String>), b: &(Vec<String>, Vec<String>)) -> bool {
    if !a.1.is_empty() && a.1 == b.1 {
        return true;
    }
    jaccard(&a.0, &b.0) > 0.6
}

fn trigrams(s: &str) -> Vec<String> {
    let norm = normalize(s);
    let chars: Vec<char> = norm.chars().collect();
    if chars.len() < 3 {
        return vec![norm];
    }
    let mut out: Vec<String> = Vec::with_capacity(chars.len().saturating_sub(2));
    for i in 0..chars.len().saturating_sub(2) {
        out.push(chars[i..i + 3].iter().collect());
    }
    out.sort();
    out.dedup();
    out
}

fn first_n_tokens(s: &str, n: usize) -> Vec<String> {
    normalize(s)
        .split_whitespace()
        .take(n)
        .map(|t| t.to_string())
        .collect()
}

fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for low in ch.to_lowercase() {
                out.push(low);
            }
        } else if !out.ends_with(' ') {
            out.push(' ');
        }
    }
    out.trim().to_string()
}

fn jaccard(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    use std::collections::HashSet;
    let sa: HashSet<&String> = a.iter().collect();
    let sb: HashSet<&String> = b.iter().collect();
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn concern(desc: &str) -> Value {
        json!({"type": "s3:demo", "description": desc, "reasoning": "r"})
    }

    #[test]
    fn cluster_concerns_dedups_similar() {
        let a = concern("task can race with shutdown leading to use-after-free in foo()");
        let b =
            concern("Task can race with shutdown leading to use-after-free in foo() under load.");
        let out = cluster_concerns(&[a.clone(), b]);
        assert_eq!(out.len(), 1, "near-duplicates should collapse to one row");
        assert_eq!(out[0]["description"], a["description"]);
    }

    #[test]
    fn cluster_concerns_keeps_distinct() {
        let a = concern("missing rcu_read_lock in cpu hotplug callback");
        let b = concern("kmalloc with GFP_KERNEL inside spinlock in irq path");
        let out = cluster_concerns(&[a, b]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn cluster_concerns_empty_input_passthrough() {
        assert_eq!(cluster_concerns(&[]).len(), 0);
    }

    #[test]
    fn cluster_concerns_single_passthrough() {
        let only = concern("just one");
        let out = cluster_concerns(std::slice::from_ref(&only));
        assert_eq!(out, vec![only]);
    }

    #[test]
    fn cluster_concerns_preserves_first_seen() {
        let a = concern("memory leak on error path in init");
        let b = concern("Memory leak on error path in init() function");
        let out = cluster_concerns(&[a.clone(), b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], a, "first-seen entry should be kept verbatim");
    }

    #[test]
    fn cluster_concerns_blank_description_passthrough() {
        let blank = json!({"type": "x", "description": "", "reasoning": "r"});
        let real = concern("missing rcu_read_lock");
        let out = cluster_concerns(&[blank.clone(), real.clone()]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], blank);
        assert_eq!(out[1], real);
    }

    #[test]
    fn jaccard_basic() {
        let a = vec!["abc".to_string(), "bcd".to_string(), "cde".to_string()];
        let b = vec!["abc".to_string(), "bcd".to_string(), "cde".to_string()];
        assert!((jaccard(&a, &b) - 1.0).abs() < 0.001);

        let c = vec!["xyz".to_string()];
        assert_eq!(jaccard(&a, &c), 0.0);
    }
}
