// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use rust_embed::EmbeddedFile;

use crate::config::ReviewTarget;

pub mod kernel;
pub mod qemu;

pub trait TargetSpec: Sync {
    fn prompt_file(&self, rel: &str) -> Option<EmbeddedFile>;
    fn subsystem_map(&self) -> &'static [(&'static str, &'static str)];
    fn core_files(&self) -> &'static [&'static str];
    fn prompts_source_verbose(&self) -> &'static str;
    fn reviewer_system_prompt(&self) -> &'static str;
    fn phase0_system_prompt(&self) -> &'static str;
    fn lkml_system_prompt(&self) -> &'static str;
    fn second_opinion_system_prompt(&self) -> &'static str;
    fn quick_summary_system_prompt(&self) -> &'static str;
}

pub fn spec(target: ReviewTarget) -> &'static dyn TargetSpec {
    match target {
        ReviewTarget::Kernel => &kernel::TARGET,
        ReviewTarget::Qemu => &qemu::TARGET,
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

pub fn second_opinion_system_prompt(target: ReviewTarget) -> &'static str {
    spec(target).second_opinion_system_prompt()
}

pub fn quick_summary_system_prompt(target: ReviewTarget) -> &'static str {
    spec(target).quick_summary_system_prompt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_system_prompts_are_target_specific() {
        assert!(reviewer_system_prompt(ReviewTarget::Kernel).contains("Linux kernel"));
        assert!(reviewer_system_prompt(ReviewTarget::Qemu).contains("QEMU"));
        assert!(phase0_system_prompt(ReviewTarget::Kernel).contains("Linux kernel"));
        assert!(phase0_system_prompt(ReviewTarget::Qemu).contains("QEMU"));
        assert!(lkml_system_prompt(ReviewTarget::Kernel).contains("LKML"));
        assert!(lkml_system_prompt(ReviewTarget::Qemu).contains("qemu-devel"));
        assert!(second_opinion_system_prompt(ReviewTarget::Kernel).contains("kernel reviewer"));
        assert!(second_opinion_system_prompt(ReviewTarget::Qemu).contains("QEMU reviewer"));
        assert!(quick_summary_system_prompt(ReviewTarget::Kernel).contains("Linux kernel"));
        assert!(quick_summary_system_prompt(ReviewTarget::Qemu).contains("QEMU"));
    }
}
