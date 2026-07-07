<!-- SPDX-License-Identifier: Apache-2.0 -->

# What NOT to flag — false-positive digest (libvirt specialist stages)

This is a tight subset of `false-positive-guide.md` distilled for the specialist
stages. Apply these rules BEFORE you emit a concern, not after. If a concern
fails any applicable rule, drop it. The full guide still runs at consolidation;
your job here is to stop weak concerns at the source.

## Core principle

If you cannot point to **specific code in the diff** that proves the issue, do
not emit it. "Could happen," "might race," "should validate" → drop.

## Concrete rules

### 1. Defensive programming requests
Do NOT request bounds checks, NULL checks, or input validation unless you can
show:
- the value comes from an untrusted source (XDR-decoded RPC argument,
  client-supplied XML, guest-agent/QMP reply, file/`/proc` content), AND
- an actual code path in the diff reaches the use without intervening
  validation.

`g_new0`/`g_strdup`/`g_strdup_printf` abort on OOM — do NOT ask for a NULL check
after them. Generic "add a check for safety" with no proof: drop.

### 2. API misuse assumptions
Do NOT report "caller might not hold the lock/job" or "caller might pass NULL"
without showing the actual calling path that does so. libvirt functions often
document preconditions (a locked+ref'd `virDomainObj`, a held job) that callers
must satisfy — check the callers before flagging.

### 3. Unverifiable claims from the commit message
The commit message is not evidence on its own. If the author says "this is safe
because Y," verify Y from the code. Do not flag based only on a missing
justification, and do not exonerate based only on the changelog's promises.

### 4. Locking complaints — caller-first
Before emitting any "missing lock" concern, trace 2-3 levels up the call chain.
If a documented caller already holds the object lock or the domain job, drop the
concern. Taking `driver->lock` (e.g. via `virQEMUDriverGetConfig()`) while a
domain is locked is a normal, short critical section — not an ordering bug by
itself. A lock-order concern needs two concrete paths taking the same pair of
locks in opposite order.

### 5. Monitor / job state after ExitMonitor
Do NOT demand a `virDomainObjIsActive(vm)` re-check after every
`qemuDomainObjExitMonitor()`. The held domain job blocks concurrent destructive
operations, so updating `vm->def`/private state right after ExitMonitor is
normal. Flag stale-state use only when the path holds no job (or the specific
value could actually change during the monitor call) AND reuse is unsafe.

### 6. Use-after-free vs. use-then-free
Only flag the sequence `alloc → use → free → use` (or `alloc → free → use`).
`alloc → use → free` (normal cleanup) is not a UAF. If ownership transferred
(added to a hash/list, handed to a `virThreadPool` job, an event callback, or a
timer that took a ref), the original holder unref'ing is expected.

### 7. Resource leaks — ownership matters
Not a leak when:
- the object was added to a hash/list/`virDomainObjList` for later cleanup,
- ownership transferred to another subsystem or an async callback,
- a `g_autoptr`/`g_autofree`/`g_auto(virBuffer)` variable frees it on scope exit,
- `g_steal_pointer()` handed it off deliberately.
Check for an error label / `g_auto*` / `virObjectUnref`/`virDomainObjEndAPI` in
the diff before flagging.

### 8. Races — show two concurrent paths
A race concern must name both paths and the contested state. "X could race with
Y" with no specific call sites is not a finding. Code that detects the invalid
state and aborts is only safe if every instruction between the race window
opening and the abort point is safe under the invalidated state.

### 9. Uninitialized variables
Only flag **reads** of uninitialized memory. Assigning a value initializes it.
Passing an uninitialized variable to a function that writes it before reading (a
common out-parameter pattern, e.g. `virStrToLong_*`) is fine.

### 10. NULL dereference
Before flagging a NULL deref, check whether an earlier line (or the calling
convention) guarantees non-NULL. A `g_new0` result, a `virDomainObj` returned
locked+ref'd by `virDomainObjListFindByUUID`, or a value already checked with
`if (!p) ...` does not need another NULL check downstream.

### 11. Style and naming
Do not emit style complaints (naming, function size, comment style) as
substantive findings. They belong in commit-message concerns (`msg:style`) at
most, and usually not at all.

### 12. Patch-series context
If the diff is patch N of a series and a concern is fixed in a later patch of
the same series, treat it as not-a-finding (or note the later-fix patch).

### 13. Fixed old-code bugs
Do NOT emit a concern merely because the removed/old code had a real bug. If the
reviewed diff moves, deletes, initializes, checks, or otherwise fixes that old
behavior, the old bug is evidence the patch is a fix, not a finding. Only report
it if the new/right-side code still has the bug, the fix is incomplete, or the
patch introduces a different bug.

### 14. Function-like macro expansion
Do NOT reason from the unexpanded spelling of a function-like macro body (e.g. a
`VIR_*` or glib `g_*`/`G_*` macro). For any concern that depends on macro
semantics, expand the complete invocation chain token by token: bind formal
parameters to actual arguments, substitute every matching preprocessing token,
and rescan for nested expansions. Punctuation or member-access operators do not
make a matching parameter token literal. Account for stringification, token
pasting, and variadic arguments when present. Keep the concern only if it
remains true in the final expanded token stream.

## How to write a concern that survives consolidation

- Name the function, file, and line/region from the diff.
- Quote or paraphrase the specific code, not the general pattern.
- State the trigger condition that actually fires (not "if input is malformed").
- For locking/UAF/race, list the call chain or sequence you traced.

If you cannot do these for a concern, **do not emit it**. The consolidator will
not invent the evidence; it can only drop what you wrote.
