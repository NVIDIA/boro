# QEMU Driver

`src/qemu/` is the largest libvirt driver: it starts/stops QEMU processes, speaks
the QMP monitor and guest agent, and handles migration, snapshots, and hotplug.

## Core invariants

- **Domain object lifetime**: a `virDomainObj` is obtained locked+ref'd
  (`qemuDomainObjFromDomain` / `virDomainObjListFindBy*`) and released with
  `virDomainObjEndAPI(&vm)` on all paths. Pair every lookup with an EndAPI.
- **Jobs**: modifying a domain requires `virDomainObjBeginJob()` with the right
  job type (`VIR_JOB_MODIFY`, etc.); end it with `virDomainObjEndJob()` on every
  path (see locking.md). Async jobs (migration, dump, snapshot) use the
  async-job APIs and must be cleaned up on failure.
- **Monitor calls**: only inside `qemuDomainObjEnterMonitor()` /
  `...ExitMonitor()`. The domain lock is dropped during the call, but a held
  domain job keeps concurrent destructive operations out, so updating
  `vm->def`/private state right after `ExitMonitor` is normal and correct. Do
  **not** demand a `virDomainObjIsActive(vm)` re-check after every monitor exit
  (see locking.md); flag stale-state use only when the path holds no job, or the
  specific cached value could actually change during the call and reusing it is
  demonstrably unsafe.
- **QMP and agent replies are untrusted parsed JSON**: the guest influences the
  agent, and a malformed/unexpected monitor reply must not crash the daemon.
  Check `qemuMonitorJSON*` return values and the shape of returned objects.

## Process start/stop

- `qemuProcessStart` builds the command line (`qemuBuildCommandLine`), sets up
  cgroups, namespaces, security labels, and host devices. Each setup step has a
  matching teardown in `qemuProcessStop`; a failure mid-start must unwind what
  was already done (labels, cgroup, fd, hostdev) or the host leaks confinement
  and resources.
- Capability-gated behavior uses `virQEMUCapsGet(qemuCaps, QEMU_CAPS_*)`; a new
  command-line option must be gated on the capability that introduced it.

## Common findings

- `virDomainObjEndAPI` or `virDomainObjEndJob` missing on an error path (wedged
  domain, leaked ref).
- State cached before `EnterMonitor` and reused after `ExitMonitor` where the
  path holds no job (or the specific value could have changed during the call)
  and the reuse is demonstrably unsafe.
- Hotplug/unplug not rolling back partial device setup on failure.
- Unchecked `qemuMonitorJSON*` / `qemuAgent*` return or unexpected reply shape.
- Migration cookie / NBD / TLS state not cleaned up on a failed migration.
- Command-line option emitted without a capability guard.
