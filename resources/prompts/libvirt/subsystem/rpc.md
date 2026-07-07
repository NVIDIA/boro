# RPC / Daemon Dispatch

`src/rpc/` is the XDR-based wire protocol and server framework; `src/remote/` and
`src/admin/` define the API dispatch. This is the network/socket boundary — all
decoded args are untrusted client input.

## Core invariants

- Messages are XDR-decoded into generated `*_args` structs. Variable-length
  arrays carry a separate `<name>.<name>_len` count; the `.x` files cap each
  array with a `<MAX>` constant. Dispatch code must respect those maxima and
  re-check counts before iterating/allocating — never trust a length to match
  the buffer.
- `virNetMessage` payloads are bounded by `VIR_NET_MESSAGE_MAX` /
  `*_LEGACY_PAYLOAD_MAX`; a new large payload type needs the right limit or it's
  a memory-exhaustion vector.
- Every generated dispatch entry runs an **ACL check** (`vir*EnsureACL`)
  produced by `gendispatch.pl` from the `@acl:` annotations. A new API without
  the correct ACL annotation is a privilege bug — verify the `.x`/`access`
  wiring, not just the C body.
- FD passing: `virNetMessage` can carry SCM_RIGHTS fds; counts and ownership of
  passed fds must be checked and the fds closed on every path.

## Server framework

- `virNetServer` + `virThreadPool` dispatch each request on a worker thread; the
  handler may run concurrently with others. Shared driver state needs its lock.
- Keepalive (`virKeepAlive`) and the client object (`virNetServerClient`) are
  refcounted; a callback registered on a client must hold a ref.

## Memory / cleanup

- Generated `xdr_free` / `vir*DefFree` must run for decoded args and the return
  struct on all paths; dispatch bodies typically `goto cleanup`. A new
  allocation in a dispatch body needs a matching free.

## Common findings

- Array count from the wire used without re-checking against its declared max.
- New public API missing or with an incorrect ACL annotation.
- Unbounded payload / allocation sized directly from a client-supplied length.
- Passed fd leaked, or fd count not validated.
- Decoded-arg or return-value leak on an error path.
