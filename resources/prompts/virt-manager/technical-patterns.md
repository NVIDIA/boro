# Virt-manager Technical Review Patterns

Virt-manager is a **Python** project: the `virtinst` backend library (used by
`virt-install`, `virt-clone`, `virt-xml`) and the `virtManager` PyGObject/GTK
desktop GUI. It builds libvirt domain/network/storage XML and drives libvirt via
the `libvirt-python` bindings. Unlike a C daemon, the weight here is **Python
correctness, GTK thread-safety, libvirt object lifetime, and generating valid /
safe XML and commands** — not memory safety. Apply these patterns when reviewing
a virt-manager patch.

## Python correctness

- `None` handling: libvirt lookups, optional XML nodes, and dict `.get()` return
  `None`. Using the result without a check is an `AttributeError`/`TypeError` at
  runtime. Trace whether a value can be `None` on the path you're reviewing.
- Exceptions: libvirt calls raise `libvirt.libvirtError`; file/OS calls raise
  `OSError`. A bare `except:` swallows `KeyboardInterrupt`/`SystemExit` — prefer
  `except Exception`. Catching too broadly and continuing can hide real failures
  (e.g. a failed define reported as success).
- Don't mutate a list/dict while iterating it. Watch Python 3 specifics:
  `dict.keys()`/`map`/`filter` are views/iterators, integer division is `//`,
  and `str` vs `bytes` must not be mixed.
- Default argument values must not be mutable (`def f(x=[])`) — a classic shared-
  state bug.

## GTK / threading

- The GTK main loop runs on the main thread. **All widget updates must happen on
  the main thread.** libvirt events and polling run on background threads, so a
  callback that touches the UI must marshal via `self.idle_add(...)` /
  `GLib.idle_add(...)`. Touching widgets directly from a worker thread is a
  real, crash-prone bug.
- `vmmGObject` (virtManager/baseclass.py) subclasses must chain the base
  `__init__`. Its `connect()` / `connect_once()` / `connect_opt_out()` and
  `timeout_add()` helpers track their handles for cleanup.
- `vmmGObject.idle_add()` is a thin wrapper around `GLib.idle_add()`; it does
  not register the source ID for object cleanup. An idle callback normally
  removes itself after it returns `False`/`None`, so using `idle_add()` is not
  inherently a leak. Check whether the callback can remain pending until after
  object teardown, retain a dead object, or return `True` and repeat.
- Raw `GLib.timeout_add()` / `GLib.idle_add()` and signal connections are valid
  when their handle is explicitly removed or their lifetime is otherwise
  bounded. Report a bug only when you can show that the source or connection
  outlives its owner and can invoke stale state.
- Pay particular attention to handlers connected on a longer-lived emitter:
  they can retain the receiver and must be disconnected when the receiver is
  cleaned up.
- Don't block the main loop with a long synchronous libvirt/network call from a
  UI handler; it freezes the GUI. Such work belongs on a thread (or
  `vmmAsyncJob`).

## libvirt-python object lifetime

- libvirt objects (`virDomain`, `virConnect`, `virStoragePool`, ...) wrap a C
  object; keep a reference while you use them. The connection (`virConnect`)
  must outlive the objects obtained from it.
- The event loop must be registered (`libvirt.virEventRegister*` /
  virt-manager's poll) for event callbacks to fire; new event-driven code must
  ensure registration and deregister/close cleanly.
- Check return values: `lookupByName`/`lookupByUUID` raise on miss; many APIs
  return `-1`/`None` on failure. Don't assume success.

## XML generation (the XMLBuilder system)

- `virtinst` maps Python attributes to XML via `XMLProperty` descriptors on
  `XMLBuilder` subclasses. A new `<element>`/attribute needs the property wired
  with the correct XPath, and round-trips through parse→format must be stable.
- Values placed into XML must be the right type and validated (e.g. a path, a
  size in bytes, an enum from the allowed set). Generating XML that libvirt
  rejects, or that silently changes guest config, is the core bug class here.
- Don't hand-concatenate XML strings; go through the builder so escaping and
  structure are correct.

## Spawning processes and building commands

- `virtinst`/`virtManager` shell out (e.g. to detect images, run helpers).
  Build argv lists, not shell strings; never interpolate a user-supplied path or
  name into a `shell=True` command. A `subprocess` with `shell=True` over
  untrusted input is a command-injection finding.

## Commit-message scrutiny

- Does the diff do what the message claims? Flag mismatches.
- A change to generated XML or to a CLI option's behavior can break existing
  users/scripts — call out compatibility breaks (renamed/removed `--option`,
  changed default) that the message doesn't acknowledge.
