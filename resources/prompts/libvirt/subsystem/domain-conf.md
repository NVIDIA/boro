# Domain / Object XML Config

`src/conf/` parses and formats the XML for domains, networks, storage, etc. This
is the primary **untrusted-input parsing** surface: a client supplies XML and
the daemon turns it into in-memory `vir*Def` structs.

## Core invariants

- Parsing uses libxml2 + the `virXPath*` helpers (`virXPathString`,
  `virXPathInt`, `virXPathULongLong`, `virXPathNodeSet`). Every accessor can
  fail/return absent — check the return and handle a missing/empty node; don't
  deref a NULL node-set or use an uninitialized out param.
- Numeric attributes go through `virStrToLong_*` (e.g. `virStrToLong_ull`) /
  `virStrToDouble`, which return -1 on overflow/underflow and — only when called
  with a NULL end pointer — on trailing junk. Check the return, then range-check
  the value (e.g. vcpu counts, memory sizes, indexes) before storing; if a
  non-NULL end pointer is passed, the suffix must be validated separately.
- Enum string→value conversions (`vir*TypeFromString`, `VIR_ENUM_IMPL`) return
  -1 for unknown input; an unchecked -1 used as an array index is an OOB.
- Parse and Format must round-trip: a field added to `vir*Def` and its parser
  must also be formatted (and vice-versa) or save/restore and migration drop it.
- `virDomainDefValidate` / per-device `*Validate` callbacks enforce semantic
  constraints the schema can't; new constrainable fields should be validated
  there, not only at use sites.

## Memory and ownership

- `vir*DefFree` / `g_autoptr(virDomainDef)` must cover every field the parser
  allocates; a new field added to the struct + parser without a matching free is
  a leak. Arrays need both the element frees and the array free.
- On a mid-parse error, the partially-built def must be freed (autoptr handles
  this if used consistently).

## ABI stability

- `virDomainDefCheckABIStability` compares the persistent config against the
  live one across migration/restore. A new field that affects guest ABI must be
  included in that check or migration silently changes the guest.

## Common findings

- Unchecked `virXPath*` / `virStrToLong_*` / `vir*TypeFromString` return used
  directly (NULL deref, OOB index, garbage value).
- Field parsed but not formatted (or not freed) — round-trip / leak bug.
- Missing range validation on a client-supplied count/size/index.
- New ABI-affecting field omitted from the ABI-stability check.
