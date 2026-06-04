# Networking

QEMU net devices move packets between guest NICs and host backends
(`NetClientState`). Packet contents and lengths are guest- or network-
controlled.

## Core invariants

- `qemu_send_packet`/`qemu_send_packet_async` and receive callbacks deal in
  buffers with explicit lengths. Validate frame/segment lengths before parsing
  headers (`eth_*`, IP/TCP/UDP offset math).
- Header-offset arithmetic (`eth_get_l2_hdr_length`, VLAN/IP option parsing) is
  a classic OOB source — bound every offset against the actual packet length.
- Offload paths (TSO/GSO/checksum, `net_rx_pkt`/`net_tx_pkt`) trust guest
  descriptors for segment sizes; check `num_buffers`, MTU, and per-segment
  bounds.
- `.can_receive` must accurately reflect whether the device can accept a packet;
  returning true then dropping/over-running is a bug.

## Lifetime

- `NetClientState` teardown (`qemu_del_net_client`/`qemu_del_nic`) must cancel
  pending async sends and not leave dangling pointers in peers.
- A NIC referencing a backend that's being removed must drop cleanly.

## Common findings

- OOB read parsing a short or malformed frame.
- Integer overflow in segment/length math on the TX/RX offload path.
- Leak or UAF when a peer/backend is detached mid-transfer.
- Endianness mistakes in on-wire header fields.
