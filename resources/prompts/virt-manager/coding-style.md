# Virt-manager Coding Style

Virt-manager is Python. Style is enapsulated by `pycodestyle` (PEP 8) and
`pylint`, run via the test suite (`pytest`, the `test_dist`/lint targets). Flag
deviations as **Low** severity unless a style issue also causes a real bug (then
use the bug's severity). Be specific and quote the offending line; do not
bikeshed.

## Layout

- **4-space indentation, never tabs.** No trailing whitespace; files end with a
  newline.
- Follow PEP 8 line length as enforced by the repo's `pycodestyle` config; wrap
  long lines rather than disabling the check.
- Two blank lines between top-level defs/classes, one between methods.

## Naming

- `lower_snake_case` for functions, methods, variables; `UpperCamelCase` for
  classes (virt-manager GUI classes are prefixed `vmm`, e.g. `vmmDomain`,
  `vmmConnection`); `UPPER_SNAKE_CASE` for constants.
- "Private" attributes/methods use a leading underscore; don't reach into
  another object's `_private` members.

## Idioms

- Prefer explicit `is None` / `is not None` over truthiness when `0`/`""`/empty
  are valid values.
- Use context managers (`with open(...) as f:`) for files; don't leave file
  handles dangling.
- Use f-strings or `.format()` for interpolation; user-facing strings in the GUI
  are wrapped for translation with `_()` (gettext).
- `except Exception` (not bare `except:`); log via the module `log`
  (`from virtinst import log`) rather than `print`.

## Imports

- Standard library, then third-party (`gi`/GObject, `libvirt`), then local
  (`virtinst`, `virtManager`) groups. No unused imports (pylint flags them).
- GTK is imported through `gi` with explicit versions
  (`gi.require_version("Gtk", "3.0")`).

## What to flag (and how)

- Quote the line and name the rule: "tab indentation; project uses 4 spaces",
  "bare `except:`; use `except Exception`", "mutable default argument",
  "unused import", "missing `_()` on a user-facing GUI string".
- Keep style points Low and brief. Lead with substantive correctness findings;
  don't pad the report with nits.
