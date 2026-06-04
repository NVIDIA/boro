# Severity Levels (QEMU)

Assign a severity to each finding. Take this seriously. Don't inflate. Use
Medium as the default and move up or down based on the "Question to ask".

## Critical
- **Definition**: Host memory corruption or a guest-to-host escape surface.
- **Question to ask**: Can a malicious guest use this to corrupt or read host
  memory, or execute host code? If yes, it's critical.
- **Examples**:
    - Guest-controlled OOB read/write in a device model or DMA path.
    - Use-after-free / double-free reachable from guest I/O or migration load.
    - Heap overflow from an unvalidated length/offset in a hot device path.
    - Migration-stream parsing corruption (untrusted input).

## High
- **Definition**: Serious issues that crash QEMU or make a guest/feature
  unusable.
- **Question to ask**: Can QEMU abort/hang or a VM become unusable with
  non-trivial probability? If yes, it's high.
- **Examples**:
    - `abort()`/assertion reachable by guest action or QMP input.
    - Deadlock or event-loop stall (blocking call in a coroutine/under BQL).
    - Resource leak that grows unbounded (memory, fd, BlockBackend ref).
    - Migration incompatibility / data loss without versioning.
    - Logic error producing wrong device behavior or data corruption in-guest.

## Medium
- **Definition**: Recoverable issues or cold-path defects.
- **Examples**:
    - Leak on a rare error/teardown path.
    - Inefficient or incorrect locking with no demonstrated deadlock.
    - Commit message materially mismatching the code.
    - Non-critical functional or performance regression.
    - Missing `Error **errp` propagation that only degrades diagnostics.

## Low
- **Definition**: Style, naming, and cosmetic issues with no runtime effect.
- **Question to ask**: Is there any visible real-life effect? If no, it's low.
- **Examples**:
    - Typos in comments or trace events.
    - Formatting / coding-style deviations (checkpatch).
    - Confusing naming, missing documentation.
    - Unnecessary complexity, negligible perf differences.
