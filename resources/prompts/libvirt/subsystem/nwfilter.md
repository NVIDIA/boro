# Network Filters

`src/nwfilter/` implements the network-filter subsystem: ebtables/iptables rule
generation, the filter binding lifecycle, and `learnIPAddress` (the IP-learning
thread that snoops DHCP/ARP to discover a guest's address).

## Core invariants

- Filter rules are generated from filter XML with variables substituted from the
  binding (MAC, IP, parameters). Substituted values are guest/client-influenced
  — they must be validated and escaped so they can't inject extra rule fields or
  shell content into the ebtables/iptables invocation.
- Rules instantiated for a domain interface must be torn down when the interface
  is unplugged or the domain stops; a leaked rule set is both a resource and a
  security issue (stale filtering).
- The IP-learning thread runs asynchronously and holds references to the binding;
  it must stop and release them before the binding is freed (UAF risk on a fast
  start/stop).
- Recursive filter references (filters including filters) must be depth-bounded
  to avoid infinite recursion / stack exhaustion from crafted XML.

## Common findings

- Filter variable (MAC/IP/param) used in a rule without validation/escaping.
- Rules not removed on interface unplug / domain stop.
- learnIPAddress thread outliving the binding it references (UAF).
- Unbounded recursion through nested filter references.
