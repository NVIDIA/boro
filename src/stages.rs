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
        7 => "Hardware and architecture portability",
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
