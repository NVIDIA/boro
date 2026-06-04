# Avoiding False Positives (QEMU)

Most rejected review comments are false positives. Before keeping a finding,
clear it against this guide. When in doubt and you cannot prove the path,
**drop it** — a wrong comment costs maintainer trust.

## Read enough context first

- Use `read_files` / `git_show` / `git_blame` to read the **whole** function and
  its callers, not just the diff hunk. Many "missing checks" are done one frame
  up.
- The diff shows a delta. The bug must be real in the *resulting* code, not in
  your mental model of the hunk in isolation.

## QEMU-specific non-bugs (do NOT report these)

- **`g_malloc`/`g_new` returning NULL**: they `abort()` on OOM by design. A
  missing NULL check after `g_malloc` is not a bug. (`g_try_malloc` *can*
  return NULL — that one must be checked.)
- **BQL assumptions**: device MMIO/PIO callbacks generally run under the BQL.
  "Missing lock" around state touched only from such callbacks is usually not a
  race. Verify whether a second context can touch it before claiming a race.
- **`Error **errp` may be NULL**: passing NULL is legal; `error_setg(NULL, ...)`
  is fine. Not every call needs to check `*errp`.
- **assert() on internal invariants**: an `assert()` guarding a condition that
  *cannot* be guest-influenced is correct defensive code, not a guest-triggerable
  abort. Only flag asserts reachable from untrusted input (guest, QMP, migration).
- **VMState version bumps**: adding a field with a proper `VMSTATE_*_V` /
  subsection and `needed` function is the *correct* way to extend migration —
  not a break.
- **`qemu_coroutine`/`bdrv_*` "blocking" calls**: inside a `coroutine_fn`, the
  `_co_` variants yield rather than block. That is correct, not an event-loop
  stall.
- **Deprecated-but-present code**: code kept for compat (old machine types,
  `hw_compat_*`) is intentional; don't propose deleting it as dead code.

## Reportable vs not

- Reportable: a concrete, reachable path where guest/QMP/migration input causes
  OOB, UAF, leak, abort, deadlock, or wrong behavior — with the chain shown.
- Not reportable: "could be cleaner", "might want a check" with no demonstrated
  trigger, or speculation about callers you didn't read.

## Calibrate to the changelog

If the commit message explains a trade-off or says a follow-up handles X, factor
that in. Don't report something the author already documented as intentional —
unless the code contradicts the message (that mismatch *is* reportable).
