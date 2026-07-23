# Connection / Polling

`virtManager/connection.py` (`vmmConnection`) and `connmanager.py` own the
libvirt connection, the object cache (domains, pools, networks, nodedevs), and
the polling/event machinery that keeps the GUI's view in sync.

## Core invariants

- Polling (`_tick`) runs on a **background thread**. It updates the in-memory
  object cache and then signals the UI — any resulting widget update must be
  marshalled to the main thread (see threading.md). The tick must catch
  per-object errors so one bad object doesn't abort the whole refresh.
- The object cache is keyed by name/UUID/key; add/remove of cached objects must
  stay consistent with libvirt lifecycle events and the periodic poll. A stale
  cache entry (object removed in libvirt but kept in the cache, or vice-versa)
  surfaces as ghost or missing VMs in the UI.
- Connection open/close: `vmmConnection` may be remote (over SSH/TLS) and can
  drop; reconnect logic must reset state cleanly and not leave half-registered
  event callbacks or duplicated objects.
- Event registration: lifecycle/agent callbacks are registered on open and must
  be deregistered on close to avoid callbacks into a torn-down connection.

## Threading

- Don't perform blocking libvirt calls on the main thread from here; the poll
  thread and async jobs exist for that. A synchronous call on connect from the
  UI thread freezes the app, especially for slow remote connections.

## Common findings

- `_tick` updating widgets without `idle_add` (runs on poll thread).
- One object's error aborting the whole poll (no per-object try/except).
- Cache not updated to match a libvirt add/remove event (ghost/missing object).
- Event callbacks not deregistered / objects not cleared on connection close.
