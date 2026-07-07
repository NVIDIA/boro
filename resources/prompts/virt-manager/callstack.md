# Execution-Flow and Call-Stack Verification (Virt-manager)

When you suspect a bug, **prove the reachable path** before reporting it. A
finding without a concrete call chain is usually a false positive.

## Build the chain explicitly

State the path from an entry point (a GUI signal, a CLI option, a libvirt event)
to the defect, naming each function:

```
vmmConnection._tick()            <- background polling thread
  -> vmmDomain.tick()            <- refreshes cached domain state (worker thread)
    -> self.emit("state-changed")  <- emit off the main thread; a handler that
                                      touches widgets must go via idle_emit (bug)
```

## Virt-manager entry points worth tracing back to

- **GUI signals**: Glade/`.ui` `connect`ed handlers (`on_*_clicked`,
  `_signal_*`) — these run on the main thread.
- **Background threads**: `vmmConnection` polling/`_tick`, `vmmAsyncJob` worker
  callbacks, and libvirt event callbacks — these are **not** the main thread.
- **CLI**: `virtinst/cli.py` `Parser*` classes → `virtinst` object setters →
  `XMLBuilder` → `Installer.start_install(guest)` / domain define.
- **libvirt events**: lifecycle/agent callbacks registered on the connection.

## Context questions to answer

- **Which thread am I on?** If the code path originates in polling / an async job
  / a libvirt event callback, any GTK widget access must go through
  `idle_add`/`idle_emit`. If it originates in a GUI signal handler, it's already
  on the main thread.
- **Can this value be None?** Trace whether a libvirt lookup, an optional XML
  node, or a dict `.get()` upstream can yield `None` before this use.
- **Does an exception escape?** If a `libvirtError`/`OSError` can be raised here,
  is it caught at a level that shows the user an error rather than crashing the
  app or aborting a multi-step operation half-done?
- **Is the target correct?** For a destructive action (delete/overwrite), is the
  object it operates on the one the user selected?

## Confirm, don't assume

- If the None/error check is in the caller or a base method, the "missing check"
  is not a bug — show it.
- If the code is reached only from a GUI signal handler, it's on the main thread
  — a "needs idle_add" claim is wrong; read the callers.
- Read to the end of the method (and any `finally`) before claiming a leaked
  reference or unhandled exception.
