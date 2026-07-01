// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use rust_embed::{EmbeddedFile, RustEmbed};

use super::{render_local_prompts, TargetSpec};

#[derive(RustEmbed)]
#[folder = "resources/prompts/qemu/"]
struct PromptCorpus;

#[derive(RustEmbed)]
#[folder = "resources/prompts/qemu.local/"]
struct LocalPromptCorpus;

pub struct QemuTarget;

pub static TARGET: QemuTarget = QemuTarget;

// QEMU path -> subsystem-guide map (boro-authored, resources/prompts/qemu/).
const SUBSYSTEM_MAP: &[(&str, &str)] = &[
    ("hw/virtio/", "virtio.md"),
    ("hw/net/", "net.md"),
    ("net/", "net.md"),
    ("hw/block/", "block.md"),
    ("block/", "block.md"),
    ("hw/scsi/", "scsi.md"),
    ("hw/usb/", "usb.md"),
    ("hw/pci/", "pci.md"),
    ("hw/vfio/", "vfio.md"),
    ("hw/arm/smmu", "smmuv3.md"),
    ("hw/arm/tegra241-cmdqv", "smmuv3.md"),
    ("hw/arm/", "arm.md"),
    ("migration/", "migration.md"),
    ("accel/kvm/", "kvm.md"),
    ("accel/tcg/", "tcg.md"),
    ("tcg/", "tcg.md"),
    ("target/", "tcg.md"),
    ("qapi/", "qapi.md"),
    ("ui/", "ui.md"),
    ("chardev/", "chardev.md"),
    ("hw/core/", "qdev.md"),
    ("system/", "memory.md"),
    ("softmmu/", "memory.md"),
];

const CORE_FILES: &[&str] = &[
    "technical-patterns.md",
    "callstack.md",
    "subsystem/locking.md",
    "coding-style.md",
];

const REVIEWER_SYSTEM_PROMPT: &str =
    "You are an expert QEMU maintainer reviewing a patch to the QEMU \
machine emulator and virtualizer. Treat all guest-controlled input (device registers, DMA buffers, \
virtqueue descriptors, migration streams) as untrusted. Follow the reference material exactly. \
Be concise in JSON string fields but precise in reasoning.";

const PHASE0_SYSTEM_PROMPT: &str = "You are an AI assistant preparing a QEMU patch review.\n\
Review the provided patch and select all potentially relevant subsystem guides from the index below.\n\
CRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\n\
You MUST respond with ONLY a JSON object, no other text. Example:\n\
{\"selected_prompts\": [\"virtio.md\", \"memory.md\"]}\n";

const LKML_SYSTEM_PROMPT: &str = "You are an automated review bot preparing a reply for the qemu-devel mailing list. \
Follow the formatting rules in the user message exactly. Output plain text only: no markdown document structure around the reply, no wrapping the entire message in code fences.";

const SECOND_OPINION_SYSTEM_PROMPT: &str = "You are an automated QEMU reviewer giving an independent second opinion on a commit that was already reviewed by a multi-stage pipeline. \
Review the full patch with the reference context and current pipeline findings in mind. \
Your job is to find concrete, reportable issues the main pipeline may have missed or under-specified, not to validate the existing findings. \
Avoid re-emitting the same finding unless you can provide materially better evidence, location, or severity framing. \
Only emit a finding if you can point to specific code in the diff or pre-fetched source context as concrete evidence. \
Do not report the bug that the patch is fixing: a defect visible only in removed/old code is not a finding when the new code fixes it. \
Speculation, generic 'this could be racy', or 'should add bounds check' without a concrete path: do not emit. \
If you find no additional concrete issues, return an empty findings array - that is an acceptable outcome.";

const QUICK_SUMMARY_SYSTEM_PROMPT: &str = "You are summarizing QEMU patch-review findings for a human reviewer. \
Produce a VERY SHORT plain-text summary (1-3 sentences, ~280 characters max) that highlights the most important issues, preferring Critical and High severity items. \
Mention concrete signals (e.g. a guest-triggerable OOB in device X, a missing bounds check in path Y) when present. \
If the findings list is empty across all commits, say so plainly in a single sentence. \
Output plain text only: no markdown, no bullet points, no headings, no JSON, no code fences, no severity counts (those are rendered separately).";

impl TargetSpec for QemuTarget {
    fn prompt_file(&self, rel: &str) -> Option<EmbeddedFile> {
        PromptCorpus::get(rel)
    }

    fn subsystem_map(&self) -> &'static [(&'static str, &'static str)] {
        SUBSYSTEM_MAP
    }

    fn core_files(&self) -> &'static [&'static str] {
        CORE_FILES
    }

    fn local_reference(&self) -> String {
        render_local_prompts(LocalPromptCorpus::iter(), LocalPromptCorpus::get)
    }

    fn prompts_source_verbose(&self) -> &'static str {
        "embedded resources/prompts/qemu (baked into binary at build time)"
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
