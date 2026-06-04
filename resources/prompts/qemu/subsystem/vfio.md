# VFIO Device Assignment (hw/vfio/)

`hw/vfio/` passes physical devices through to the guest via the host kernel's
VFIO framework (ioctls on `/dev/vfio`). It is **security- and lifetime-
critical**: QEMU brokers guest access to real hardware and host DMA, state is
split between QEMU, the host kernel, and the IOMMU, and many fds/refs must stay
balanced. Treat both guest input (config writes, BAR/MSI-X accesses) and the
kernel interface (ioctl results) carefully.

## Core invariants

- **ioctl results**: every `ioctl(VFIO_*)` return must be checked. A failed
  `VFIO_DEVICE_GET_REGION_INFO`, `VFIO_DEVICE_GET_IRQ_INFO`,
  `VFIO_IOMMU_MAP_DMA`/`UNMAP_DMA`, or `VFIO_DEVICE_SET_IRQS` left unhandled
  desyncs QEMU from the kernel/IOMMU and can leak host resources or mappings.
- **DMA map/unmap balance**: `vfio_container_dma_map` / `vfio_container_dma_unmap`
  (driven by the `MemoryListener` region add/del in `hw/vfio/listener.c`) must
  balance. A region added but not removed,
  or unmapped with the wrong iova/size, leaves host DMA mappings pinned or maps
  guest-controlled IOVAs incorrectly — a host memory-safety issue. IOVA + size
  arithmetic must not overflow.
- **Region / BAR handling**: region sizes and offsets come from the kernel
  (`region_info`) and accesses from the guest. Bound guest offsets against the
  region size before read/write; honor `mmap`-able vs trapped regions. Quirks
  (`vfio_pci` device-specific quirks) that synthesize register behavior must
  bound their own state.
- **Config space**: guest config-space writes are filtered/emulated before
  reaching the device. Writable-bit masks must be correct so the guest can't
  write through to host-only or capability fields it shouldn't control (BARs,
  MSI-X capability, ROM BAR, command register). MSI/MSI-X vector indices from
  the guest must be range-checked against the device's reported vector count.
- **Interrupts**: MSI/MSI-X setup via eventfds and `SET_IRQS` must keep the
  guest's vector table consistent with the kernel's; tearing down on mask/unmask
  and on device reset must release the eventfds.
- **Lifetime / hot-unplug**: fds (device, group, container), eventfds, and
  `object_ref`/`memory_region` references must all be released on
  `instance_finalize`/unplug. A dangling MemoryListener, eventfd handler, or DMA
  mapping after unplug is a UAF or leak. The container/group refcount must be
  dropped exactly once.
- **Migration**: assigned-device migration (where supported) goes through the
  VFIO migration region/state machine; state read back is untrusted and its
  sizes must be validated (see migration.md).

## Common findings

- Unchecked VFIO ioctl failure → QEMU/kernel/IOMMU desync.
- DMA map without matching unmap (pinned host pages / stale IOMMU entry), or
  iova/size overflow in the map call.
- Guest MMIO/BAR offset not bounded against region size.
- Config-space writable mask too permissive (guest writes host-only bits).
- MSI-X vector index from the guest not range-checked.
- fd / eventfd / ref / MemoryListener leak or dangling handler on hot-unplug.
