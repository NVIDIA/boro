// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Specialist stage instruction bodies (stages 3-8) bundled as resources.

/// Short label for progress UI (matches the intent of each bundled stage file).
pub fn short_description(stage: u8) -> &'static str {
    match stage {
        3 => "Execution flow verification",
        4 => "Resource management",
        5 => "Locking and concurrency",
        6 => "Security",
        7 => "Build, configuration, and hardware portability",
        8 => "Comment / code consistency",
        _ => "Specialist stage",
    }
}

/// Compact label for the per-step usage table (single lowercase word per stage).
pub fn short_label(stage: u8) -> &'static str {
    match stage {
        3 => "execution",
        4 => "resource",
        5 => "locking",
        6 => "security",
        7 => "hardware",
        8 => "comments",
        _ => "stage?",
    }
}

pub fn instruction_body(stage: u8) -> Option<&'static str> {
    match stage {
        3 => Some(include_str!("../resources/stage-03-execution.md")),
        4 => Some(include_str!("../resources/stage-04-resource.md")),
        5 => Some(include_str!("../resources/stage-05-locking.md")),
        6 => Some(include_str!("../resources/stage-06-security.md")),
        7 => Some(include_str!("../resources/stage-07-hardware.md")),
        8 => Some(include_str!("../resources/stage-08-comment-accuracy.md")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portability_stage_requires_negative_config_checks() {
        let prompt = instruction_body(7).expect("stage 7 prompt");
        assert!(prompt.contains("caller is compiled => provider exists"));
        assert!(prompt.contains("`y`, `m`, and `n`"));
        assert!(prompt.contains("checked-out review tree as authoritative"));
        assert!(prompt.contains("missing prerequisite"));
        assert!(prompt.contains("CONFIG_BAR=n"));
        assert!(prompt.contains("`failing_config`"));
        assert!(prompt.contains("`caller_condition`"));
        assert!(prompt.contains("`provider_condition`"));
        assert!(prompt.contains("structured `proof` object"));
        assert!(prompt.contains("not guaranteed"));

        let single_pass = include_str!("../resources/one-shot-review.md");
        assert!(single_pass.contains("Build / configuration portability"));
        assert!(single_pass.contains("relevant `CONFIG_*={y,m,n}` states"));
        assert!(single_pass.contains("checked-out tree as authoritative"));
        assert!(single_pass.contains("`failing_config`"));
        assert!(single_pass.contains("`provider_condition`"));
        assert!(single_pass.contains("substitute for this proof"));
        assert!(single_pass.contains("Every finding must carry concrete proof"));
        assert!(single_pass.contains("interleaving or lock-order cycle"));
    }

    #[test]
    fn execution_stage_tracks_validation_across_candidate_substitution() {
        let prompt = instruction_body(3).expect("stage 3 prompt");
        assert!(prompt.contains("Validation provenance and candidate substitution"));
        assert!(prompt.contains("validation applies only to the object that was checked"));
        assert!(prompt.contains("alias, sibling, parent,\n   representative, first set bit"));
        assert!(prompt.contains("Membership in the same mask, set, domain"));
        assert!(prompt.contains("validating object A and consuming object B"));

        let portability = instruction_body(7).expect("stage 7 prompt");
        assert!(portability.contains("architecture-overridable predicate or helper"));
        assert!(portability.contains("representative non-stub implementations"));

        let single_pass = include_str!("../resources/one-shot-review.md");
        assert!(single_pass.contains("Execution flow and validation provenance"));
        assert!(single_pass.contains("set/domain membership alone does not carry"));
        assert!(single_pass.contains("architecture-overridable helpers"));
    }
}
