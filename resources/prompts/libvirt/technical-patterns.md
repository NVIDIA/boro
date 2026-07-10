# Libvirt Technical Review Patterns

Libvirt is a privileged C management daemon (the modular `virtqemud` /
`virtnetworkd` / ... daemons, or the legacy monolithic `libvirtd`) plus a client
library, built on GLib. It parses XML, speaks an XDR RPC protocol to clients,
and drives hypervisors (QEMU/KVM, LXC, etc.). Most of the correctness-and-
security weight sits in the **RPC/XML parsing surface** (untrusted client and
guest-agent input crossing into a root daemon) and the **object lifetime +
locking** machinery. Apply these patterns when reviewing a libvirt patch.

## Untrusted input crosses several boundaries

Treat the following as attacker-influenced, even inside a privileged daemon:

- **RPC messages** from clients (XDR-decoded `*_args`). A client may be
  unprivileged and reach the daemon through socket permissions + polkit.
- **Domain / network / storage / nwfilter XML** supplied by clients. Never
  assume it matches the RNG schema — validate every field you read.
- **The guest agent channel** (`qemu-ga` over virtio-serial) and **QMP monitor
  replies**: the guest can influence agent responses, so agent/monitor JSON is
  not trusted host state.
- Data read back from `/proc`, sysfs, or hypervisor logs.

For each: read a field once into a local, validate it (range, enum, length,
NULL), and only then use it. `virStrToLong_*` / `virStrToDouble` return -1 on
overflow/underflow — but they reject trailing garbage **only when called with a
NULL end pointer**. With a non-NULL `end_ptr` they succeed on `"10abc"` and
leave the suffix in `*end_ptr` for the caller to validate. So: check the return,
and when a non-NULL end pointer is used, also verify the suffix yourself before
trusting the value.

## Object lifetime and reference counting

- `virObject` is refcounted: `virObjectNew`/`virObjectRef`/`virObjectUnref` (and
  `virObjectLockableNew`). Refs must balance on **every** path; error paths are
  where leaks and use-after-free hide.
- `g_autoptr(virXXX)` / `g_autofree` / `g_steal_pointer()` are the modern
  cleanup idioms — confirm a pointer handed off (returned, stored in a list) is
  `g_steal_pointer`'d so the autoptr doesn't free it, and that an object kept
  alive after a function returns took a ref.
- A `virDomainObj` looked up via `virDomainObjListFindByUUID` returns a **locked,
  ref'd** object: it must be released with `virDomainObjEndAPI()` (which unlocks
  and unrefs) on all paths.
- Objects handed to the event loop, a thread-pool job, or an RPC callback must
  outlive the async work — verify the ref is held for the duration.

## Locking and the domain job model

See `subsystem/locking.md`. The recurring bug classes:

- Driver-state lock vs per-object lock ordering (always document/keep one order).
- Dropping the domain lock around a monitor call (`qemuDomainObjEnterMonitor` /
  `qemuDomainObjExitMonitor`) and then reusing state that could actually have
  changed — not every post-`ExitMonitor` access is a bug (the held job blocks
  concurrent destructive ops; updating `vm->def` there is normal).
- Missing `virDomainObjBeginJob()` / `virDomainObjEndJob()` around a sequence
  that must be serialized against other API calls on the same domain.

## Error-handling contract

- A libvirt function reports failure by returning `-1` (or NULL) **and** calling
  `virReportError`/`virReportSystemError` to set the per-thread last error.
  Returning -1 without an error, or reporting an error then returning success,
  is a bug.
- Don't overwrite an already-reported error on the cleanup path
  (`virErrorPreserveLast` / `virErrorRestore` when you must run cleanup that
  itself reports).
- `virReportSystemError` takes the positive `errno`; passing a libvirt -1 return
  in its place produces a garbage message.

## Memory, strings, and buffers (GLib era)

- Allocate with GLib: `g_new0`/`g_strdup`/`g_strdup_printf`; free with `g_free`
  (`g_free(NULL)` is fine). `g_new0` aborts on OOM — do **not** add a NULL check
  after it. `VIR_ALLOC` has been removed; new code uses GLib allocators
  (`VIR_FREE` still exists and zeroes the pointer).
- `virBuffer` builds strings incrementally (`virBufferAsprintf`,
  `virBufferAddLit`, `virBufferEscapeString`). Always emit XML/shell content
  through the escaping helpers, and remember the auto-cleanup
  (`g_auto(virBuffer)`). Note `virBuffer` has no error state (it holds only a
  `GString *` and indent): `virBufferCurrentContent` returns NULL only when the
  buffer pointer itself is NULL and an empty string for an empty buffer — do not
  flag a "missing error check" on it.
- `virCommand*` builds argv directly — there is no shell, so prefer it over
  string concatenation; never build a command line that gets handed to `sh -c`
  with interpolated untrusted data.

## Privileged-daemon filesystem and process pitfalls

- The daemon often runs as root: path handling is symlink/TOCTOU-sensitive.
  Prefer `virFileRewrite`/atomic-write helpers and openat-style checks over
  stat-then-open.
- Label/cgroup/namespace setup (`virSecurityManager*`, `virCgroup*`) must be
  undone on the teardown/error path or a VM teardown leaks confinement.

## Commit-message / API scrutiny

- Does the diff do what the message claims? Flag mismatches.
- New public RPC APIs must add an ACL/polkit check (the `gendispatch`/access
  layer) and bump `*.syms` + the protocol; a new wire field needs a version
  guard. A missing ACL check on a new API is a real security finding.
- "Fixes:" — is the referenced commit the real first-bad one, and is a tag
  missing for an obvious regression fix?
