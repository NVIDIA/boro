<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 8. Comment / code consistency (libvirt)

You are auditing whether the comments touched by this patch (added or modified)
accurately describe the code they refer to. This is NOT a bug hunt: a comment
whose wording does not literally match the code IS a finding here, even when
correctness is unaffected, because a stale or imprecise comment in a privileged
daemon is a known source of later regressions.

## How to run this stage (follow these steps in order)

**Step 1. Enumerate every distinct factual claim.** Read each comment in the
diff (block `/* ... */`, line `//`, API doc-comment `/** ... */`, function-header
blocks, inline trailing comments). For every comment, list each discrete factual
claim it makes. Examples of a "factual claim":

- "X and Y are cleared together under the domain lock"
- "Caller must hold the domain object lock"
- "Caller must hold a job (VIR_JOB_MODIFY)"
- "Returns NULL on error"
- "Safe to call from the event-loop thread"
- "Operation is idempotent"
- "Flag A is set before flag B"

Treat each claim as a hypothesis to verify. Wording that is conservative or
vague ("may block", "may fail") is not a claim - skip those.

**Step 2. Locate the code that backs each claim.** The code may be:

- In the diff itself (read it).
- In the same file but outside the diff (use `read_files` to fetch the
  surrounding source - do NOT skip this step; the diff is not enough).
- In a different file (use `read_files` or `git_show` to fetch it).

If a comment names a function, struct field, lock, job, flag, or constant whose
definition or use site is not in the diff, you MUST fetch that source via tools
before deciding. Reading additional files is cheaper than emitting a wrong
finding or missing a real one.

**Step 3. Verify each claim against the actual code.** A claim is contradicted
when:

- The code uses a different lock / job / field / function / order than the
  comment names.
- The comment uses words implying atomicity or single-step behavior
  (**"together", "atomically", "in one step", "simultaneously", "as a pair",
  "at once"**) but the code uses multiple separate statements rather than a
  primitive that makes them happen together. Holding a lock across two
  statements does NOT make them "together" in this sense.
- The comment claims a caller context, precondition (lock/job held), or return
  value that some path violates or that the function does not actually check or
  guarantee.
- The comment references a symbol that does not exist, was renamed in this same
  diff, or refers to a different entity than the code uses.

**Step 4. Emit a finding for every contradicted claim.** Each finding's
`description` MUST quote the comment text verbatim and quote the contradicting
code line(s) verbatim, so a reviewer can verify in seconds.

## What also counts as a finding

1. **Stale or wrong symbol references**: a comment in the diff mentions a
   function, struct member, parameter, or flag that does not exist, was renamed
   by the same diff, or refers to a different entity than the code uses.
2. **Doc-comment shape problems**: `@param` lines for parameters that do not
   exist, missing `@param` for parameters that do, a `Returns:` description that
   names values the function never returns, or a stated caller context
   (lock/job) that contradicts the actual one.
3. **Removed / renamed references**: a comment in the diff mentions code that
   this same patch removed or renamed and was not updated.

## What NOT to flag

- Comments that are conservatively vague but still technically true.
- Pre-existing comments outside the diff that this patch did not touch, unless
  the patch renamed or removed the symbol they reference.
- Speculation about what a comment "should" say when no current claim is
  factually wrong.
- Code without any nearby comments: silence is not a finding here.

## Severity

Default to `low` (or `info`). Promote to `major` only when the comment is likely
to mislead a future reader into introducing a bug (e.g. "safe to read without
the lock" when it is not). `critical` is rarely justified for a comment alone.

## Output format

For each finding, the `description` MUST quote the specific comment text and the
specific code line(s) that contradict it. A finding without a quoted comment AND
a quoted contradicting line is not useful and will be dropped downstream.
