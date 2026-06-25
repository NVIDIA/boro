# KVM Acceleration

`accel/kvm/` and per-target KVM code bridge QEMU and the in-kernel hypervisor
via ioctls. State is split between QEMU and the kernel and must stay coherent.

## Core invariants

- ioctl return values must be checked; a failed `KVM_*` ioctl left unhandled
  leaves QEMU and the kernel out of sync.
- Register/CPU state sync (`kvm_arch_get_registers`/`kvm_arch_put_registers`)
  must run at the right points; reading stale state or skipping a put loses
  guest state.
- `kvm_run`/`exit_reason` handling: every MMIO/IO/hypercall exit must be fully
  serviced before re-entering; an unhandled exit reason is a bug.
- Capability checks (`kvm_check_extension`, `kvm_has_*`) must gate use of
  optional KVM features; assuming a capability without checking breaks on older
  kernels.
- Memory slots are managed internally by the KVM MemoryListener as
  MemoryRegions are added/removed (the `kvm_set_user_memory_region` ioctl is a
  private helper, not a public API). Verify slot consistency is preserved across
  region add/remove/resize; overlapping or leaked slots are bugs. Public helpers
  like `kvm_get_max_memslots()` / `kvm_get_free_memslots()` report capacity.

## Common findings

- Unchecked ioctl failure.
- Missing capability gate for an optional feature.
- Register sync omitted on a state-changing path.
- Memory-slot leak or inconsistency on region remove/resize.
