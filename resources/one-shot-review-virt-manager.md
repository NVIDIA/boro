<!-- SPDX-License-Identifier: Apache-2.0 -->

# Single-pass review (boro, virt-manager)

You are performing **one consolidated pass** over a virt-manager patch that must
cover the same dimensions as the multi-stage protocol, without running separate
model calls per stage. The codebase is Python: the `virtinst` library (domain
XML building, device models, install/guest logic, the `virt-install`/`virt-xml`
CLI) and the `virtManager` GTK GUI, both driving libvirt through `libvirt-python`.

1. **Intent / architecture** — CLI/API/GUI design, `XMLBuilder` model changes,
   maintainability, conceptual flaws.
2. **Commit message** — English spelling, grammar, syntax, and clarity (subject
   and body); misleading or incomplete changelog vs the diff; missing updates,
   semantic correctness.
3. **Execution flow** — branches, exception handling, early returns, off-by-one,
   and especially `None` handling (an attribute/lookup that can be `None` used
   without a guard raises `AttributeError`/`TypeError`). Track the identity of
   every validated value through its final use; a check on one object does not
   transfer to a different one.
4. **Resources** — Python is garbage-collected, so focus on: leaked or
   never-closed libvirt connections/streams and file handles, GObject/GTK signal
   handlers connected but never disconnected (callbacks firing on dead objects),
   reference cycles that pin a GObject/widget alive (CPython's GC already
   reclaims pure-Python cycles), and background jobs not cleaned up.
5. **Concurrency / GTK thread-safety** — GTK is **not** thread-safe: touching
   widgets or GObject state off the main thread is a bug. Background work runs
   via `vmmAsyncJob`/worker threads and must marshal UI updates back with
   `GLib.idle_add` (or equivalent). Flag widget/model access from a worker thread.
6. **Security / correctness of generated artifacts** — validate user/config
   input; generate valid, safe domain XML (proper escaping via the builder, no
   hand-concatenated XML) and safe commands (`virtinst` builds argv, never a
   `shell=True` string from untrusted data). No `eval`/`exec` on external input.
7. **Portability** — Python version compatibility, optional dependencies guarded
   with `try/except ImportError`, and correct `gi.require_version(...)` before
   `from gi.repository import ...`. This is not a compiled-language build audit.
8. **XML / CLI surface** — `XMLBuilder` parse↔format round-trips (a new property
   must both parse and re-serialize), CLI option wiring in `cli.py`, and
   `libvirt-python` error handling (`libvirt.libvirtError` caught where the API
   can raise).

Every finding must carry concrete proof appropriate to its issue type: the
relevant code facts, a reachable trigger or witness when applicable, the
violated invariant or direct contradiction, and the concrete failure or
user-visible defect (a traceback, a malformed XML, a hung/blocked UI, a wrong
command). Do not use "may", "might", "could", or "not guaranteed" as a
substitute for missing evidence.

Be skeptical of the commit message. Prefer reporting a suspected issue with
clear reasoning over silence.

Do not report the bug that the patch is fixing. A defect visible only in
removed/old code is not a finding when the new/right-side diff fixes it. Report
only if the new code still has the defect, the fix is incomplete, or the patch
introduces a different bug.

When the diff is documentation-only or trivial comment fixes, return an empty
`findings` array.
