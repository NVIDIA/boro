# Severity Levels (Virt-manager)

Assign a severity to each finding. Take this seriously. Don't inflate. Use
Medium as the default and move up or down based on the "Question to ask".
Virt-manager is a client app/library, so severity tracks user-facing impact and
the few real security surfaces (spawned commands, credentials, root system
connections), not memory safety.

## Critical
- **Definition**: Command/code injection, credential leakage, or generating XML/
  config that hands the guest or a remote more than intended.
- **Question to ask**: Can input (a name, path, URI, or remote-supplied value)
  reach a `shell=True`/spawned command, leak a stored password, or produce a
  guest config that breaks isolation? If yes, it's critical.
- **Examples**:
    - `subprocess` with `shell=True` interpolating a user/remote-supplied value.
    - A stored connection/VNC/SPICE password logged or written world-readable.
    - Generated domain XML granting host device / filesystem access not intended
      by the user.

## High
- **Definition**: Crash or data loss in a common path; an operation that
  silently does the wrong thing to a VM.
- **Question to ask**: Will a normal user hit a traceback/hang, or will a VM be
  misconfigured/damaged with non-trivial probability? If yes, it's high.
- **Examples**:
    - Unhandled exception (`AttributeError`/`libvirtError`) on a common action.
    - Touching GTK widgets from a worker thread (crash/UI corruption).
    - Destructive op (delete storage, overwrite disk) on the wrong target or
      without the expected guard.
    - Generated XML rejected by libvirt for a normal configuration, blocking VM
      creation.

## Medium
- **Definition**: Recoverable issues or cold-path defects.
- **Examples**:
    - Exception only on a rare/error path; leaked signal connection or object
      reference.
    - XML round-trip dropping a field; CLI option parsed but not applied.
    - Commit message materially mismatching the code.
    - Compatibility break in a `--option` default without a note.

## Low
- **Definition**: Style, naming, and cosmetic issues with no runtime effect.
- **Question to ask**: Is there any visible real-life effect? If no, it's low.
- **Examples**:
    - Typos in comments, logs, or labels.
    - PEP 8 / pylint style deviations.
    - Missing `_()` translation wrapper, confusing naming, missing docstring.
    - Unnecessary complexity, negligible perf differences.
