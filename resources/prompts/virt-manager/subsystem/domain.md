# Domain / Objects

`virtManager/object/` holds the GUI-side wrappers around libvirt objects:
`vmmDomain`, `vmmStoragePool`, `vmmNetwork`, etc., all based on
`vmmLibvirtObject`. They cache the object's XML and expose it to the UI.

## Core invariants

- `vmmLibvirtObject` caches the backing XML and re-parses it on change. Code that
  reads domain config should use the cached/parsed accessors; forcing a fresh
  `XMLDesc()` on every access (especially from polling) is a performance bug,
  and reading stale cache after a change you made without invalidating it shows
  wrong data.
- A `vmmDomain` wraps a `libvirt.virDomain`. The underlying object can become
  invalid (domain undefined/migrated) — operations must handle `libvirtError`
  and the object disappearing rather than crashing.
- State-changing operations (start/stop/save/migrate/hotplug) should run via an
  async job (not block the UI) and refresh the cached XML afterward so the UI
  reflects the new state.
- Edits to a persistent domain go through `define` of the full XML; a partial
  edit that re-defines from stale cached XML can revert concurrent changes — use
  the proper modify path (e.g. `virt-xml`-style device update / `updateDeviceFlags`).

## Common findings

- Operating on a `vmmDomain` whose libvirt object is gone without catching
  `libvirtError`.
- Reading stale cached XML after a modification without invalidating the cache.
- Re-defining a domain from stale XML, dropping concurrent changes.
- Blocking state-change call on the main thread instead of an async job.
