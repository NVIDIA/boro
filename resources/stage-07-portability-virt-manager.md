<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 7. Portability and dependency availability (virt-manager)

You are reviewing whether this patch remains valid across virt-manager's
supported Python runtimes and optional dependencies. This is a **Python
import/version and dependency audit** — there is no compiled-language build or
hardware audit.

## Dependency and version audit

Use the checked-out review tree as authoritative. Do not assume a symbol,
keyword argument, or module from a newer dependency exists.

For every newly referenced module, class, function, or keyword argument:

1. Confirm it exists in the codebase or in a dependency the project actually
   requires. Check imports at the top of the file and the project's declared
   minimums.
2. **GObject Introspection**: any `from gi.repository import X` must be preceded
   by the correct `gi.require_version("X", "...")` at a point that runs before
   the import. Using a `Gtk`/`Gdk`/`GLib`/`Libosinfo` API added in a newer
   version than the project requires will fail at runtime on supported systems.
3. **Optional dependencies**: features that depend on an optional module must
   guard the import (`try: import foo / except ImportError:`) and degrade
   gracefully; an unconditional import of an optional dependency breaks
   environments without it.
4. **Python-version compatibility**: flag use of syntax or stdlib APIs newer than
   the project's minimum supported Python (e.g. a `match` statement, `str.removeprefix`,
   or a new `typing`/`functools` feature) unless the minimum has been raised.
5. **libvirt-python API level**: a libvirt API/constant used here must exist in
   the minimum `libvirt-python` the project supports; a newer constant needs a
   guard or a bumped minimum.

Report a concern only with concrete evidence: name the symbol/version, the place
it is used unconditionally, and the supported configuration where it is absent
(e.g. "`gi.require_version('Gtk','4.0')` but the project targets GTK 3", or
"`import argcomplete` is unconditional but it is an optional dependency"). Do not
use "may"/"might" as a substitute for identifying the missing symbol and the
configuration that lacks it.
