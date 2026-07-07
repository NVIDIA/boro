# Host Device Assignment

`src/hypervisor/virhostdev*` plus `src/util/virpci`, `src/util/virusb`,
`src/util/virscsi`, and `src/util/virmdev` manage assigning host PCI/USB/SCSI/
mediated devices to guests (VFIO passthrough). This crosses the host/guest
isolation boundary and runs privileged.

## Core invariants

- **Ownership tracking**: `virHostdevManager` records which domain owns each
  assigned device. Prepare/reattach must be symmetric — a device detached from
  the host for a guest on start must be reattached (or marked free) on
  stop/failure, or it's leaked (unusable by host and other guests).
- PCI device reset and driver rebind (`vfio-pci`): a failed assignment mid-way
  must roll back the rebind/reset so the device returns to a sane state. Don't
  leave a device bound to vfio with no owner.
- IOMMU group handling: assigning a PCI function requires the whole IOMMU group
  be assignable; verify the group membership check is intact — assigning one
  function while another is host-bound is an isolation break.
- USB/SCSI device lookup by vendor:product / address is ambiguous; validate that
  exactly the intended device is matched before detaching it from the host.
- mdev UUIDs and PCI/USB addresses come from domain XML — validate before using
  as a sysfs path component.

## Common findings

- Device detached for a guest but not reattached on stop / start-failure (leak).
- Partial VFIO rebind/reset not rolled back on failure.
- IOMMU-group completeness check missing or bypassed (isolation break).
- Ambiguous USB/SCSI match detaching the wrong host device.
- Address/UUID from XML used in a sysfs path without validation.
