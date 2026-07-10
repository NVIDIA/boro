# Storage Pools and Volumes

`src/storage/` implements storage pools and volumes across backends (dir, fs,
logical/LVM, disk, iSCSI, RBD, gluster, SCSI, etc.). It runs in the privileged
daemon and manipulates host files, block devices, and external tools.

## Core invariants

- **Path safety in a root daemon**: volume target paths come from client XML.
  Guard against traversal (`../`), absolute-path escapes out of the pool, and
  symlink races (TOCTOU) when creating/deleting/opening volumes. Prefer the
  pool-relative checks and atomic operations; a stat-then-open on an
  attacker-influenced path is exploitable.
- Volume creation/clone/wipe/resize must validate the requested capacity and
  allocation against the backend and against integer overflow before use.
- External tools (`qemu-img`, `mkfs`, LVM/iSCSI utilities) are run via
  `virCommand` with argv arrays — never build a shell string from a volume name
  or path. Image format must be passed explicitly to `qemu-img` (don't let it
  probe an untrusted backing file's format).
- Backing-chain handling: a volume's backing file is attacker-influenced; don't
  follow an untrusted backing chain to an arbitrary host path, and validate
  format at each level.

## Locking / lifetime

- Pools and volumes are refcounted objects with their own locks; look-up +
  `virStoragePoolObjEndAPI` / unref must balance. Pool state (active, autostart)
  changes need the pool lock.

## Common findings

- Volume target/backing path not checked for traversal or symlink races.
- Capacity/allocation arithmetic overflow or missing bound.
- `qemu-img`/`mkfs` invoked with a probed (not explicit) format on untrusted
  input, or with an unescaped path in a shell context.
- Pool/volume object ref or lock not released on an error path.
- Leftover temp file / partial volume on a failed create.
