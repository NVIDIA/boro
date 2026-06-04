# Block Layer

The block layer is coroutine- and AioContext-based. `BlockDriverState` (BDS) is
the node graph; `BlockBackend` (BB) is the device-facing handle. Image-format
drivers (qcow2, vhdx, ...) parse **untrusted on-disk metadata**.

## Core invariants

- Image-format metadata (qcow2 L1/L2 tables, refcount tables, headers) is
  attacker-controlled when the image is untrusted. Validate every offset, size,
  cluster index, and count before use — overflow and OOB here is critical.
- I/O entry points are `coroutine_fn` (`bdrv_co_*`, `blk_co_*`) and may yield.
  Don't call blocking/synchronous variants from the event loop; don't hold a
  mutex across a yield. See locking.md.
- Reference counts: `bdrv_ref`/`bdrv_unref`, `blk_ref`/`blk_unref` must balance
  on all paths. A BDS still referenced by pending I/O must not be freed —
  use drained sections (`bdrv_drained_begin`/`end`) before graph changes.
- `BlockAIOCB` callbacks complete asynchronously; the request structure must
  live until the callback runs and be freed exactly once.

## Graph and context

- Graph manipulation must happen under the right lock / in a drained section.
  Moving a BDS between AioContexts requires the proper acquire/release (or the
  newer graph-lock model). Cross-context access without it is a race.

## Common findings

- OOB / overflow parsing image-format metadata (critical).
- Synchronous block call stalling the main loop.
- Missing drain before a graph change → UAF on in-flight I/O.
- Refcount imbalance on an error path (leak or premature free).
- Wrong byte/sector unit math (`BDRV_SECTOR_*`).
