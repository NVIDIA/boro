# CPU Models

`src/cpu/` handles CPU model definitions, feature decoding, baseline, and
comparison across architectures (x86, ARM, ppc64, s390). It feeds guest CPU
configuration and migration compatibility.

## Core invariants

- CPU feature/model data is loaded from `src/cpu_map/` XML and from client domain
  XML; unknown model/feature names must be handled (reported, not used as an
  unchecked index). `virCPUDefParseXML` accessors follow the usual `virXPath*`
  check rules.
- `cpuBaseline` / `cpuCompare` must be deterministic and not mutate their input
  defs unexpectedly; feature add/remove lists are sets — guard against
  duplicates and ensure both the "require" and "disable" sides are consistent.
- Architecture dispatch goes through the `cpuArchDriver` table; a new feature
  must be wired for each arch it applies to, and arch-specific code must not run
  for the wrong arch.
- CPU model used for a guest affects **migration compatibility**: changing how a
  model expands its features can break migration to/from older libvirt. Such
  changes need versioned/explicit handling.

## Common findings

- Unknown CPU model/feature name used without a checked lookup.
- Feature set built with duplicates or inconsistent require/disable state.
- Baseline/compare mutating an input def or being non-deterministic.
- Model-expansion change that silently alters migration compatibility.
