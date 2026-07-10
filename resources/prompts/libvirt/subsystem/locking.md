# Libvirt Locking and Concurrency

The daemon is heavily multi-threaded: RPC worker threads (`virThreadPool`), the
event-loop thread, and per-driver worker threads all touch shared state. Getting
the locking model wrong causes deadlocks and data races. Audit against the
following.

## Object locks and lock ordering

- `virObjectLockable` objects are locked with `virObjectLock` /
  `virObjectUnlock` (RWLockable: `virObjectRWLockRead`/`Write`). A bare struct
  field touched from two threads without its lock is a data race.
- **Lock ordering deadlocks require an actual inverse lock edge.** The
  domain-list lock is held only briefly: `virDomainObjListFindBy*` takes the
  list lock, finds+locks the domain, then *drops the list lock* before
  returning the locked domain. Not every "lock B while holding A" is a bug: the
  driver's own mutex is a separate short critical section, and
  `virQEMUDriverGetConfig()` legitimately takes `driver->lock` while a domain is
  locked. Only flag an ordering deadlock when you can point to two concrete
  paths that take the *same* pair of locks in opposite order (A→B here, B→A
  there); a single acquisition edge is not proof.
- Don't call a function that re-locks an object you already hold locked
  (self-deadlock with a non-recursive mutex).

## The domain job model (QEMU/LXC drivers)

- `virDomainObjBeginJob()` / `virDomainObjEndJob()` serialize operations on one
  domain. A long-running operation must take the appropriate job
  (`VIR_JOB_MODIFY`, async jobs for migration/dump) so concurrent API calls on
  the same domain don't interleave.
- `qemuDomainObjEnterMonitor()` **drops the domain lock** for the QMP round trip
  and `qemuDomainObjExitMonitor()` re-takes it. The held job keeps another API
  from running a destructive operation on the domain meanwhile, so it is normal
  and correct for hotplug/other paths to update `vm->def` immediately after
  `qemuDomainObjExitMonitor()`. Do **not** demand a `virDomainObjIsActive(vm)`
  re-check after every monitor exit. Only flag stale-state use when you can show
  the cached value can actually be invalidated during the monitor call (e.g. the
  path holds no job, or the guest/monitor could have changed exactly the state
  being reused) and that reusing it is genuinely unsafe.
- A job must be ended on **every** path (including errors) or the domain wedges
  permanently; `virDomainObjEndAPI()` does not end a job.

## Condition variables and the event loop

- `virCondWait` must be in a loop re-checking its predicate (spurious wakeups,
  and the lock is dropped during the wait — state can change).
- The single event-loop thread runs `virEvent*` handle/timeout callbacks. A
  callback that blocks (synchronous monitor call, blocking I/O) stalls *all*
  event handling — flag blocking work on the event-loop thread.

## Reference lifetime under concurrency

- An object handed to a `virThreadPool` job, a timer, or an event callback must
  hold a ref for the lifetime of that async work; freeing it while a callback
  can still fire is a UAF. Cancel/remove the handle and drain before unref.

## What to flag

- A proven lock-order inversion: two concrete paths taking the same pair of
  locks in opposite order (not a lone driver-lock-under-domain-lock edge).
- Shared state read/written from multiple threads without the object lock or an
  atomic.
- State cached before `EnterMonitor` and reused after `ExitMonitor` where the
  path holds no job (or the specific state could have changed) and the reuse is
  demonstrably unsafe.
- A domain job started but not ended on some error path.
- Blocking calls on the event-loop thread.
