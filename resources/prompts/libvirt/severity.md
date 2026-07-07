# Severity Levels (Libvirt)

Assign a severity to each finding. Take this seriously. Don't inflate. Use
Medium as the default and move up or down based on the "Question to ask".

## Critical
- **Definition**: Host compromise, privilege escalation, or memory corruption in
  the privileged daemon reachable from a less-privileged client or from guest-
  influenced input.
- **Question to ask**: Can a client (possibly unprivileged) or a guest, via
  RPC / XML / the guest agent / QMP, corrupt daemon memory, escalate privilege,
  or escape confinement? If yes, it's critical.
- **Examples**:
    - OOB read/write or UAF in the RPC/XDR or XML parsing path.
    - Missing ACL/polkit check on a new API that exposes a privileged operation.
    - Path/symlink (TOCTOU) bug in the root daemon letting a client touch
      arbitrary host files, or a security-label/cgroup escape.
    - Shell/command injection from unvalidated input into a spawned process.

## High
- **Definition**: Serious issues that crash the daemon, deadlock it, or break a
  domain/feature.
- **Question to ask**: Can the daemon abort/hang, or a VM/connection become
  unusable, with non-trivial probability? If yes, it's high.
- **Examples**:
    - NULL deref / assert reachable from client RPC or guest-agent input.
    - Deadlock from lock-ordering or a job held across a blocking monitor call.
    - Unbounded resource leak (memory, fd, virObject ref) on a repeatable path.
    - State desync between libvirt and the hypervisor causing data loss or a
      domain that can't be managed.

## Medium
- **Definition**: Recoverable issues or cold-path defects.
- **Examples**:
    - Leak or missing unref on a rare error/teardown path.
    - Incorrect or missing error reporting (returns -1 without `virReportError`,
      or reports then proceeds).
    - Commit message materially mismatching the code.
    - Migration/save-image compatibility issue without versioning.
    - Non-critical functional or performance regression.

## Low
- **Definition**: Style, naming, and cosmetic issues with no runtime effect.
- **Question to ask**: Is there any visible real-life effect? If no, it's low.
- **Examples**:
    - Typos in comments, logs, or error messages.
    - Coding-style deviations (syntax-check / cppcheck).
    - Missing `_()` translation wrapper, confusing naming, missing docs.
    - Unnecessary complexity, negligible perf differences.
