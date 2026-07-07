# Avoiding False Positives (Virt-manager)

Most rejected review comments are false positives. Before keeping a finding,
clear it against this guide. When in doubt and you cannot prove the path,
**drop it** — a wrong comment costs maintainer trust.

## Read enough context first

- Use `read_files` / `git_show` / `git_blame` to read the **whole** function/
  method and its callers, not just the diff hunk. Many "missing None checks" or
  "missing idle_add" are handled by a base class, a decorator, or one frame up.
- The diff shows a delta. The bug must be real in the *resulting* code.

## Virt-manager-specific non-bugs (do NOT report these)

- **`idle_add` already in the call chain**: virt-manager has helpers
  (`vmmGObject.idle_add`, `vmmGObject.idle_emit`) and patterns where the caller
  is already on the main thread. Only flag a thread-safety issue if you can show
  the code runs on a worker thread *and* touches widgets without marshalling.
- **GObject source cleanup**: `vmmGObject` tracks signal connections made
  through its connection helpers and timers made through `timeout_add()`.
  `idle_add()` is not tracked for cleanup, but its callback normally removes
  itself after returning `False`/`None`. Do not report a leak merely because a
  raw or wrapped GLib source is used. Trace whether it is self-removing,
  explicitly removed, or can remain active after its owning object is cleaned
  up. Keep a finding only when the surviving source has a concrete stale-object
  or retention consequence.
- **`XMLProperty` round-trips**: a new attribute wired via `XMLProperty` parses
  and formats through the builder; you don't need a manual getter/setter.
- **`except libvirt.libvirtError` then continue**: catching a libvirt error to
  show a dialog / fall back is the normal pattern, not swallowed.
- **CLI option not "validated"**: `virtinst/cli.py` defers most validation to
  libvirt when the domain is defined; a value passed through to XML that libvirt
  will reject is not necessarily a virt-install bug. Flag only when virt-manager
  itself would crash or silently mis-set it.
- **Test-only code**: changes under `tests/` follow different conventions; don't
  apply production-path expectations to fixtures and mocks.

## Reportable vs not

- Reportable: a concrete, reachable path where input or a libvirt return causes
  a traceback, a worker-thread UI access, a destructive action on the wrong
  target, a shell-injection, or invalid/dangerous generated XML — with the chain
  shown.
- Not reportable: "could be cleaner", "might want a check" with no demonstrated
  trigger, or speculation about callers you didn't read.

## Calibrate to the changelog

If the commit message explains a trade-off or says a follow-up handles X, factor
that in. Don't report something the author documented as intentional — unless
the code contradicts the message (that mismatch *is* reportable).
