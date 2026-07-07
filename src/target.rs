// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use rust_embed::EmbeddedFile;

use crate::config::ReviewTarget;

pub mod kernel;
pub mod libvirt;
pub mod qemu;
pub mod virt_manager;

pub trait TargetSpec: Sync {
    fn prompt_file(&self, rel: &str) -> Option<EmbeddedFile>;
    fn subsystem_map(&self) -> &'static [(&'static str, &'static str)];
    fn core_files(&self) -> &'static [&'static str];
    /// Boro-owned, target-specific prompt files outside the upstream corpus.
    fn local_reference(&self) -> String;
    fn prompts_source_verbose(&self) -> &'static str;
    fn reviewer_system_prompt(&self) -> &'static str;
    fn phase0_system_prompt(&self) -> &'static str;
    fn lkml_system_prompt(&self) -> &'static str;
    fn quick_summary_system_prompt(&self) -> &'static str;

    /// Single-pass ("fast" mode) review instructions. Defaults to the kernel
    /// corpus; non-kernel targets override with a domain-appropriate variant so
    /// the model is not handed kernel-only mandates (Kconfig/Kbuild, GFP,
    /// RCU, copy_to_user, DMA).
    fn one_shot_review(&self) -> &'static str {
        include_str!("../resources/fast-review.md")
    }

    /// Distilled false-positive digest injected into the specialist stages.
    /// Defaults to the kernel corpus; non-kernel targets override.
    fn false_positive_digest(&self) -> &'static str {
        include_str!("../resources/false-positive-digest.md")
    }

    /// Per-stage specialist instruction body (stages 3-8). `None` means "use the
    /// shared kernel stage prompt"; a target returns `Some` to supply its own
    /// domain-specific variant.
    fn stage_instructions(&self, _stage: u8) -> Option<&'static str> {
        None
    }

    /// Target-specific addendum appended to the shared, domain-neutral
    /// findings-validation system prompt. `None` keeps the neutral prompt as-is;
    /// a target returns `Some` to add domain-specific linkage/build rules (e.g.
    /// the kernel's Kbuild ownership and loadable-module `EXPORT_SYMBOL` rules).
    fn validation_findings_addendum(&self) -> Option<&'static str> {
        None
    }
}

pub fn spec(target: ReviewTarget) -> &'static dyn TargetSpec {
    match target {
        ReviewTarget::Kernel => &kernel::TARGET,
        ReviewTarget::Qemu => &qemu::TARGET,
        ReviewTarget::Libvirt => &libvirt::TARGET,
        ReviewTarget::VirtManager => &virt_manager::TARGET,
    }
}

pub fn prompt_file(target: ReviewTarget, rel: &str) -> Option<EmbeddedFile> {
    spec(target).prompt_file(rel)
}

pub fn subsystem_map(target: ReviewTarget) -> &'static [(&'static str, &'static str)] {
    spec(target).subsystem_map()
}

pub fn core_files(target: ReviewTarget) -> &'static [&'static str] {
    spec(target).core_files()
}

pub fn local_reference(target: ReviewTarget) -> String {
    spec(target).local_reference()
}

/// Render boro-owned Markdown prompts from a target's `.local` corpus.
pub fn render_local_prompts<I, F>(paths: I, get: F) -> String
where
    I: IntoIterator,
    I::Item: AsRef<str>,
    F: Fn(&str) -> Option<EmbeddedFile>,
{
    let mut paths: Vec<String> = paths
        .into_iter()
        .map(|path| path.as_ref().to_owned())
        .filter(|path| path.ends_with(".md"))
        .collect();
    paths.sort();

    let mut reference = String::new();
    for path in paths {
        let file = get(&path).expect("embedded local prompt must exist");
        let text =
            std::str::from_utf8(file.data.as_ref()).expect("local prompt must be valid UTF-8");
        reference.push_str(&format!("# --- {path} ---\n\n{text}\n"));
    }
    reference
}

pub fn prompts_source_verbose(target: ReviewTarget) -> &'static str {
    spec(target).prompts_source_verbose()
}

pub fn reviewer_system_prompt(target: ReviewTarget) -> &'static str {
    spec(target).reviewer_system_prompt()
}

pub fn phase0_system_prompt(target: ReviewTarget) -> &'static str {
    spec(target).phase0_system_prompt()
}

pub fn lkml_system_prompt(target: ReviewTarget) -> &'static str {
    spec(target).lkml_system_prompt()
}

pub fn quick_summary_system_prompt(target: ReviewTarget) -> &'static str {
    spec(target).quick_summary_system_prompt()
}

pub fn one_shot_review(target: ReviewTarget) -> &'static str {
    spec(target).one_shot_review()
}

pub fn false_positive_digest(target: ReviewTarget) -> &'static str {
    spec(target).false_positive_digest()
}

/// Target-specific specialist stage body, or `None` to fall back to the shared
/// kernel stage prompt in [`crate::stages::instruction_body`].
pub fn stage_instructions(target: ReviewTarget, stage: u8) -> Option<&'static str> {
    spec(target).stage_instructions(stage)
}

/// Findings-validation system prompt for `target`: the shared domain-neutral
/// base ([`crate::api::SYSTEM_REVIEW_VALIDATION_FINDINGS`]) plus any
/// target-specific linkage/build addendum.
pub fn review_validation_findings(target: ReviewTarget) -> String {
    let base = crate::api::SYSTEM_REVIEW_VALIDATION_FINDINGS;
    match spec(target).validation_findings_addendum() {
        Some(addendum) => format!("{}\n\n{}", base.trim_end(), addendum.trim()),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_system_prompts_are_target_specific() {
        assert!(reviewer_system_prompt(ReviewTarget::Kernel).contains("Linux kernel"));
        assert!(reviewer_system_prompt(ReviewTarget::Qemu).contains("QEMU"));
        assert!(reviewer_system_prompt(ReviewTarget::Libvirt).contains("libvirt"));
        assert!(reviewer_system_prompt(ReviewTarget::VirtManager).contains("virt-manager"));
        assert!(phase0_system_prompt(ReviewTarget::Kernel).contains("Linux kernel"));
        assert!(phase0_system_prompt(ReviewTarget::Qemu).contains("QEMU"));
        assert!(phase0_system_prompt(ReviewTarget::Libvirt).contains("libvirt"));
        assert!(phase0_system_prompt(ReviewTarget::VirtManager).contains("virt-manager"));
        assert!(lkml_system_prompt(ReviewTarget::Kernel).contains("LKML"));
        assert!(lkml_system_prompt(ReviewTarget::Qemu).contains("qemu-devel"));
        assert!(lkml_system_prompt(ReviewTarget::Libvirt).contains("devel@lists.libvirt.org"));
        assert!(lkml_system_prompt(ReviewTarget::VirtManager).contains("GitHub pull request"));
        assert!(quick_summary_system_prompt(ReviewTarget::Kernel).contains("Linux kernel"));
        assert!(quick_summary_system_prompt(ReviewTarget::Qemu).contains("QEMU"));
        assert!(quick_summary_system_prompt(ReviewTarget::Libvirt).contains("libvirt"));
        assert!(quick_summary_system_prompt(ReviewTarget::VirtManager).contains("virt-manager"));
    }

    #[test]
    fn findings_validator_is_domain_neutral_except_kernel() {
        // The shared base must not carry kernel-only linkage jargon; those
        // concepts now live in the kernel-specific addendum.
        let base = crate::api::SYSTEM_REVIEW_VALIDATION_FINDINGS;
        for tok in ["Kbuild", "EXPORT_SYMBOL", "loadable-module"] {
            assert!(
                !base.contains(tok),
                "shared findings validator leaked kernel token: {tok}"
            );
        }

        // A kernel review re-attaches the kernel linkage rules via its addendum.
        let kernel = review_validation_findings(ReviewTarget::Kernel);
        assert!(kernel.contains("Kbuild"));
        assert!(kernel.contains("EXPORT_SYMBOL"));
        assert!(kernel.contains("loadable-module"));

        // Non-kernel targets get exactly the domain-neutral base, no kernel rules.
        for t in [
            ReviewTarget::Qemu,
            ReviewTarget::Libvirt,
            ReviewTarget::VirtManager,
        ] {
            let prompt = review_validation_findings(t);
            assert_eq!(
                prompt, base,
                "non-kernel findings validator must equal the shared base"
            );
        }
    }
}
