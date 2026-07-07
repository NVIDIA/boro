# Event Loop

`src/util/virevent*` (the default poll-based loop, `virEventPoll`) drives all
asynchronous I/O: socket handles, timers, and the RPC client/server callbacks.
The loop runs on a single thread.

## Core invariants

- Handle and timeout registrations return an integer watch id; the id must be
  removed (`virEventRemoveHandle` / `virEventRemoveTimeout`) exactly once. The
  associated opaque data has a free callback (`virFreeCallback`) that runs when
  the watch is removed — freeing the data directly *and* via the callback is a
  double-free.
- Callbacks must not block: the loop is single-threaded, so a synchronous
  network/monitor/D-Bus call inside a handle/timeout callback stalls all I/O
  (keepalive timeouts, other clients). Flag blocking work on the loop thread.
- Removing a handle from inside its own callback is supported but the opaque data
  may be freed asynchronously — don't touch it after requesting removal.
- Re-arming a timer from its callback is fine; freeing the object the timer
  references while the timer can still fire is a UAF (remove the timer first).

## Reference lifetime

- An object referenced by a registered handle/timer must hold a ref for the
  lifetime of the watch; the free callback drops it. Registering a callback
  without taking a ref is a UAF when the owner goes away first.

## Common findings

- Watch id removed twice, or opaque data freed both directly and via the free
  callback (double-free).
- Blocking call inside an event-loop callback (stalls all I/O).
- Object freed while a timer/handle referencing it is still registered (UAF).
- Missing ref for the lifetime of a registered callback.
