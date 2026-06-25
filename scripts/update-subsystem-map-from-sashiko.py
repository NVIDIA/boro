#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Derive boro's path-based fallback subsystem map from Sashiko's
# kernel/subsystem/subsystem.md trigger table.

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import List, Optional, Set, Tuple


HEADER = """// Generated from resources/prompts/kernel/subsystem/subsystem.md by
// scripts/update-subsystem-map-from-sashiko.py.
"""


def split_table_row(line: str) -> Optional[List[str]]:
    line = line.strip()
    if not line.startswith("|") or not line.endswith("|"):
        return None
    cols = [col.strip() for col in line.strip("|").split("|")]
    if len(cols) < 3:
        return None
    if cols[0].lower() == "subsystem" or set(cols[0]) <= {"-"}:
        return None
    return cols


def normalize_trigger(trigger: str) -> Optional[str]:
    trigger = trigger.strip()
    trigger = trigger.strip("`")
    trigger = trigger.replace("\\.", ".")
    trigger = trigger.replace("\\*", "*")
    trigger = re.sub(r"\s+", " ", trigger)
    if not trigger:
        return None

    # SUBSYSTEM_MAP only receives changed file paths. Skip prose and symbol-only
    # triggers that need model inspection of the diff body.
    lower = trigger.lower()
    if lower.startswith("any ") or lower.startswith("files marked "):
        return None
    if "/" not in trigger and trigger not in {"Kconfig"}:
        return None

    # Convert common glob/regex table entries into prefixes usable with
    # `path.contains(trigger)`: fs/*.c -> fs/, include/linux/kvm* ->
    # include/linux/kvm, arch/.../kvm.*.h -> arch/.../kvm.
    star_idx = trigger.find("*")
    if star_idx >= 0:
        trigger = trigger[:star_idx].rstrip(".")

    # Drop trailing prose from entries such as "*.yaml in devicetree"; path-like
    # entries with spaces are not meaningful for changed-path matching.
    if " " in trigger:
        return None

    return trigger or None


def parse_subsystem_map(prompt_dir: Path) -> Tuple[List[Tuple[str, str]], List[str]]:
    index = prompt_dir / "subsystem" / "subsystem.md"
    if not index.is_file():
        raise SystemExit(f"missing subsystem index: {index}")

    entries: List[Tuple[str, str]] = []
    seen: Set[Tuple[str, str]] = set()
    warnings: List[str] = []

    for line in index.read_text(encoding="utf-8").splitlines():
        cols = split_table_row(line)
        if cols is None:
            continue

        triggers_col = cols[1]
        guide = cols[2].strip().strip("`")
        if not guide.endswith(".md"):
            continue

        guide_path = prompt_dir / "subsystem" / guide
        if not guide_path.is_file():
            warnings.append(f"skipping {guide}: file not present under {guide_path.parent}")
            continue

        for raw_trigger in triggers_col.split(","):
            trigger = normalize_trigger(raw_trigger)
            if trigger is None:
                continue
            entry = (trigger, guide)
            if entry in seen:
                continue
            seen.add(entry)
            entries.append(entry)

    if not entries:
        raise SystemExit(f"no path-based subsystem triggers found in {index}")
    return entries, warnings


def rust_string(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def render_map(entries: List[Tuple[str, str]]) -> str:
    out = [HEADER, "const SUBSYSTEM_MAP: &[(&str, &str)] = &[\n"]
    for trigger, guide in entries:
        out.append(f"    ({rust_string(trigger)}, {rust_string(guide)}),\n")
    out.append("];")
    return "".join(out)


def replace_map(target_rs: Path, rendered: str) -> bool:
    text = target_rs.read_text(encoding="utf-8")
    pattern = re.compile(
        r"(?ms)(?:// Generated from .*?\n// scripts/update-subsystem-map-from-sashiko\.py\.\n)?"
        r"const SUBSYSTEM_MAP: &\[\(&str, &str\)\] = &\[\n.*?\n\];"
    )
    new_text, count = pattern.subn(rendered, text, count=1)
    if count != 1:
        raise SystemExit(f"failed to find SUBSYSTEM_MAP in {target_rs}")
    if new_text == text:
        return False
    target_rs.write_text(new_text, encoding="utf-8")
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("prompt_dir", type=Path)
    parser.add_argument("target_rs", type=Path)
    args = parser.parse_args()

    entries, warnings = parse_subsystem_map(args.prompt_dir)
    changed = replace_map(args.target_rs, render_map(entries))
    for warning in warnings:
        print(f"update-subsystem-map: {warning}", file=sys.stderr)
    state = "updated" if changed else "already current"
    print(f"update-subsystem-map: {state}; {len(entries)} path trigger(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
