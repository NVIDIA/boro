# Snapshots

`virtinst/snapshot.py` (`DomainSnapshot` XMLBuilder) and the virt-manager
snapshots UI create, list, revert, and delete libvirt domain snapshots (and,
where supported, checkpoints).

## Core invariants

- Snapshot XML must describe the snapshot kind correctly (internal vs external,
  disk-only vs full system, memory state). Generating a combination libvirt
  doesn't support (e.g. external memory snapshot on an unsupported config) fails
  at create time; verify the kind matches the guest's disks/state.
- **Revert and delete are destructive.** Reverting discards current state;
  deleting a snapshot with children may merge/remove data. The UI must confirm
  intent and operate on the snapshot the user selected — a wrong target is data
  loss. Verify the selected-vs-acted-on object.
- Snapshot names from the user must be validated; listing/lookup must handle a
  snapshot disappearing (deleted out from under the UI) without crashing.
- External-snapshot disk paths follow the same overwrite/existence concerns as
  storage (see storage.md).

## Common findings

- Generated snapshot XML kind inconsistent with the guest (rejected at create).
- Revert/delete acting on the wrong snapshot, or without a confirmation guard
  (data loss).
- Crash when a snapshot is missing/changed during list or revert.
- External snapshot disk path overwriting existing data.
