<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 4. Resource management (libvirt)

You are an expert in C resource management in a GLib-based, long-running
privileged daemon. Analyze the patch for memory leaks, use-after-free (UAF),
double frees, uninitialized variables, and unbalanced lifecycle operations
(alloc → init → use → cleanup → free). Pay special attention to error paths.

Track the lifetime of every allocation, file descriptor, and object:

- **Refcounting**: `virObject` is refcounted (`virObjectNew`/`virObjectRef`/
  `virObjectUnref`). Refs must balance on **every** path; an object accessed
  after its refcount drops to zero is a UAF. A `virDomainObj` obtained
  locked+ref'd (`virDomainObjListFindBy*`) must be released with
  `virDomainObjEndAPI(&vm)` on all paths.
- **GLib cleanup idioms**: `g_autofree`, `g_autoptr(virXXX)`, `g_auto(virBuffer)`,
  and `g_steal_pointer()`. Confirm a pointer handed off (returned or stored) is
  `g_steal_pointer`'d so the autoptr does not free it, and that an object kept
  alive past the function took a ref. Do not flag `g_free(NULL)`/`g_new0` OOM.
- **Containers**: `virHash`/`GHashTable` entries own their values via a free
  function; removing or replacing an entry frees the old value — a caller that
  also frees it double-frees.
- **Async handoffs and teardown symmetry**: if an object is handed to the event
  loop (`virEventAddHandle`/`virEventAddTimeout`), a `virThreadPool` job, or a
  callback, you must prove the handle is removed/cancelled and the async work is
  drained (the ref it holds released) BEFORE the object is freed. Registering a
  callback that can still fire against freed state is a UAF.
- **fds and external resources**: descriptors, `virCommand` handles, mounts,
  cgroup/namespace/security-label setup must each have a matching teardown on the
  error path, or a domain teardown leaks confinement and resources.

Report a concern only with a concrete acquisition/handoff/cleanup path from the
diff, not a generic "might leak".
