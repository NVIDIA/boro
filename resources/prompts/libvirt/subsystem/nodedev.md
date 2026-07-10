# Node Devices / mdev

`src/node_device/` enumerates host devices via udev and manages mediated devices
(mdev), SR-IOV VFs, and NPIV vHBAs. It bridges kernel device state into libvirt
objects.

## Core invariants

- udev callbacks deliver device add/remove asynchronously; the device object
  list must be updated under its lock, and an object removed while another
  thread holds a ref must stay valid until that ref drops (refcount, not
  free-on-remove).
- mdev create/destroy and SR-IOV `sriov_numvfs` writes manipulate sysfs files in
  the privileged daemon — validate the parent device, the requested type/UUID,
  and write through the expected sysfs path; don't construct sysfs paths from
  unvalidated names.
- Capability parsing (PCI/USB/SCSI/net details) reads sysfs values that can be
  absent or malformed; check each read and bound counts (e.g. number of
  capabilities, VF index).
- Device names/UUIDs from the client must be validated before use as a lookup
  key or path component.

## Common findings

- udev add/remove racing with a lookup: use-after-free if remove frees an object
  another thread is using.
- sysfs path built from an unvalidated device/parent name.
- Unchecked sysfs read or unbounded capability/VF count.
- mdev/VF created but not cleaned up on a later failure in the same operation.
