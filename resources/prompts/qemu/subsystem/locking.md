# QEMU Locking and Concurrency

QEMU is multi-threaded: vCPU threads, the main loop, iothreads, the block layer
(AioContext + coroutines), and worker thread pools. Getting the locking model
wrong causes deadlocks, event-loop stalls, and data races. Audit against the
following.

## The Big QEMU Lock (BQL)

- The BQL (`bql_lock()`/`bql_unlock()`, formerly `qemu_mutex_lock_iothread`)
  serializes most device emulation and global state.
- Device MMIO/PIO callbacks and most QMP handlers run **with the BQL held**.
  vCPU threads drop the BQL while executing guest code and re-take it for MMIO.
- `BQL_LOCK_GUARD()` takes it scoped. Check the BQL is held where required
  (`assert(bql_locked())`) and **not** taken recursively.
- Calling a function that itself takes the BQL while already holding it is a
  deadlock. Calling BQL-protected state from an iothread without the BQL is a
  race.

## AioContext and coroutines (block layer)

- Block I/O runs in an `AioContext`. A `coroutine_fn` may **yield**; the `_co_`
  variants (`bdrv_co_*`, `blk_co_*`) are the yielding forms.
- Calling a **blocking** `bdrv_*`/`blk_*` from a non-coroutine context inside
  the event loop stalls the whole loop — a hang. Flag synchronous block calls on
  hot paths.
- Do not access a `BlockDriverState` from the wrong `AioContext`. The old
  `aio_context_acquire()`/`release()` API was **removed** (2023) — current code
  relies on the graph lock (`GRAPH_RDLOCK`/`GRAPH_WRLOCK` annotations) and
  drained sections for safe cross-context access. Flag any reintroduced
  `aio_context_acquire`, and check drained/graph-lock discipline around graph
  changes.
- A coroutine must not hold a `QemuMutex` across a yield unless that's
  explicitly designed for — another coroutine can run and re-enter.

## Bottom halves, timers, and async work

- `qemu_bh_schedule` / `aio_bh_schedule_oneshot`, `QEMUTimer`, and AIO
  completion callbacks run later, in a specific context. An object referenced by
  pending async work must outlive it: cancel/delete the BH/timer (`timer_del`,
  `qemu_bh_delete`) and drain in-flight AIO **before** freeing.
- Re-arming a timer from its own callback is fine; freeing the timer's owner
  from elsewhere while it can still fire is a UAF.

## Atomics and RCU

- Use `qatomic_read`/`qatomic_set`/`qatomic_cmpxchg` for lock-free shared
  fields; a plain C access racing with another thread is a data race.
- QEMU has its own RCU (`rcu_read_lock`/`rcu_read_unlock`, `g_free_rcu`,
  `call_rcu`, `QLIST_*_RCU`). Pointers published via RCU must be read inside an
  RCU critical section; freeing must go through `call_rcu`/`g_free_rcu`, not a
  direct `g_free`, if readers may still hold it.

## Re-entrancy

- A guest can trigger a second MMIO access (even from another vCPU) while the
  first is in progress. State mutated mid-callback can be re-entered. QEMU added
  a generic re-entrancy guard (`MemReentrancyGuard`) for exactly this — check
  whether new device code needs it.

## What to flag

- Blocking/synchronous block-layer calls reachable from the main loop.
- Shared state touched from multiple threads/contexts without a lock or atomic.
- async work (BH/timer/AIO/coroutine) outliving the object it dereferences.
- BQL taken recursively, or required-but-missing.
