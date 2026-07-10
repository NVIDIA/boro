# Execution-Flow and Call-Stack Verification (Libvirt)

When you suspect a bug, **prove the reachable path** before reporting it. A
finding without a concrete call chain is usually a false positive.

## Build the chain explicitly

State the path from a caller- or attacker-reachable entry point to the defect,
naming each function:

```
remoteDispatchDomainSetMemory()      <- client RPC, untrusted args
  -> virDomainSetMemory()            <- public API + ACL check
    -> qemuDomainSetMemoryFlags()    <- driver, takes domain job
      -> qemuDomainObjEnterMonitor() <- drops domain lock
        -> qemuMonitorSetBalloon(newmem) <- newmem not bounded
```

## Libvirt entry points worth tracing back to

- **RPC dispatch** → `remoteDispatch*` / `*Helper` (generated), then the
  `vir*` public API, then the driver method in the `virHypervisorDriver`
  table.
- **XML parsing** → `virDomain*DefParse*`, `virNetworkDefParse*`,
  `virStorageVolDefParse*`, `virXPath*` accessors.
- **Guest-agent / monitor input** → `qemuAgentCommand` /
  `qemuMonitorJSON*` reply handlers; treat the parsed JSON as untrusted.
- **Event loop callbacks** → `virEventAdd*` handles/timeouts, RPC keepalive.
- **Thread-pool jobs** → `virThreadPool` worker functions.

## Context questions to answer

- **Which locks are held, in what order?** Is the driver state lock held while
  taking a per-domain lock (or vice versa)? Crossing the established order is a
  deadlock. Is a domain job (`virDomainObjBeginJob`) held where required?
- **Is a lock dropped mid-sequence?** Around `EnterMonitor`/`ExitMonitor` the
  domain lock is released. A held domain job blocks concurrent destructive
  operations, so reusing `vm->def`/private state right after `ExitMonitor` is
  normally fine — only a concern when the path holds no job, or the specific
  state could actually change during the call and its reuse is unsafe (see
  locking.md).
- **Who owns the object?** Did this path take a ref (`virObjectRef`) it must
  release, and does `virDomainObjEndAPI` run on every exit?
- **Can a client trigger this concurrently** from two connections, racing on the
  same domain/object?

## Confirm, don't assume

- If the bound/NULL check is in the caller (or the generated ACL/arg-validation
  layer), the "missing check" is not a bug — show the caller.
- If a field is set and read under the same lock, there is no race.
- If an error path `goto`s cleanup or relies on `g_auto*`, read to the end of
  the function before claiming a leak or UAF.
