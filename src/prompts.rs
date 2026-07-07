// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};

use crate::config::ReviewTarget;
use crate::target;

/// Fetch an embedded prompt file (relative path) from the tree for `target`.
fn embed_get(target: ReviewTarget, rel: &str) -> Option<rust_embed::EmbeddedFile> {
    target::prompt_file(target, rel)
}

/// Pick subsystem/*.md files from changed paths (best-effort). Always includes locking via CORE_FILES.
pub fn pick_subsystem_files(target: ReviewTarget, changed: &[String]) -> Vec<String> {
    use std::collections::HashSet;
    let mut out = HashSet::new();
    for p in changed {
        let pl = p.replace('\\', "/");
        for (prefix, md) in target::subsystem_map(target) {
            if pl.contains(prefix) {
                out.insert(format!("subsystem/{md}"));
            }
        }
    }
    out.into_iter().collect()
}

fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut t = s.chars().take(max).collect::<String>();
    t.push_str("\n\n[... truncated by boro for size ...]\n");
    t
}

fn read_prompt_rel(target: ReviewTarget, rel: &str, max: usize) -> Result<Option<String>> {
    let Some(file) = embed_get(target, rel) else {
        return Ok(None);
    };
    let raw = std::str::from_utf8(file.data.as_ref())
        .with_context(|| format!("embedded prompt {rel} is not valid UTF-8"))?;
    Ok(Some(truncate_utf8(raw, max)))
}

fn prompt_exists(target: ReviewTarget, rel: &str) -> bool {
    embed_get(target, rel).is_some()
}

/// Normalize Phase 0 basename or path to `subsystem/foo.md`.
fn normalize_phase0_guide(name: &str) -> String {
    let n = name.trim().replace('\\', "/");
    if n.starts_with("subsystem/") {
        n
    } else {
        format!("subsystem/{n}")
    }
}

pub fn build_reference_context(
    target: ReviewTarget,
    changed_paths: &[String],
    max_total: usize,
    phase0_selected: Option<&[String]>,
    followup_summary: Option<&str>,
) -> Result<String> {
    use std::collections::HashSet;
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut loaded_subsystem: HashSet<String> = HashSet::new();

    let single_pass = include_str!("../resources/fast-review.md");
    parts.push(format!("# boro instructions\n{single_pass}\n"));
    used += parts.last().map(|s| s.len()).unwrap_or(0);

    let local_reference = target::local_reference(target);
    if !local_reference.is_empty() {
        let block = format!("\n\n# boro target instructions\n{local_reference}\n");
        used += block.len();
        parts.push(block);
    }

    for rel in target::core_files(target) {
        let chunk = read_prompt_rel(target, rel, max_total / 4).context("core prompt read")?;
        if let Some(text) = chunk {
            let header = format!("\n\n# --- {} ---\n\n", rel);
            let add = header.len() + text.len();
            if used + add > max_total {
                parts.push(format!(
                    "\n\n# --- {} ---\n[skipped: context budget]\n",
                    rel
                ));
                used = max_total;
                break;
            }
            used += add;
            parts.push(header);
            parts.push(text);
        }
    }

    // Phase 0 narrowing: when Phase 0 returned a non-empty pick list, treat it as the
    // authoritative subsystem-guide selection and skip the broad path-matched fallback.
    // Path-matched picks only fire when Phase 0 was disabled, failed, or returned nothing.
    let phase0_has_picks = phase0_selected.map(|s| !s.is_empty()).unwrap_or(false);
    if !phase0_has_picks {
        let mut sub = pick_subsystem_files(target, changed_paths);
        sub.sort();
        for rel in sub {
            loaded_subsystem.insert(rel.clone());
            let chunk = read_prompt_rel(target, &rel, max_total / 3).context("subsystem read")?;
            if let Some(text) = chunk {
                let header = format!("\n\n# --- {} ---\n\n", rel);
                let add = header.len() + text.len();
                if used + add > max_total {
                    parts.push(format!(
                        "\n\n# --- {} ---\n[skipped: context budget]\n",
                        rel
                    ));
                    break;
                }
                used += add;
                parts.push(header);
                parts.push(text);
            }
        }
    }

    if let Some(extra) = phase0_selected {
        let mut extras: Vec<String> = extra
            .iter()
            .map(|s| normalize_phase0_guide(s))
            // Locking is already in CORE_FILES; exclude duplicate from Phase 0 picks.
            .filter(|rel| !rel.ends_with("locking.md"))
            .collect();
        extras.sort();
        extras.dedup();
        for rel in extras {
            if loaded_subsystem.contains(&rel) {
                continue;
            }
            if !prompt_exists(target, &rel) {
                continue;
            }
            loaded_subsystem.insert(rel.clone());
            let chunk =
                read_prompt_rel(target, &rel, max_total / 3).context("phase0 subsystem read")?;
            if let Some(text) = chunk {
                let header = format!("\n\n# --- {} (phase 0) ---\n\n", rel);
                let add = header.len() + text.len();
                if used + add > max_total {
                    parts.push(format!(
                        "\n\n# --- {} ---\n[skipped: context budget]\n",
                        rel
                    ));
                    break;
                }
                used += add;
                parts.push(header);
                parts.push(text);
            }
        }
    }

    if let Some(summary) = followup_summary {
        let trimmed = summary.trim();
        if !trimmed.is_empty() {
            let block = format!("\n\n# --- upstream follow-up ---\n\n{trimmed}\n");
            if used + block.len() <= max_total {
                parts.push(block);
            } else {
                parts.push(
                    "\n\n# --- upstream follow-up ---\n[skipped: context budget]\n".to_string(),
                );
            }
        }
    }

    Ok(parts.concat())
}

/// Files appended only for specialist stages (3 and 5 have extras).
pub fn load_stage_prompt_files(target: ReviewTarget, stage: u8, max_each: usize) -> Result<String> {
    let rels: &[&str] = match stage {
        3 => &["callstack.md", "technical-patterns.md"],
        5 => &["subsystem/locking.md"],
        _ => &[],
    };
    let mut s = String::new();
    for rel in rels {
        if let Some(t) = read_prompt_rel(target, rel, max_each)? {
            s.push_str(&format!("\n\n# --- {rel} ---\n\n{t}"));
        }
    }
    Ok(s)
}

pub fn load_consolidation_extras(target: ReviewTarget, max_each: usize) -> Result<String> {
    let mut s = String::new();
    for rel in ["false-positive-guide.md", "severity.md"] {
        if let Some(t) = read_prompt_rel(target, rel, max_each)? {
            s.push_str(&format!("\n\n# --- {rel} ---\n\n{t}"));
        }
    }
    Ok(s)
}

/// Short distilled false-positive guide for specialist stages. Embedded at build time
/// (not read from `third_party/`) so it is reviewable and deterministic; the consolidator
/// continues to receive the full upstream guide via [`load_consolidation_extras`].
pub fn load_false_positive_digest() -> String {
    include_str!("../resources/false-positive-digest.md").to_string()
}

/// `subsystem/subsystem.md` index for Phase 0 (capped).
pub fn load_subsystem_index(target: ReviewTarget, max_chars: usize) -> Result<Option<String>> {
    read_prompt_rel(target, "subsystem/subsystem.md", max_chars)
}

/// `inline-template.md` for the mailing-list-style report.
pub fn load_inline_template(target: ReviewTarget, max_chars: usize) -> Result<Option<String>> {
    read_prompt_rel(target, "inline-template.md", max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_kernel_prompts_are_present() {
        let t =
            read_prompt_rel(ReviewTarget::Kernel, "technical-patterns.md", 50_000).expect("read");
        assert!(
            t.map(|s| s.len() > 500).unwrap_or(false),
            "kernel technical-patterns.md must be embedded from resources/prompts/kernel/"
        );
    }

    #[test]
    fn reference_context_checks_ubuntu_annotations_justification() {
        let context = build_reference_context(ReviewTarget::Kernel, &[], 100_000, None, None)
            .expect("build context");
        assert!(context.contains("Ubuntu kernel annotations policy"));
        assert!(context.contains("note<...>"));
        assert!(context.contains("global mechanical update"));
    }

    #[test]
    fn ubuntu_annotations_policy_is_kernel_target_only() {
        let qemu_context = build_reference_context(ReviewTarget::Qemu, &[], 100_000, None, None)
            .expect("build QEMU context");
        assert!(!qemu_context.contains("Ubuntu kernel annotations policy"));
    }

    #[test]
    fn embedded_qemu_prompts_are_present() {
        for rel in [
            "technical-patterns.md",
            "callstack.md",
            "false-positive-guide.md",
            "severity.md",
            "inline-template.md",
            "coding-style.md",
            "subsystem/subsystem.md",
            "subsystem/locking.md",
        ] {
            let t = read_prompt_rel(ReviewTarget::Qemu, rel, 50_000).expect("read");
            assert!(
                t.map(|s| s.len() > 200).unwrap_or(false),
                "QEMU prompt {rel} must be embedded and non-trivial (resources/prompts/qemu/)"
            );
        }
    }

    #[test]
    fn kernel_subsystem_mapping_selects_expected_guides() {
        let picked = pick_subsystem_files(
            ReviewTarget::Kernel,
            &[
                "mm/page_alloc.c".to_string(),
                "drivers/net/ethernet/example.c".to_string(),
                "kernel/sched/core.c".to_string(),
            ],
        );
        for want in [
            "subsystem/mm-alloc.md",
            "subsystem/networking-drivers.md",
            "subsystem/networking-core.md",
            "subsystem/scheduler.md",
        ] {
            assert!(picked.contains(&want.to_string()), "missing {want}");
            assert!(
                prompt_exists(ReviewTarget::Kernel, want),
                "{want} not embedded"
            );
        }
    }

    #[test]
    fn qemu_subsystem_mapping_selects_expected_guides() {
        let picked = pick_subsystem_files(
            ReviewTarget::Qemu,
            &[
                "hw/virtio/virtio-blk.c".to_string(),
                "hw/vfio/pci.c".to_string(),
                "hw/arm/smmuv3.c".to_string(),
            ],
        );
        for want in [
            "subsystem/virtio.md",
            "subsystem/vfio.md",
            "subsystem/smmuv3.md",
            "subsystem/arm.md", // smmuv3.c also lives under hw/arm/ → stacks
        ] {
            assert!(picked.contains(&want.to_string()), "missing {want}");
            assert!(
                prompt_exists(ReviewTarget::Qemu, want),
                "{want} not embedded"
            );
        }
    }

    #[test]
    fn phase0_narrowing_skips_path_matched_when_picks_present() {
        // mm/page_alloc.c would normally pull in subsystem/mm-alloc.md via pick_subsystem_files,
        // but Phase 0 picked a different guide → mm-alloc.md should be absent and the picked
        // guide should be present.
        let ref_md = build_reference_context(
            ReviewTarget::Kernel,
            &["mm/page_alloc.c".to_string()],
            300_000,
            Some(&["subsystem/networking-core.md".to_string()]),
            None,
        )
        .expect("ctx");
        assert!(
            !ref_md.contains("# --- subsystem/mm-alloc.md "),
            "path-matched subsystem must be skipped when Phase 0 has picks"
        );
        assert!(
            ref_md.contains("subsystem/networking-core.md"),
            "Phase 0 pick must be loaded"
        );
    }

    #[test]
    fn phase0_narrowing_fallback_when_picks_empty() {
        // Empty Phase 0 picks → fall back to path-matched behavior.
        let ref_md = build_reference_context(
            ReviewTarget::Kernel,
            &["mm/page_alloc.c".to_string()],
            300_000,
            Some(&[]),
            None,
        )
        .expect("ctx");
        assert!(
            ref_md.contains("subsystem/mm-alloc.md"),
            "path-matched mm-alloc.md must be loaded when Phase 0 picks are empty"
        );
    }

    #[test]
    fn phase0_narrowing_fallback_when_phase0_none() {
        // No Phase 0 at all → path-matched behavior unchanged.
        let ref_md = build_reference_context(
            ReviewTarget::Kernel,
            &["mm/page_alloc.c".to_string()],
            300_000,
            None,
            None,
        )
        .expect("ctx");
        assert!(
            ref_md.contains("subsystem/mm-alloc.md"),
            "path-matched mm-alloc.md must be loaded when Phase 0 was not run"
        );
    }

    #[test]
    fn followup_summary_appended_when_provided() {
        let summary = "## Upstream follow-up summary\nConsensus: under_discussion\n";
        let ref_md = build_reference_context(
            ReviewTarget::Kernel,
            &["mm/page_alloc.c".to_string()],
            300_000,
            None,
            Some(summary),
        )
        .expect("ctx");
        assert!(ref_md.contains("# --- upstream follow-up ---"));
        assert!(ref_md.contains("Consensus: under_discussion"));
    }

    #[test]
    fn followup_summary_skipped_when_empty() {
        let ref_md = build_reference_context(
            ReviewTarget::Kernel,
            &["mm/page_alloc.c".to_string()],
            300_000,
            None,
            Some("   "),
        )
        .expect("ctx");
        assert!(!ref_md.contains("# --- upstream follow-up ---"));
    }
}
