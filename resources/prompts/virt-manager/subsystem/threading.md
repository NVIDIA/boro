# Threading and vmmGObject

Virt-manager's GUI is single-threaded GTK with background work on other threads:
`vmmConnection` polling, `vmmAsyncJob` workers, and libvirt event callbacks. The
base class `vmmGObject` (virtManager/baseclass.py) coordinates lifetime and
thread marshalling.

## Main-thread rule

- **GTK widgets may only be touched on the main thread.** Any code reached from
  polling (`vmmConnection._tick`), an async job worker, or a libvirt event
  callback must marshal widget updates through `self.idle_add(...)` /
  `self.idle_emit(...)` / `GLib.idle_add(...)`. A direct `widget.set_*()` from a
  worker thread is a crash/corruption bug.
- Conversely, code reached only from a GUI signal handler is already on the main
  thread — adding `idle_add` there is unnecessary (and reviewers shouldn't
  demand it).

## vmmGObject lifetime

- Subclasses must chain `vmmGObject.__init__`. Signal connections made via
  `connect()` / `connect_once()` / `connect_opt_out()` and timers made via
  `timeout_add()` record their handles and are released when the object is
  cleaned up. `idle_add()` is a thin wrapper around `GLib.idle_add()` and is
  *not* tracked, but an idle callback normally removes itself after returning
  `False`/`None`, so it is not inherently a leak. A handle whose lifetime is not
  bounded — a raw `GLib.timeout_add`, or a connection on a longer-lived object
  that outlives this one — can fire a callback on a destroyed object; flag it
  only when you can show the source/connection outlives its owner and touches
  stale state.
- `_cleanup()` must release references (child objects, libvirt objects, and any
  handles the wrappers don't cover) so the object can be garbage-collected; a
  retained reference keeps a whole window/connection alive.

## Async jobs

- `vmmAsyncJob` runs a callback on a thread and shows progress; the callback
  must not touch widgets directly, and its result/error is delivered back for
  the main thread to act on. Long/blocking libvirt or network calls belong in an
  async job, not a signal handler (which would freeze the UI).

## Common findings

- Widget access from `_tick`/polling/event-callback/worker thread without
  `idle_add` (crash).
- Blocking libvirt/network call directly in a GUI signal handler (UI freeze).
- An untracked handle (raw `GLib.timeout_add`, or a connection on a long-lived
  object not released in `_cleanup()`) that fires after teardown.
- `_cleanup()` not releasing a reference/signal added by the patch.
