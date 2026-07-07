# Avoiding False Positives (Libvirt)

Most rejected review comments are false positives. Before keeping a finding,
clear it against this guide. When in doubt and you cannot prove the path,
**drop it** — a wrong comment costs maintainer trust.

## Read enough context first

- Use `read_files` / `git_show` / `git_blame` to read the **whole** function and
  its callers, not just the diff hunk. Many "missing checks" or "missing
  unlocks" are handled one frame up or by a `g_auto*`/`virDomainObjEndAPI`
  cleanup you can't see in the hunk.
- The diff shows a delta. The bug must be real in the *resulting* code.

## Libvirt-specific non-bugs (do NOT report these)

- **`g_new0` / `g_strdup` returning NULL**: they `abort()` on OOM by design. A
  missing NULL check after them is not a bug. (The `g_try_*` allocators *can*
  return NULL and must be checked — know which allocator is in use.)
- **`g_autofree` / `g_autoptr` / `g_auto(virBuffer)`**: these free automatically
  at scope exit. A "leak" on an early return where the owner is an autoptr is
  not a leak. Conversely, a value that is `g_steal_pointer`'d out is
  intentionally not freed here.
- **`virDomainObjEndAPI()`**: this both unlocks and unrefs. Code that looks up a
  domain and calls `virDomainObjEndAPI(&vm)` at the end is correctly balanced;
  don't claim a missing unlock or unref.
- **Lock dropped around a monitor call**: `qemuDomainObjEnterMonitor` /
  `...ExitMonitor` intentionally release the domain lock during the QMP round
  trip; that is the design, not a race — only flag it if state read *after*
  ExitMonitor assumes nothing changed and that assumption is actually unsafe.
- **`ignore_value(...)`**: a deliberately-unchecked return; not a bug.
- **Error returned as -1 with `virReportError` already called**: that is the
  contract. Don't ask for an error message that's set two lines up.
- **Driver code behind a capability/version check**: feature-gated paths
  (`virQEMUCapsGet`, version checks) are intentional compatibility handling.

## Reportable vs not

- Reportable: a concrete, reachable path where client RPC / XML / guest-agent /
  QMP input causes OOB, UAF, leak, NULL deref, deadlock, an ACL bypass, or wrong
  behavior — with the chain shown.
- Not reportable: "could be cleaner", "might want a check" with no demonstrated
  trigger, or speculation about callers you didn't read.

## Calibrate to the changelog

If the commit message explains a trade-off or says a follow-up handles X, factor
that in. Don't report something the author documented as intentional — unless
the code contradicts the message (that mismatch *is* reportable).
