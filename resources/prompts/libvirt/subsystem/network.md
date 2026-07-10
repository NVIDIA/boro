# Virtual Networks

`src/network/` implements the virtual network driver: bridges, NAT/forwarding,
dnsmasq, and firewall rules. `src/util/virnetdev*` provides the low-level
netdev/bridge/tap operations.

## Core invariants

- Network state changes (bridge create, iptables/nftables rules, dnsmasq spawn)
  must be **fully unwound on failure**. A half-applied network leaves stale
  bridges, firewall rules, or a dnsmasq process — `networkStartNetwork` failure
  must run the matching shutdown.
- Firewall rules are applied via `virFirewall`; rules added on start must be
  removed on stop, and a reload must not duplicate or orphan rules. With the
  nftables/iptables backends, verify the rollback set matches what was added.
- dnsmasq/radvd config is generated into files and the helper is spawned via
  `virCommand`; config values that come from the network XML (hostnames, MACs,
  IP ranges) must be validated/escaped, not concatenated blindly.
- Interface names are length-limited (`IFNAMSIZ`); generated names (vnetN, tap
  devices) must fit and be unique.

## Untrusted input

- Network XML (forward mode, IP ranges, DHCP host entries, port groups) is
  client-supplied — validate addresses/prefixes and counts before use.
- For `<forward mode='bridge'>` / macvtap, the referenced host device name comes
  from config; confirm it exists and is the expected type before operating.

## Common findings

- Firewall/bridge/dnsmasq setup not rolled back on a start failure (leak/stale
  state).
- DHCP host or DNS entry from XML written to a config file without validation.
- iptables/nftables rule added without a matching removal on teardown/reload.
- MAC/IP/range parsed but not range-checked.
