# QEMU Technical Review Patterns

QEMU is a userspace C program that emulates whole machines and devices. Most of
the security-and-correctness weight sits in **device emulation** (untrusted
guest input crossing into host code) and the **block/migration/coroutine**
machinery. Apply these patterns when reviewing a QEMU patch.

## Guest-controlled input is untrusted

Anything a device model reads from guest memory, a register write, a virtqueue
descriptor, a DMA buffer, or a migration stream is **attacker-controlled**.
Treat it like `copy_from_user` in the kernel:

- Validate lengths and offsets *before* using them to index, allocate, or
  `memcpy`. Look for `addr + len` / `idx * elem` arithmetic that can overflow.
- A guest can change a value in shared memory *after* you validated it
  (TOCTOU). Read each field once into a local, then validate the local.
- Never trust a length field to match the actual buffer; bound it against the
  real region size (`dma_memory_read`/`address_space_read` return a status —
  check it).

## DMA and address-space access

- `dma_memory_map` / `address_space_map` can fail or map a *shorter* region than
  requested — they return a host pointer and update the `*plen` length output;
  check both. The access helpers (`pci_dma_read`/`write`, `dma_memory_read`/
  `write`) instead return a `MemTxResult` status (no partial-length output) —
  check it for failure.
- A mapped guest buffer (`dma_memory_map`) must be unmapped with
  `dma_memory_unmap` on **every** path, including errors. Passing the wrong
  access length to unmap corrupts dirty tracking.
- `cpu_physical_memory_*` bypasses the device's `AddressSpace` — usually wrong
  in device code; prefer the device's `dma_*` helpers.

## Reference counting and object lifetime

- `object_ref`/`object_unref`, `blk_ref`/`blk_unref`, `bdrv_ref`/`bdrv_unref`,
  `memory_region_ref` must balance on all paths. Error paths are where leaks and
  use-after-free hide.
- `qdev_realize` failure must undo partial construction. Check that
  `error_propagate`/`goto` cleanup doesn't double-free or leak.
- An object handed to a timer, bottom half, AIO callback, or coroutine must
  outlive the async work — verify the work is cancelled/drained before unref.

## Error handling (the `Error **errp` contract)

- A function taking `Error **errp` must set `*errp` on failure and **return a
  failure indication**, or set nothing on success. Setting an error *and*
  continuing is a bug.
- Use `ERRP_GUARD()` when you dereference `*errp` yourself. Don't
  `error_setg(errp, ...)` then also `error_propagate` the same error.
- A non-NULL `errp` may be `error_fatal`/`error_abort`: code after a failed call
  that set `errp` may never run — don't rely on it for cleanup.

## Integer and allocation patterns

- `g_malloc`/`g_new` abort on failure (no NULL check needed) but
  `g_malloc(n * size)` can overflow — use `g_new(T, n)` / `g_malloc_array`.
- Mixing `g_free` with `free`, or `g_new` with `qemu_memalign`/`qemu_vfree`, is
  a corruption bug. Match allocator to free.
- `qemu_memalign` / `qemu_vfree`, `qemu_blockalign` / `qemu_vfree` must pair.

## Commit-message / changelog scrutiny

- Does the diff actually do what the commit message claims? Flag mismatches.
- "Fixes:" tags: is the referenced commit the real first-bad one? Is a Fixes:
  tag missing for an obvious regression fix?
- A behavior or migration-format change without a compat property or versioned
  field is an ABI/migration break — call it out.
