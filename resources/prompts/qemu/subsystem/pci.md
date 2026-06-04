# PCI / PCIe

PCI devices expose config space and BARs to the guest. Config writes, BAR
accesses, and MSI/MSI-X table writes are all guest-controlled.

## Core invariants

- `config_write`/`config_read` callbacks receive guest values. Mask writes to
  writable bits; never let a guest write read-only or RsvdP fields into device
  state unchecked.
- BAR-backed `MemoryRegionOps` callbacks get guest offsets; bound the offset
  against the region size before indexing registers. Use the `size` argument;
  don't assume 4-byte accesses.
- MSI-X: vector numbers from the guest (`msix_notify`, table writes) must be
  range-checked against `msix_nr_vectors_allocated(dev)`. Out-of-range vector is
  a common OOB.
- DMA from a PCI device must go through `pci_dma_*` / the device's
  `AddressSpace` (IOMMU-aware), not `cpu_physical_memory_*`. Check return
  status; the mapping can be short or fail.

## Lifetime / hotplug

- Hot-unplug must tear down BARs, MSI-X, and DMA mappings and not leave dangling
  references in the bus or IOMMU.

## Common findings

- Register-array OOB from an unbounded BAR offset.
- MSI-X vector index OOB.
- Guest writing reserved/RO config bits into device logic.
- DMA bypassing the IOMMU-aware AddressSpace, or unchecked map length.
