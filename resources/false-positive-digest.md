<!-- SPDX-License-Identifier: Apache-2.0 -->

# What NOT to flag — false-positive digest (specialist stages)

This is a tight subset of `false-positive-guide.md` distilled for upstream stages.
Apply these rules BEFORE you emit a concern, not after. If a concern fails any
applicable rule, drop it. The full guide still runs at consolidation; your job
here is to stop weak concerns at the source.

## Core principle

If you cannot point to **specific code in the diff** that proves the issue, do
not emit it. "Could happen," "might race," "should validate" → drop.

## Concrete rules

### 1. Defensive programming requests
Do NOT request bounds checks, NULL checks, or input validation unless you can
show:
- the value comes from an untrusted source (user input, network, file content),
  AND
- an actual code path in the diff reaches the dereference without intervening
  validation.

Generic "add a check for safety" with no proof: drop.

### 2. API misuse assumptions
Do NOT report "caller might not hold X" / "caller might pass NULL" without
showing the actual calling path that does so. Internal kernel APIs may have
documented preconditions (function name, kerneldoc, header) that callers are
required to satisfy — check those before flagging.

### 3. Unverifiable claims from the commit message
The commit message is not evidence on its own. If the author says "this is
safe because Y," verify Y from the code. Do not flag based only on missing
justification, and do not exonerate based only on the changelog's promises.

### 4. Locking complaints — caller-first
Before emitting any "missing lock" concern, mentally trace 2–3 levels up the
call chain. If a documented caller already holds the lock (BPF prog protected
by RCU, sysfs handler called under `kn->active`, etc.), drop the concern.
RCU-protected sections are real locking; "no spinlock" is not a bug if RCU
covers the access.

### 5. Use-after-free vs. use-then-free
Only flag the sequence `alloc → use → free → use`. The sequences `alloc → use
→ free` (normal cleanup) and `alloc → free → use` is what to flag; everything
else is not UAF. If ownership transferred to another subsystem (added to a
list, handed to a workqueue, passed to RCU), the original holder freeing is
expected.

### 6. Resource leaks — ownership matters
Not a leak when:
- the object was added to a list/tree/queue for later cleanup
- ownership transferred to another subsystem
- cleanup happens in an async path (workqueue, RCU callback, fput)
- the path is error-handling for an `__init` function that aborts the kernel.
Check for an error label / `goto out_*` / `put_*()` in the diff before flagging.

### 7. Races — show two concurrent paths
A race concern must name both paths and the contested state. "X could race
with Y" with no specific call sites is not a finding. Code that detects the
invalid state and aborts is only safe if **every** instruction between the
race window opening and the abort point is safe under the invalidated state.

### 8. Performance regressions — read the changelog
Intentional regressions (simplicity over speed, correctness fix that adds a
lock, removing an optimization the commit message explains) are not findings.
Only flag perf concerns when:
- the regression is measurable from the code (extra allocation in a hot path,
  added syscall in a loop), AND
- the commit message does not acknowledge it.

### 9. Uninitialized variables
Only flag **reads** of uninitialized memory. Assigning a value to a variable
initializes it. Passing an uninitialized variable to a function that writes
to it before reading (a common out-parameter pattern) is fine.

### 10. NULL dereference
Before flagging a NULL deref, check whether an earlier line in the same
function (or the calling convention) guarantees non-NULL. `container_of`,
`list_for_each_entry`, return values from `alloc_*()` checked with `if (!p)
return -ENOMEM;` — these do not need an additional NULL check downstream.

### 11. Style and naming
Do not emit style complaints (variable naming, function size, comment style)
as substantive findings. They belong in commit-message concerns
(`msg:style`) at most, and usually not at all.

### 12. Patch-series context
If the diff is patch N of a series and a concern is fixed in a later patch
of the same series, treat it as not-a-finding (or note the later-fix patch).
Intermediate patches may intentionally use a stub later replaced.

### 13. Fixed old-code bugs
Do NOT emit a concern merely because the removed/old code had a real bug.
If the reviewed diff moves, deletes, initializes, checks, or otherwise fixes
that old behavior, the old bug is evidence that the patch is a fix, not a
review finding. Only report it if the new/right-side code still has the bug,
the fix is incomplete, or the patch introduces a different bug.

## How to write a concern that survives consolidation

- Name the function, file, and line/region from the diff.
- Quote or paraphrase the specific code, not the general pattern.
- State the trigger condition that actually fires (not "if input is
  malformed").
- For locking/UAF/race, list the call chain or sequence you traced.

If you cannot do these for a concern, **do not emit it**. The consolidator
will not invent the evidence; it can only drop what you wrote.
