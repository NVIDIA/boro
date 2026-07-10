<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 7. Build and configuration portability (libvirt)

You are reviewing whether this patch remains valid across libvirt's supported
build configurations. libvirt is configured with meson: optional
drivers/features are gated by `WITH_*` macros emitted into the generated
`config.h` (e.g. `WITH_QEMU`, `WITH_LIBXL`, `WITH_LXC`, `WITH_SELINUX`,
`WITH_APPARMOR`, `WITH_DTRACE`), and platform differences are handled with
`#ifdef`/gnulib. There is no hardware/DMA audit for libvirt — this is a pure
build/configuration and symbol-availability review.

## Configuration and linkage audit

Use the checked-out review tree as authoritative. Do not assume that a helper,
stub, declaration, or prerequisite from a newer upstream tree exists. A
dependency absent from the reviewed commit or series is absent.

For every function, macro, type, or symbol newly referenced by added code:

1. Locate its declaration and definition in the checked-out tree. Use repository
   tools when the pre-fetched context does not show the complete declaration,
   its surrounding guards, or its build ownership (which meson `WITH_*`
   condition compiles it).
2. Record the conditions under which the caller and provider are compiled:
   `#if`/`#ifdef WITH_*` guards, meson `if`/`conditional` object selection, and
   whether the symbol is driver-local or exported.
3. Prove the implication **caller is compiled => provider exists** for every
   relevant configuration, including the disabled form of an optional feature
   (`WITH_FOO` both defined and undefined). A runtime check does not make an
   unavailable compile-time declaration safe.
4. Check header/stub fallbacks (e.g. the `!WITH_FOO` stub) have compatible types
   and semantics.
5. **Symbol versioning**: a new public symbol must be added to the matching
   `*.syms` file (e.g. `src/libvirt_public.syms`, `src/libvirt_private.syms`, or
   a driver `.syms`); a missing entry is a link/visibility failure. A new RPC
   wire element must update the protocol (`*.x`) and its generated code
   consistently.
6. If the patch relies on a prerequisite commit, verify it is an ancestor of the
   reviewed tree or earlier in the series; otherwise report the missing
   prerequisite.

Report a concern only with concrete evidence. For every configuration or linkage
concern, the `reasoning` field MUST contain a proof with all of the following,
and add them as a structured `proof` object with these four string fields:

1. `failing_config`: at least one valid failing configuration, such as
   `WITH_SELINUX undefined`;
2. `caller_condition`: the exact guard/meson condition under which the new
   caller is compiled;
3. `provider_condition`: the exact condition guarding the declaration,
   definition, export, `.syms` entry, or fallback stub in the checked-out tree;
4. `failure`: the concrete consequence, such as an undeclared identifier, an
   unresolved/unexported symbol, or incompatible stub semantics.

Verify these facts with repository tools before emitting the concern. Do not use
"may", "might", "not guaranteed", or "could be absent" as a substitute for the
proof. If you cannot complete the proof from the checked-out tree, do not emit
the concern.

Example pattern: added code in a common file calls `virFooBar()`, but the
checked-out header declares `virFooBar()` only under `WITH_FOO`. Unless the
caller is also guarded or a `!WITH_FOO` stub exists in this tree, report that a
`WITH_FOO`-disabled build fails to compile/link.
