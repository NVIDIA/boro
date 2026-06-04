# Device Model (qdev / QOM)

QOM is QEMU's object model; qdev builds devices on it. `hw/core/` plus every
device's `TypeInfo`/`DeviceClass` and `*_realize`/`*_reset`/property code.

## Core invariants

- `realize` must fully construct or fully fail. On error it must
  `error_setg(errp, ...)`, undo anything already created (regions, child
  objects, refs, fd), and return — leaving no half-initialized device.
- Object lifecycle: `object_new`/`object_ref` balanced by `object_unref`;
  children added with `object_property_add_child` are owned by the parent.
  `instance_finalize` must free what `instance_init`/`realize` allocated.
- Properties: input values (from `-device`, QMP) are validated in setters; an
  unchecked property can produce an invalid device. Static-property defaults vs
  runtime sets must be consistent.
- Reset must restore *all* guest-visible state to power-on values; forgotten
  fields cause subtle post-reset bugs and migration mismatches. New code should
  implement the **3-phase Resettable interface** (`ResettableClass` phases:
  `enter` / `hold` / `exit`) rather than the legacy single `DeviceClass` reset
  callback (`legacy_reset` is deprecated). Flag new devices that add a legacy
  reset instead of resettable phases.
- VMState for the device must cover the state that reset touches (see
  migration.md).

## Common findings

- Partial construction left behind on a `realize` error path (leak / UAF).
- Missing field in `reset`.
- Property value not validated.
- `instance_finalize` not freeing what was allocated, or double-free with the
  child-object ownership.
