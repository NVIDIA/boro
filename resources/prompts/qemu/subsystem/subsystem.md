# Subsystem Guide Index (QEMU)

Load subsystem guides based on what the code touches. Each guide contains
QEMU-subsystem-specific invariants, API contracts, and common bug patterns.

The triggers column includes path names, function calls, and symbol regexes.
Err on the side of inclusion: only exclude a guide if it is clearly irrelevant.

## Subsystem Guides

| Subsystem | Triggers | File |
|-----------|----------|------|
| VirtIO | hw/virtio/, virtio_, vring_, virtqueue_, VirtQueueElement, vhost_ | virtio.md |
| Networking | net/, hw/net/, qemu_send_packet, NetClientState, eth_, virtio-net | net.md |
| Block layer | block/, hw/block/, bdrv_, blk_, BlockDriverState, BlockBackend, qcow2, .bdrv_co_ | block.md |
| SCSI | hw/scsi/, scsi_, SCSIRequest, scsi_req_, cdb | scsi.md |
| USB | hw/usb/, usb_, USBDevice, USBPacket, usb_packet_ | usb.md |
| PCI / PCIe | hw/pci/, pci_, PCIDevice, msi_, msix_, config_write, BAR | pci.md |
| VFIO passthrough | hw/vfio/, vfio_, VFIO_, VFIODevice, vfio_dma_map, region_info | vfio.md |
| ARM machines/SoC | hw/arm/, arm_load_kernel, sysbus_, memory_region_add_subregion, GIC, fdt/dtb | arm.md |
| ARM SMMUv3 / IOMMU | hw/arm/smmuv3*, hw/arm/smmu-common, smmuv3_translate, smmu_ptw, smmuv3_cmdq_consume, smmu_iotlb_inv_*, IOMMUMemoryRegion, cmdqv | smmuv3.md |
| Migration / VMState | migration/, vmstate_, VMStateDescription, VMSTATE_, save_vmstate, qemu_get_/qemu_put_ | migration.md |
| TCG / accel | tcg/, accel/tcg/, target/, gen_, tcg_gen_, translate, helper_, cpu_exec | tcg.md |
| KVM | accel/kvm/, kvm_, KVM_ | kvm.md |
| QAPI / QMP | qapi/, qmp_, hmp_, qobject_, visit_, QAPIEvent, *.json schema | qapi.md |
| Memory API | system/, softmmu/, memory_region_, MemoryRegion, address_space_, dma_, AddressSpace | memory.md |
| Device model (qdev/QOM) | hw/core/, qdev_, object_, TypeInfo, DeviceClass, *_realize, *_reset, property | qdev.md |
| Char devices | chardev/, qemu_chr_, Chardev, chr_be_ | chardev.md |
| UI / display | ui/, dpy_, DisplaySurface, console_, vnc_, gtk_, QemuConsole | ui.md |
| Locking / concurrency | bql_, qemu_mutex_, aio_, AioContext, qemu_coroutine_, qatomic_, rcu_, QEMUBH | locking.md |

## Optional Patterns

Load only when explicitly requested:

- **Memory API** (memory.md): also load whenever DMA or `address_space_*` is in
  the diff, even if the primary subsystem is a specific device.
- **Locking** (locking.md): always relevant when threads, coroutines, AIO, or
  bottom halves appear.
