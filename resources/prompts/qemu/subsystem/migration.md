# Migration / VMState

Migration serializes device state to a stream and reloads it on the
destination. The **incoming stream is untrusted** (it may come from a malicious
or corrupted source), and the format is an ABI that must stay compatible.

## Core invariants

- Load handlers (`VMStateField` `.get`, `SaveVMHandlers.load_state`,
  `qemu_get_*`) parse untrusted input. Validate every count, length, and index
  before allocating or indexing. A guest-sized array length from the stream is a
  classic OOB/overflow.
- `VMStateDescription` must match between save and load. Adding/removing/
  reordering fields without versioning (`.version_id`/`.minimum_version_id`) or
  a subsection breaks migration.
- New optional state belongs in a **subsection** with a `.needed` function, or a
  versioned field — so older/newer QEMU and compat machine types still migrate.
- `VMSTATE_*` macros must use the right type/size; a mismatch silently
  corrupts the stream (e.g. `VMSTATE_UINT32` for a `uint64_t`).
- Post-load (`.post_load`) must re-validate cross-field consistency; values that
  were consistent at save time can be tampered with in the stream.

## Compatibility

- Behavioral changes that affect migratable state need a compat property gated
  on `hw_compat_*`/machine version, or they break cross-version migration.

## Common findings

- Unvalidated length/count from the stream → OOB or huge allocation.
- Field added without versioning/subsection → migration break.
- `VMSTATE_*` type/size mismatch.
- Missing `.post_load` validation of guest-influenced invariants.
