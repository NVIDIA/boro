# Storage

`virtinst/storage.py` (`StoragePool`, `StorageVolume` XMLBuilders) and the
virt-manager storage-browse UI create and manage libvirt storage pools and
volumes for guest disks.

## Core invariants

- Volume creation derives format, capacity, and allocation; capacity/allocation
  must be parsed and converted (bytes vs KiB/MiB/GiB) correctly and bounded —
  an off-by-1024, or a size exceeding what libvirt/the backend accepts, produces
  a wrong-sized or failed volume.
- **Do not overwrite an existing path/volume unintentionally.** Creating a
  volume whose target collides with an existing file, or building a disk on a
  path that already exists, can destroy data — verify the existence check and
  the user's intent (clone vs create vs use-existing).
- Pool target paths and volume names come from the user; a name used as a path
  component must be sanity-checked (no traversal into another pool). For remote/
  network pools, don't assume local filesystem semantics.
- Default pool/format selection must be valid for the connection (a format the
  backend doesn't support fails at create time).

## Lifetime

- `StorageVolume.install()` creates the volume; failure must propagate and not
  leave a partial volume the caller thinks succeeded. Pool refresh after create
  keeps the cache accurate.

## Common findings

- Capacity/allocation unit-conversion bug, or a size the backend rejects.
- Creating over an existing volume/path without an existence check (data loss).
- Volume name/path not checked for traversal outside the pool.
- Create failure swallowed, leaving a partial/ghost volume.
