// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "third_party/prompts/kernel/"]
struct KernelPrompts;

// Generated from third_party/prompts/kernel/subsystem/subsystem.md by
// scripts/update-subsystem-map-from-sashiko.py.
const SUBSYSTEM_MAP: &[(&str, &str)] = &[
    ("net/", "networking-core.md"),
    ("drivers/net/", "networking-drivers.md"),
    ("Documentation/netlink/specs/", "netlink.md"),
    ("mm/memory.c", "mm-pagetable.md"),
    ("mm/mprotect.c", "mm-pagetable.md"),
    ("mm/pagewalk.c", "mm-pagetable.md"),
    ("mm/filemap.c", "mm-folio.md"),
    ("mm/swap.c", "mm-folio.md"),
    ("mm/truncate.c", "mm-folio.md"),
    ("mm/huge_memory.c", "mm-largepage.md"),
    ("mm/hugetlb.c", "mm-largepage.md"),
    ("mm/memory-failure.c", "mm-largepage.md"),
    ("mm/vma.c", "mm-vma.md"),
    ("mm/mmap.c", "mm-vma.md"),
    ("mm/mmap_lock.c", "mm-vma.md"),
    ("mm/page_alloc.c", "mm-alloc.md"),
    ("mm/slub.c", "mm-alloc.md"),
    ("mm/vmalloc.c", "mm-alloc.md"),
    ("mm/vmscan.c", "mm-reclaim.md"),
    ("mm/swap_state.c", "mm-reclaim.md"),
    ("mm/migrate.c", "mm-reclaim.md"),
    ("mm/memcontrol.c", "mm-reclaim.md"),
    ("fs/", "vfs.md"),
    ("kernel/sched/", "scheduler.md"),
    ("kernel/bpf/", "bpf.md"),
    ("tools/lib/bpf/", "bpf.md"),
    ("tools/testing/selftests/bpf", "bpf.md"),
    ("tools/lib/bpf/", "libbpf.md"),
    ("kernel/workqueue.c", "workqueue.md"),
    ("fs/btrfs/", "btrfs.md"),
    ("drivers/gpu/drm/", "drm.md"),
    ("fs/nfsd/", "nfsd.md"),
    ("fs/lockd/", "nfsd.md"),
    ("net/sunrpc/", "sunrpc.md"),
    ("io_uring/", "io_uring.md"),
    ("drivers/pmdomain/", "pmdomain.md"),
    ("include/linux/pm_runtime.h", "pm.md"),
    ("fs/sysfs/", "sysfs.md"),
    ("drivers/cxl/", "cxl.md"),
    ("net/bluetooth/", "bluetooth.md"),
    ("drivers/tty/", "tty.md"),
    ("drivers/pci/", "pci.md"),
    ("fs/smb/server/", "smb-ksmbd.md"),
    ("drivers/of/", "of.md"),
    ("tools/perf/", "perf.md"),
    ("arch/mips/", "mips.md"),
    ("drivers/hwmon/", "hwmon.md"),
    ("drivers/net/wireless/", "wireless.md"),
    ("net/mac80211/", "wireless.md"),
    ("tools/testing/selftests/", "selftests.md"),
    ("Documentation/devicetree/bindings/", "dt-bindings.md"),
    ("drivers/usb/storage/", "usb-storage.md"),
    ("drivers/ata/", "ata.md"),
    ("Kconfig", "kconfig.md"),
    ("scripts/", "build.md"),
    ("tools/", "build.md"),
    ("drivers/input/", "input.md"),
    ("include/linux/input.h", "input.md"),
    ("include/linux/input/", "input.md"),
    ("tools/objtool/", "objtool.md"),
    ("lib/test_kho.c", "kho.md"),
    ("drivers/i2c/", "i2c.md"),
    ("virt/kvm/", "kvm.md"),
    ("include/linux/kvm", "kvm.md"),
    ("arch/arm64/", "arm64.md"),
    ("arch/arm64/kvm/", "kvm-arm64.md"),
    ("arch/arm64/kvm/hyp/", "hyp-arm64.md"),
    ("arch/arm64/include/asm/kvm", "hyp-arm64.md"),
    ("drivers/iommu/arm/arm-smmu-v3/pkvm/", "hyp-arm64.md"),
];

const CORE_FILES: &[&str] = &[
    "technical-patterns.md",
    "callstack.md",
    "subsystem/locking.md",
];

/// Shown with `--verbose`; prompts always come from `third_party/prompts/kernel/` embedded at compile time.
pub const PROMPTS_SOURCE_VERBOSE: &str =
    "embedded third_party/prompts/kernel (baked into binary at build time)";

/// Pick subsystem/*.md files from changed paths (best-effort). Always includes locking via CORE_FILES.
pub fn pick_subsystem_files(changed: &[String]) -> Vec<String> {
    use std::collections::HashSet;
    let mut out = HashSet::new();
    for p in changed {
        let pl = p.replace('\\', "/");
        for (prefix, md) in SUBSYSTEM_MAP {
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

fn read_prompt_rel(rel: &str, max: usize) -> Result<Option<String>> {
    let Some(file) = KernelPrompts::get(rel) else {
        return Ok(None);
    };
    let raw = std::str::from_utf8(file.data.as_ref())
        .with_context(|| format!("embedded prompt {rel} is not valid UTF-8"))?;
    Ok(Some(truncate_utf8(raw, max)))
}

fn prompt_exists(rel: &str) -> bool {
    KernelPrompts::get(rel).is_some()
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
    changed_paths: &[String],
    max_total: usize,
    phase0_selected: Option<&[String]>,
    followup_summary: Option<&str>,
) -> Result<String> {
    use std::collections::HashSet;
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut loaded_subsystem: HashSet<String> = HashSet::new();

    let one_shot = include_str!("../resources/one-shot-review.md");
    parts.push(format!("# boro instructions\n{one_shot}\n"));
    used += parts.last().map(|s| s.len()).unwrap_or(0);

    for rel in CORE_FILES {
        let chunk = read_prompt_rel(rel, max_total / 4).context("core prompt read")?;
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
        let mut sub = pick_subsystem_files(changed_paths);
        sub.sort();
        for rel in sub {
            loaded_subsystem.insert(rel.clone());
            let chunk = read_prompt_rel(&rel, max_total / 3).context("subsystem read")?;
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
            if !prompt_exists(&rel) {
                continue;
            }
            loaded_subsystem.insert(rel.clone());
            let chunk = read_prompt_rel(&rel, max_total / 3).context("phase0 subsystem read")?;
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
pub fn load_stage_prompt_files(stage: u8, max_each: usize) -> Result<String> {
    let rels: &[&str] = match stage {
        3 => &["callstack.md", "technical-patterns.md"],
        5 => &["subsystem/locking.md"],
        _ => &[],
    };
    let mut s = String::new();
    for rel in rels {
        if let Some(t) = read_prompt_rel(rel, max_each)? {
            s.push_str(&format!("\n\n# --- {rel} ---\n\n{t}"));
        }
    }
    Ok(s)
}

pub fn load_consolidation_extras(max_each: usize) -> Result<String> {
    let mut s = String::new();
    for rel in ["false-positive-guide.md", "severity.md"] {
        if let Some(t) = read_prompt_rel(rel, max_each)? {
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
pub fn load_subsystem_index(max_chars: usize) -> Result<Option<String>> {
    read_prompt_rel("subsystem/subsystem.md", max_chars)
}

/// `inline-template.md` for LKML-style report.
pub fn load_inline_template(max_chars: usize) -> Result<Option<String>> {
    read_prompt_rel("inline-template.md", max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_kernel_prompts_are_present() {
        let t = read_prompt_rel("technical-patterns.md", 50_000).expect("read");
        assert!(
            t.map(|s| s.len() > 500).unwrap_or(false),
            "technical-patterns.md must be embedded and non-trivial; if this fails in debug, enable rust-embed \"debug-embed\" (see Cargo.toml)"
        );
    }

    #[test]
    fn phase0_narrowing_skips_path_matched_when_picks_present() {
        // mm/page_alloc.c would normally pull in subsystem/mm-alloc.md via pick_subsystem_files,
        // but Phase 0 picked a different guide → mm-alloc.md should be absent and the picked
        // guide should be present.
        let ref_md = build_reference_context(
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
        let ref_md =
            build_reference_context(&["mm/page_alloc.c".to_string()], 300_000, Some(&[]), None)
                .expect("ctx");
        assert!(
            ref_md.contains("subsystem/mm-alloc.md"),
            "path-matched mm-alloc.md must be loaded when Phase 0 picks are empty"
        );
    }

    #[test]
    fn phase0_narrowing_fallback_when_phase0_none() {
        // No Phase 0 at all → path-matched behavior unchanged.
        let ref_md = build_reference_context(&["mm/page_alloc.c".to_string()], 300_000, None, None)
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
        let ref_md =
            build_reference_context(&["mm/page_alloc.c".to_string()], 300_000, None, Some("   "))
                .expect("ctx");
        assert!(!ref_md.contains("upstream follow-up"));
    }
}
