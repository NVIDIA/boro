<!-- SPDX-License-Identifier: Apache-2.0 -->

# What NOT to flag — false-positive digest (virt-manager specialist stages)

This is a tight subset of `false-positive-guide.md` distilled for the specialist
stages. Apply these rules BEFORE you emit a concern, not after. If a concern
fails any applicable rule, drop it. The full guide still runs at consolidation;
your job here is to stop weak concerns at the source.

## Core principle

If you cannot point to **specific code in the diff** that proves the issue, do
not emit it. "Could happen," "might race," "should validate" → drop.

## Concrete rules

### 1. Defensive programming requests
Do NOT request `None`/type/bounds checks or input validation unless you can show
an actual code path in the diff that reaches the use without an intervening
check, AND the value can realistically take the bad form there. Generic "add a
None check for safety" with no reaching path: drop. Python raising a clean
`ValueError`/`libvirt.libvirtError` that a caller already handles is not a bug.

### 2. API misuse assumptions
Do NOT report "caller might pass None" or "this could be the wrong type" without
showing the actual calling path. Many virtinst methods document/assume a built
object or an open connection; check the callers before flagging.

### 3. Unverifiable claims from the commit message
The commit message is not evidence on its own. Verify the author's "this is safe
because Y" from the code. Do not flag only on a missing justification.

### 4. GTK thread-safety — trace the call context
Before flagging a "widget touched off the main thread" concern, confirm the code
actually runs on a worker thread. Code inside a normal signal handler, a
`GLib.idle_add` callback, or the main event loop IS on the main thread and may
touch widgets freely. Only flag when a `vmmAsyncJob`/thread callback reaches
widget/GObject state without marshaling back via `idle_add`.

### 5. Exceptions vs. crashes
A raised, caught exception is control flow, not a crash. Only flag an unhandled
exception when a realistic input reaches a raise that no caller in the diff (or
its documented callers) handles, producing a traceback or a broken UI state.

### 6. "Leaks" in a garbage-collected language
Python frees unreferenced objects, and its cyclic GC reclaims pure-Python
reference cycles. Do NOT flag a local that goes out of scope, or a plain
reference cycle, as a leak. Real leaks here are: a libvirt connection/stream or
file handle never closed, a GObject signal handler connected but never
disconnected so it fires on a stale object, or a reference cycle that pins a
GObject/widget alive. Show the retained reference or the missing
`disconnect`/`close`.

### 7. Races — show two concurrent paths
A race concern must name both paths and the contested state. "X could race with
Y" with no specific call sites is not a finding.

### 8. None / attribute access
Before flagging an `AttributeError`/`None` deref, check whether an earlier line
(or the calling convention) guarantees the value is set. A property that returns
a default, a value already checked with `if x is None: return`, or an object the
method requires to be built does not need another guard downstream.

### 9. Style and naming
Do not emit style complaints (naming, function size, comment style, f-string vs
%) as substantive findings. They belong in commit-message concerns (`msg:style`)
at most, and usually not at all.

### 10. Patch-series context
If the diff is patch N of a series and a concern is fixed in a later patch of the
same series, treat it as not-a-finding (or note the later-fix patch).

### 11. Fixed old-code bugs
Do NOT emit a concern merely because the removed/old code had a real bug. If the
reviewed diff fixes that behavior, the old bug is evidence the patch is a fix,
not a finding. Only report it if the new code still has the bug, the fix is
incomplete, or the patch introduces a different bug.

## How to write a concern that survives consolidation

- Name the function, file, and line/region from the diff.
- Quote or paraphrase the specific code, not the general pattern.
- State the trigger condition that actually fires (not "if input is malformed").
- For thread-safety/leak/race, name the thread context or the retained reference.

If you cannot do these for a concern, **do not emit it**. The consolidator will
not invent the evidence; it can only drop what you wrote.
