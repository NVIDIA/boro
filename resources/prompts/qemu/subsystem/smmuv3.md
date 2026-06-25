# ARM SMMUv3 / IOMMU (hw/arm/smmuv3*, hw/arm/smmu-common)

The SMMUv3 model (`hw/arm/smmuv3.c`, `hw/arm/smmu-common.c`,
`include/hw/arm/smmuv3.h`, `smmu-common.h`, `smmuv3-internal.h`) emulates an ARM
System MMU: it translates device DMA addresses through guest-owned page tables
and is driven by **guest-programmed command and event queues** and register
writes. Almost everything it consumes — queue contents, stream-table and
context-descriptor entries, page-table entries — is **guest-controlled**, so
this is a security-sensitive translation engine. Bugs here are OOB reads of
guest-pointed structures, wrong/again-translatable mappings, missing TLB
invalidation, and queue index errors.

## Translation path

- The IOMMU entry point is `smmuv3_translate()` (an `IOMMUMemoryRegionClass`
  `.translate`), which builds an `SMMUTransCfg` via `smmuv3_decode_config()` and
  returns an `IOMMUTLBEntry`. Faults are reported with `smmuv3_record_event()`.
- Config decode reads guest structures (stream table entry, context descriptor)
  from guest memory. **Validate every field, base address, and size before
  use** — a malicious/buggy guest STE/CD can point anywhere. `SMMUTransCfg`
  carries `stage`, `disabled`, `bypassed`, `aborted`; respect those states
  rather than translating unconditionally.
- Page-table walks: `smmu_ptw()` → `smmu_ptw_64_s1()` / `smmu_ptw_64_s2()`
  (`hw/arm/smmu-common.c`). The walk dereferences guest-provided table base
  addresses at each level. Each descriptor read must be bounds/permission
  checked; an unvalidated level/offset or output-address width (`oas`) is an OOB
  read of guest memory or a wrong translation. Stage-1, stage-2, and nested
  (S1+S2) all have distinct rules — don't conflate them.
- Permissions (`IOMMUAccessFlags`) and the access-fault path must match the
  descriptor bits; granting more than the PTE allows is a real bug.

## Command / event queues

- The command queue is consumed by `smmuv3_cmdq_consume()`; the queue lives in
  **guest memory** and the guest owns the producer index. The `SMMUQueue`
  (`base`/`prod`/`cons`/`entry_size`/`log2size`) plus the `Q_PROD`/`Q_CONS`/
  `Q_*_WRAP` macros (`smmuv3-internal.h`) implement a wrapping ring.
- Index/wrap handling is a classic bug surface: `cons`/`prod` and the wrap bit
  must be masked (`INDEX_MASK`/`WRAP_MASK`) so a guest-supplied index can't read
  a command entry outside the queue region. Verify each consumed command's type
  is range-checked (`SMMUCommandType`) and that malformed commands set the error
  field (`CMDQ_CONS.ERR`) rather than being acted on.
- Events are emitted with `smmuv3_record_event(SMMUv3State*, SMMUEventInfo*)`
  into the event queue; on queue-full / write-failure the model must set the
  overflow state, not write past the ring.

## TLB / IOTLB invalidation correctness

- The model caches translations (`SMMUTLBEntry`, `SMMUIOTLBKey`). Invalidation
  helpers: `smmu_iotlb_inv_all()`, `smmu_iotlb_inv_iova()`,
  `smmu_iotlb_inv_ipa()` (stage-2), `smmu_iotlb_inv_asid_vmid()`,
  `smmu_iotlb_inv_vmid()`. A `CMD_TLBI_*` that invalidates **less** than it
  should leaves stale translations the guest can exploit; invalidating by the
  wrong asid/vmid/range is equally wrong. Check that each TLBI command path maps
  to the correct invalidation scope.
- Downstream notifiers (`smmuv3_notify_iova()` →
  `memory_region_notify_iommu_one()`, gated by `smmuv3_notify_flag_changed()`)
  must fire on the same invalidations so mapped users (e.g. vhost, VFIO) stay
  coherent. A missing notify is a silent stale-mapping bug.

## Common findings

- Unvalidated STE/CD/PTE base or size from guest memory → OOB read or wrong map.
- Command/event queue index or wrap not masked → read/write outside the ring.
- Stage-1 vs stage-2 vs nested confusion; wrong `oas`/granule handling.
- TLBI scope too narrow (stale entry) or wrong asid/vmid; missing IOMMU notify.
- Translating while `disabled`/`bypassed`/`aborted` instead of honoring state.

## In development — command-queue virtualization (cmdqv / accel)

`hw/arm/smmuv3-accel.c` and `hw/arm/tegra241-cmdqv.c` add accelerated /
hardware-assisted command queues (the `cmdqv` SMMUv3 property, gated on
`accel=on`). **This code is not yet finalized upstream — do not assume specific
register names, structs, or function symbols are stable; read the current tree
(`read_files`/`git_show`) for the actual API before commenting.** The durable
review invariants that hold regardless of how the acceleration is wired:

- A hardware-accelerated, **guest-owned** command queue is still an untrusted
  trust boundary: queue base, size, and the producer index come from the guest
  and must be bounded against the allocated/backed region. Queue size limits
  (e.g. derived from the backend page size) must be enforced.
- Doorbell / `VCMDQ`-style register writes from the guest must be range-checked
  and validated before driving any host action.
- Reset must cover the accelerated queue state, and migration/VMState must
  either cover it or explicitly block migration while active.
- The accel path must stay consistent with the emulated SMMU state (e.g. CMDQV
  only valid with `accel=on`, viommu association enforced) — flag paths that can
  desync the two.
