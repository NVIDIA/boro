# Execution-Flow and Call-Stack Verification (QEMU)

When you suspect a bug, **prove the reachable path** before reporting it. A
finding without a concrete call chain is usually a false positive.

## Build the chain explicitly

State the path from an attacker- or caller-reachable entry point to the
defect, naming each function:

```
virtio_blk_handle_request()   <- guest kicks the virtqueue
  -> virtqueue_pop()          <- reads guest descriptor (untrusted len)
    -> iov_to_buf(... req->in_len)  <- len not bounded against buffer
```

## QEMU entry points worth tracing back to

- **MMIO/PIO register writes** → `MemoryRegionOps.write`/`.read` callbacks.
- **Virtqueue kicks** → `VirtIOHandleOutput` / `virtio_*_handle_*`.
- **DMA completion / AIO callbacks** → `BlockAIOCB`, `IOCanReadHandler`.
- **Migration load** → `VMStateDescription` `.get`/field load, `*_load` in
  `SaveVMHandlers`.
- **QMP/HMP commands** → `qmp_*` / `hmp_*`.
- **Timers / bottom halves / coroutines** → `QEMUTimer` cb, `QEMUBH`,
  `coroutine_fn`.

## Context questions to answer

- **Which lock is held?** Is this under the BQL (`bql_locked()`)? An `AioContext`?
  Calling a BQL-requiring function without it (or vice versa) is a real bug.
- **Coroutine vs non-coroutine?** A `coroutine_fn` may yield; a non-coroutine
  caller of a blocking `bdrv_*` will stall the event loop. Mixing them wrong is
  a hang/deadlock.
- **Can the guest trigger this repeatedly / concurrently?** Re-entrancy through
  a second vCPU or a nested MMIO write is a classic QEMU bug class.

## Confirm, don't assume

- If the length/bound is validated in the caller, the "missing check" is not a
  bug — show the caller. Use `read_files` / `git_show` to read the surrounding
  function before reporting.
- If a field is set under a lock and read under the same lock, there is no race.
- If an error path returns before the dangerous use, there is no UAF. Read to
  the function's end.
