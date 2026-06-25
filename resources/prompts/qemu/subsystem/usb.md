# USB

USB device emulation processes `USBPacket`s and control-transfer setup data from
the guest. Setup-packet fields (`bRequest`, `wValue`, `wIndex`, `wLength`) and
data-stage lengths are untrusted.

## Core invariants

- Control transfers: validate `wLength` and the requested descriptor/index
  against the actual descriptor table size before copying. A guest can request
  more than the buffer holds.
- `usb_packet_copy`/`usb_packet_map` bound by the packet iov; confirm the device
  buffer is large enough and the direction (IN/OUT) matches.
- Endpoint and interface numbers from the guest must be range-checked before
  indexing endpoint arrays.
- `USBPacket` may complete asynchronously (`USB_RET_ASYNC`); the packet and any
  mapped buffers must remain valid until completion, and be unmapped/cleaned on
  cancel (`usb_device_cancel_packet`).

## Common findings

- OOB read serving a descriptor request with an unchecked index/length.
- Endpoint index OOB.
- Async packet UAF when the transfer is cancelled or the device is detached.
- DMA/sg buffer leak on the error path.
