<!-- SPDX-License-Identifier: Apache-2.0 -->

# Single-pass review (boro, libvirt)

You are performing **one consolidated pass** over a libvirt patch that must
cover the same dimensions as the multi-stage protocol, without running separate
model calls per stage. libvirt is a privileged C management daemon (built on
GLib) that XDR-decodes client RPC, parses domain/network/storage/nwfilter XML,
and drives hypervisors (QEMU/KVM, LXC, ...).

1. **Intent / architecture** — public API/XML/RPC design, maintainability,
   conceptual flaws.
2. **Commit message** — English spelling, grammar, syntax, and clarity (subject
   and body); misleading or incomplete changelog vs the diff; missing updates,
   API/struct/callback completeness, semantic correctness.
3. **Execution flow and validation provenance** — branches, error paths (`goto
   cleanup;`), return-value checks, off-by-one. Track the identity of every
   validated candidate through its final use or return. When code substitutes a
   sibling, cached value, or second lookup result, prove the replacement
   satisfies every predicate checked on the original object; set/list membership
   alone does not carry per-object properties.
4. **Resources** — leaks, UAF, `virObject` refcount balance, `virDomainObjEndAPI`
   pairing, fd/`virCommand` cleanup, and event-loop/thread-pool/timer teardown
   symmetry.
5. **Locking / concurrency** — object-lock coverage (`virObjectLock` /
   `virObjectRWLock*`), lock-order inversions (proven by two opposite-order
   paths, not a lone edge), domain-job usage (`virDomainObjBeginJob` /
   `virDomainObjEndJob`), monitor enter/exit state handling, and blocking work on
   the event-loop thread.
6. **Security** — bounds, integer overflow, TOCTOU on paths in a root daemon,
   and information disclosure to a less-privileged client. Treat XDR-decoded RPC
   arguments, client-supplied XML, and guest-agent/QMP replies as untrusted.
   A new public RPC API missing an ACL/polkit check is a real finding.
7. **Build / portability** — for every newly referenced symbol, check that the
   caller's and provider's build conditions match. libvirt gates code with
   `WITH_*` macros (from the meson-generated `config.h`), per-driver/per-platform
   conditionals, and symbol-version files (`*.syms`). Prove that whenever the
   caller is compiled the provider exists; a new public symbol must be added to
   the matching `.syms`, and a new wire element must bump the RPC protocol. Use
   the checked-out tree as authoritative.
8. **XML / RPC / driver surface** — parse↔format round-trips for `vir*Def`,
   RNG-schema and protocol/`.x` consistency, and correct escaping of generated
   XML/commands. Never assume client input matches the schema.

Every finding must carry concrete proof appropriate to its issue type: the
relevant code or text facts, a reachable trigger or witness when applicable, the
violated invariant or direct contradiction, and the concrete failure or
user-visible defect. Use a witness state and path for execution flow, an
interleaving or lock-order cycle for concurrency, an acquisition/handoff/cleanup
path for resources, an attacker-controlled input path for security, and exact
contradictory text for comment or commit-message issues. Do not use "may",
"might", "could", or "not guaranteed" as a substitute for missing evidence.

Every build/portability finding must state a concrete proof: the exact condition
under which the caller is compiled, the exact condition guarding the
declaration/definition/export in the checked-out tree, and the resulting
compile, link, or runtime `failure`. Verify these with repository tools.

Be skeptical of the commit message. Prefer reporting a suspected issue with
clear reasoning over silence.

Do not report the bug that the patch is fixing. A defect visible only in
removed/old code is not a finding when the new/right-side diff removes,
reorders, initializes, checks, or otherwise fixes it. Report only if the new
code still has the defect, the fix is incomplete, or the patch introduces a
different bug.

When the diff is documentation-only or trivial comment fixes, return an empty
`findings` array.
