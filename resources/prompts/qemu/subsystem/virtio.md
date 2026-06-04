# VirtIO

VirtIO devices process descriptors the **guest** places in virtqueues. Every
field read from a descriptor or the available/used rings is untrusted.

## Core invariants

- `virtqueue_pop()` returns a `VirtQueueElement` with guest-supplied `in_sg`/
  `out_sg` scatter-gather lists and counts. Validate sizes before use; a guest
  can supply zero, huge, or overlapping segments.
- Always `g_free(elem)` (or `virtqueue_detach_element`/`virtqueue_unpop`) on
  every path after `virtqueue_pop`, including errors — otherwise the element and
  its mappings leak.
- `iov_to_buf`/`iov_from_buf`/`qemu_iovec_*` bound the copy by the iov length;
  confirm the destination buffer is at least as large. Don't trust a guest
  header's length field as the buffer size.
- Use `virtio_ldl_p`/`virtio_stl_p` (endianness-aware) for multi-byte fields;
  raw access mishandles cross-endian and legacy/modern layouts.
- After consuming, `virtqueue_push()` then `virtio_notify()` with the **correct
  consumed length**; a wrong length corrupts the guest's view.

## Feature negotiation and migration

- Behavior must depend on negotiated features (`virtio_vdev_has_feature`), not
  assumptions. Reading/writing config space ignoring `VIRTIO_F_VERSION_1`
  (modern) vs legacy is a common bug.
- Device state added for migration needs a VMState field/subsection gated on the
  relevant feature; see migration.md.

## Common findings

- OOB / overflow from an unvalidated descriptor length or num-buffers.
- Leak of `VirtQueueElement` on an early-return error path.
- Missing check of `virtqueue_pop` returning NULL (empty queue).
- Re-entrancy: handling output that triggers config changes mid-loop.
- vhost: state split between QEMU and the vhost backend not kept consistent.
