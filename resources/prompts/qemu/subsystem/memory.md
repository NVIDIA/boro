# Memory API and DMA

QEMU's `MemoryRegion`/`AddressSpace` API models guest physical memory and MMIO.
DMA helpers move data between devices and guest memory. Mistakes here are a top
source of guest-to-host issues.

## Core invariants

- `MemoryRegionOps` callbacks receive a guest offset and access size. Bound the
  offset against the region size; honor the `size` parameter (don't assume 4
  bytes); respect `.valid`/`.impl` min/max access sizes.
- Device DMA must use the device's `AddressSpace` via `dma_memory_read/write`,
  `pci_dma_*`, or `dma_memory_map` — IOMMU-aware. `cpu_physical_memory_*` and
  `address_space_*` on `address_space_memory` bypass per-device translation and
  are usually wrong in device code.
- `dma_memory_map`/`address_space_map` can return a **shorter** region than
  asked or fail (NULL). Check the returned length and pointer. Every successful
  map needs a matching `dma_memory_unmap`/`address_space_unmap` with the correct
  access length and `access_len`, on all paths.
- Reads/writes can cross region boundaries; don't assume a single contiguous
  host pointer for a guest range.
- `memory_region_init_*` / `memory_region_add_subregion` must be balanced by
  the corresponding del/finalize; leaked regions and dangling subregions are
  bugs.

## Common findings

- MMIO callback indexing a register array with an unbounded offset.
- Map result (short/NULL) not checked, or unmap missing on error.
- DMA bypassing the IOMMU-aware AddressSpace.
- MemoryRegion lifecycle imbalance.
