# Devices

`virtinst/devices/` holds the `Device*` XMLBuilder subclasses (`DeviceDisk`,
`DeviceInterface`, `DeviceController`, `DeviceHostdev`, `DeviceChannel`,
`DeviceGraphics`, etc.) that model each `<devices>` child in domain XML.

## Core invariants

- Each device's `XMLProperty` set must match the libvirt schema for that
  element, and `set_defaults(guest)` must pick values valid for the guest's
  arch/machine/os — a default that's wrong for some guest type produces XML
  libvirt rejects or a misconfigured device.
- **Address/target assignment**: controllers, PCI/USB/drive addresses, and disk
  target names (`vda`, `sda`) must be unique and consistent within a guest.
  Auto-assigning a duplicate target/address, or one invalid for the bus, is a
  common bug. Verify the assignment accounts for existing devices.
- `DeviceDisk` path handling: a disk source path/URL is user-supplied; the
  device must set `type` (file/block/network/dir) consistent with the source,
  and storage creation (if any) must not clobber an existing path
  unintentionally.
- `DeviceHostdev` (PCI/USB/mdev passthrough) selects a specific host device;
  the wrong match assigns the wrong device to the guest.

## Validation

- Device-level `validate()` runs before define; new constraints belong there.
  Don't assume the GUI validated it — `virt-install` constructs devices too.

## Common findings

- Default value invalid for the guest's arch/machine/bus.
- Duplicate or bus-invalid disk target / device address auto-assignment.
- `DeviceDisk` `type` inconsistent with the source (file vs block vs network).
- Hostdev matching the wrong host device.
- New device attribute not wired to round-trip (see xmlbuilder.md).
