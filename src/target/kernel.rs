// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use rust_embed::{EmbeddedFile, RustEmbed};

use super::TargetSpec;

#[derive(RustEmbed)]
#[folder = "resources/prompts/kernel/"]
struct PromptCorpus;

pub struct KernelTarget;

pub static TARGET: KernelTarget = KernelTarget;

// Generated from resources/prompts/kernel/subsystem/subsystem.md by
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

const REVIEWER_SYSTEM_PROMPT: &str = "You are an expert Linux kernel maintainer. \
Follow the reference material exactly. Be concise in JSON string fields but precise in reasoning.";

const PHASE0_SYSTEM_PROMPT: &str = "You are an AI assistant preparing a Linux kernel patch review.\n\
Review the provided patch and select all potentially relevant subsystem guides from the index below.\n\
CRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\n\
You MUST respond with ONLY a JSON object, no other text. Example:\n\
{\"selected_prompts\": [\"networking.md\", \"mm.md\"]}\n";

const LKML_SYSTEM_PROMPT: &str = "You are an automated review bot preparing a reply for the Linux Kernel Mailing List (LKML). \
Follow the formatting rules in the user message exactly. Output plain text only: no markdown document structure around the reply, no wrapping the entire message in code fences.";

const SECOND_OPINION_SYSTEM_PROMPT: &str = "You are an automated kernel reviewer giving an independent second opinion on a commit that was already reviewed by a multi-stage pipeline. \
Review the full patch with the reference context and current pipeline findings in mind. \
Your job is to find concrete, reportable issues the main pipeline may have missed or under-specified, not to validate the existing findings. \
Avoid re-emitting the same finding unless you can provide materially better evidence, location, or severity framing. \
Only emit a finding if you can point to specific code in the diff or pre-fetched source context as concrete evidence. \
Speculation, generic 'this could be racy', or 'should add bounds check' without a concrete path: do not emit. \
If you find no additional concrete issues, return an empty findings array - that is an acceptable outcome.";

pub const QUICK_SUMMARY_SYSTEM_PROMPT: &str = "You are summarizing Linux kernel patch-review findings for a human reviewer. \
Produce a VERY SHORT plain-text summary (1-3 sentences, ~280 characters max) that highlights the most important issues, preferring Critical and High severity items. \
Mention concrete signals (e.g. a UAF in driver X, a missing lock in path Y) when present. \
If the findings list is empty across all commits, say so plainly in a single sentence. \
Output plain text only: no markdown, no bullet points, no headings, no JSON, no code fences, no severity counts (those are rendered separately).";

impl TargetSpec for KernelTarget {
    fn prompt_file(&self, rel: &str) -> Option<EmbeddedFile> {
        PromptCorpus::get(rel)
    }

    fn subsystem_map(&self) -> &'static [(&'static str, &'static str)] {
        SUBSYSTEM_MAP
    }

    fn core_files(&self) -> &'static [&'static str] {
        CORE_FILES
    }

    fn prompts_source_verbose(&self) -> &'static str {
        "embedded resources/prompts/kernel (baked into binary at build time)"
    }

    fn reviewer_system_prompt(&self) -> &'static str {
        REVIEWER_SYSTEM_PROMPT
    }

    fn phase0_system_prompt(&self) -> &'static str {
        PHASE0_SYSTEM_PROMPT
    }

    fn lkml_system_prompt(&self) -> &'static str {
        LKML_SYSTEM_PROMPT
    }

    fn second_opinion_system_prompt(&self) -> &'static str {
        SECOND_OPINION_SYSTEM_PROMPT
    }

    fn quick_summary_system_prompt(&self) -> &'static str {
        QUICK_SUMMARY_SYSTEM_PROMPT
    }
}
