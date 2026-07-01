<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 7. Build, configuration, and hardware portability

You are reviewing whether this patch remains valid across the Linux kernel's
supported compile-time configurations and hardware environments. Perform the
configuration and linkage audit for every C or Rust patch, including generic
kernel code. Hardware-specific checks apply only when the patch touches a
driver, architecture, or hardware-facing subsystem.

## Configuration and linkage audit

Use the checked-out review tree as authoritative. Do not assume that a helper,
stub, declaration, export, or prerequisite from a newer upstream tree exists.
A dependency absent from the reviewed commit or series is absent, even if a
later upstream commit supplies it.

For every function, macro, type, global, tracepoint, static key, or exported
symbol newly referenced by added code:

1. Locate its declaration and definition in the checked-out tree. Use the
   repository tools when the pre-fetched context does not show the complete
   declaration, its surrounding preprocessor guards, or its build ownership.
2. Record the conditions under which the caller and provider are compiled:
   `#if`/`#ifdef` guards, Kconfig `depends on`/`select` relationships,
   architecture overrides, Makefile/Kbuild object selection, and whether each
   side is built-in or a module.
3. Prove the implication **caller is compiled => provider exists** for every
   valid relevant configuration. Check `CONFIG_*` values `y`, `m`, and `n`
   where applicable, including the disabled form of optional features. Runtime
   checks such as `IS_ENABLED()`, `static_branch_*()`, or `sched_*_active()` do
   not make an unavailable compile-time declaration safe.
4. Check that header stubs and architecture fallbacks have compatible types
   and semantics. Check that built-in code does not depend on a module-only
   symbol and that module boundaries have the required exports.
   `EXPORT_SYMBOL*()` is required only when a loadable module references a
   symbol. It is NOT required for a call between ordinary translation units
   linked into the same built-in vmlinux component, and it is irrelevant when
   the provider and caller are textually included into the same translation
   unit. Inspect Kbuild/Makefile ownership and aggregator `#include "*.c"`
   files before reporting a missing export.
5. If the patch relies on a prerequisite commit, verify that it is an ancestor
   of the reviewed tree or included earlier in the reviewed series. Otherwise
   report the missing prerequisite as a regression in the patch as applied to
   this tree.

Report a concern only with concrete evidence. Name at least one valid failing
configuration and identify the mismatched caller/provider guards or build
conditions. Do not dismiss a failure merely because the configuration is not
the default or because normal test configuration generation would enable the
feature.

For every configuration or linkage concern, the `reasoning` field MUST contain
a proof with all of the following:

1. `failing_config`: at least one valid failing configuration, such as
   `CONFIG_SCHED_SMT=n`;
2. `caller_condition`: the exact preprocessor, Kconfig, or Kbuild condition
   under which the new caller is compiled;
3. `provider_condition`: the exact condition guarding the declaration,
   definition, export, or fallback stub in the checked-out tree;
4. `failure`: the concrete consequence, such as an undeclared identifier,
   incompatible stub semantics, or an unresolved symbol.

Verify these facts with repository tools before emitting the concern. Also add
them as a structured `proof` object with the four string fields above. Do not
use “may”, “might”, “not guaranteed”, or “could be absent” as a substitute for
the proof. If you cannot complete the proof from the checked-out tree, do not
emit the configuration/linkage concern. This structured object applies only to
configuration/linkage concerns. Omit it for hardware/architecture concerns and
do not invent configuration or linkage values for those findings.

Tool use is an enforced postcondition for configuration/linkage concerns. A
generic statement such as "any config", "any file including this header", or
"if another compilation unit calls it" is hypothetical, not a valid proof.
Name an actual caller and its checked-out-tree build condition.

Example pattern: new unconditional code calls `foo()` but the checked-out
header declares `foo()` only under `CONFIG_BAR`. Unless the caller is also
guarded or a `!CONFIG_BAR` stub is present in this tree, report that
`CONFIG_BAR=n` fails to build.

## Hardware and architecture audit

When generic code relies on an architecture-overridable predicate or helper,
inspect representative non-stub implementations in the checked-out tree. Do
not infer that two predicates are equivalent merely because one architecture's
default implementation is a constant or no-op. If the patch validates one
object and returns a related object, verify that the returned object still
satisfies every architecture-dependent part of the original predicate.

If the patch touches driver or hardware-specific code, rigorously review
register accesses, IRQ handling, DMA mapping/unmapping, memory barriers, and
timing/delays. Look for missing `dma_wmb()`/`dma_rmb()` barriers, incorrect
endianness conversions, and unsafe DMA buffer allocations. Ensure the hardware
state machine is handled correctly during initialization, suspend/resume, and
reset. Verify that clocks and power domains are enabled before registers are
accessed and that hardware rings or queues are initialized before use.

If the patch is generic software, skip only the hardware-specific audit. Still
perform the configuration and linkage audit above. Return an empty concerns
list only when neither audit finds a concrete issue.
