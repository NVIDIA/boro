# Networking

`virtinst/network.py` (the `Network` XMLBuilder) and the virt-manager host-
network UI define libvirt virtual networks; `DeviceInterface` (see devices.md)
attaches guests to them.

## Core invariants

- Network XML (forward mode, bridge name, IP/DHCP ranges, NAT settings) must be
  consistent: e.g. a DHCP range must fall within the configured subnet, and a
  `<forward mode='bridge'>` needs a bridge, not an IP block. Generating an
  inconsistent combination yields a network libvirt won't start.
- IP addresses, prefixes, and MACs are user input — parse and validate them
  (well-formed, in-range) before placing them in XML. An invalid address
  silently produces a broken network.
- Interface attachment (`DeviceInterface`): the referenced source network/bridge
  must exist; the model and MAC must be valid, and an auto-generated MAC must be
  unique and use the locally-administered libvirt prefix.

## Common findings

- DHCP range outside the subnet, or forward-mode/source mismatch in generated
  network XML.
- IP/prefix/MAC from user input not validated.
- Interface referencing a non-existent source network/bridge.
- Auto-generated MAC not unique or wrong prefix.
