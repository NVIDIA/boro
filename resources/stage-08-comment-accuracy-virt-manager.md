<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 8. Comment / code consistency (virt-manager)

You are auditing whether the comments and docstrings touched by this patch
(added or modified) accurately describe the code they refer to. This is NOT a
bug hunt: a comment whose wording does not literally match the code IS a finding
here, even when correctness is unaffected, because a stale comment is a known
source of later regressions.

## How to run this stage (follow these steps in order)

**Step 1. Enumerate every distinct factual claim.** Read each comment and
docstring in the diff (`#` lines, `""" ... """` docstrings, inline trailing
comments). For every one, list each discrete factual claim. Examples:

- "Callable only from the main thread"
- "Caller must hold an open connection"
- "Returns None if the domain is not running"
- "This runs in a worker thread"
- "Raises libvirt.libvirtError on failure"
- "value is always set by __init__"

Treat each claim as a hypothesis. Conservatively vague wording ("may fail") is
not a claim - skip those.

**Step 2. Locate the code that backs each claim.** The code may be:

- In the diff itself (read it).
- In the same file but outside the diff (use `read_files` to fetch it - do NOT
  skip this; the diff is not enough).
- In a different module (use `read_files` to fetch it).

If a comment names a function, attribute, signal, or constant whose definition or
use site is not in the diff, you MUST fetch that source before deciding.

**Step 3. Verify each claim against the actual code.** A claim is contradicted
when:

- The code uses a different attribute / method / signal / thread context than the
  comment names.
- The comment states a thread context ("main thread only", "runs in a thread")
  that the code violates or does not guarantee.
- The comment claims a precondition, return value, or raised exception that some
  path violates or that the function does not actually check or guarantee.
- The comment references a name that does not exist, was renamed in this same
  diff, or refers to a different entity than the code uses.

**Step 4. Emit a finding for every contradicted claim.** Each finding's
`description` MUST quote the comment text verbatim and quote the contradicting
code line(s) verbatim.

## What also counts as a finding

1. **Stale or wrong name references**: a comment/docstring mentions a function,
   attribute, parameter, or signal that does not exist, was renamed by the same
   diff, or refers to a different entity than the code uses.
2. **Docstring shape problems**: documented parameters that do not exist, missing
   documentation for parameters that do, a documented return/raise that the
   function never produces, or a stated thread context that contradicts reality.
3. **Removed / renamed references**: a comment mentions code this same patch
   removed or renamed and was not updated.

## What NOT to flag

- Comments that are conservatively vague but still technically true.
- Pre-existing comments outside the diff that this patch did not touch, unless
  the patch renamed or removed the name they reference.
- Speculation about what a comment "should" say when no current claim is wrong.
- Code without any nearby comments: silence is not a finding here.

## Severity

Default to `low` (or `info`). Promote to `major` only when the comment is likely
to mislead a future reader into introducing a bug (e.g. "safe to call from any
thread" when it must be main-thread only). `critical` is rarely justified for a
comment alone.

## Output format

For each finding, the `description` MUST quote the specific comment text and the
specific code line(s) that contradict it. A finding without a quoted comment AND
a quoted contradicting line is not useful and will be dropped downstream.
