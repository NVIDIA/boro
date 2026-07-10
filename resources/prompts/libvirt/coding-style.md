# Libvirt Coding Style

Derived from libvirt's `docs/coding-style.rst` and `docs/hacking`. Libvirt has
its own conventions enforced by `cppcheck`, `syntax-check` (the `make
syntax-check` rules under `build-aux/`), and `clang-format` for some files. Flag
deviations as **Low** severity (style/cosmetic) unless a style issue also causes
a real bug (then use the bug's severity). Be specific and quote the offending
line; do not bikeshed.

## Whitespace and layout

- Indent with **4 spaces, never tabs**. No trailing whitespace. Files end with a
  single newline.
- Lines should stay within ~80 columns.
- One statement per line; one declaration per line.

## Braces

- Braces are mandatory on multi-line bodies. For a **single-statement** body,
  libvirt omits the braces (`if (cond)\n    return -1;`) — the opposite of
  QEMU. Don't add braces around a one-line body, and don't drop them when any
  branch of the same `if/else` needs them.
- Opening brace of a control block goes on the **same line**; opening brace of a
  function definition goes on **its own line**.

## Naming

- Functions and types use a subsystem prefix in `lowerCamelCase` /
  `UpperCamelCase`: `virDomainObjListFindByUUID`, `qemuProcessStart`,
  `virStorageVolDef`. Public symbols are `vir...`; per-driver code uses its
  driver prefix (`qemu`, `lxc`, `virNetwork...`).
- Enums: `VIR_DOMAIN_FOO_BAR`; macros: `UPPER_CASE`.
- Typedefs are `CamelCase`, with a matching pointer typedef
  (`typedef struct _virX virX; typedef virX *virXPtr;` — newer code often uses
  `virX *` directly rather than the `Ptr` alias).

## Memory and strings

- Use GLib allocators: `g_new0`, `g_strdup`, `g_strdup_printf`, `g_free`. Use
  `g_autofree`, `g_autoptr()`, and `g_auto(virBuffer)` for scope-based cleanup;
  this is strongly preferred over manual `cleanup:` goto labels in new code.
- `g_new0`/`g_strdup` abort on OOM — do **not** add a NULL check after them.
- `VIR_ALLOC`/`VIR_STRDUP` have been removed; use `g_new0`/`g_strdup`. `VIR_FREE`
  still exists (it zeroes the pointer, unlike bare `g_free`), but new code prefers
  `g_autofree`/`g_clear_pointer`.
- Build strings with `virBuffer`, escaping with `virBufferEscapeString` /
  `virBufferEscapeShell`; don't hand-roll XML/shell escaping.

## Control flow and cleanup

- New code prefers `g_auto*` cleanup and early `return` over the historical
  `goto cleanup;` / `goto error;` pattern, but matching the surrounding file's
  existing style is acceptable.
- `ignore_value()` wraps a deliberately-unchecked return; a silently-dropped
  return value without it is flagged by syntax-check.

## Strings, printf, and gettext

- User-facing messages are wrapped in `_()` for translation; format strings must
  be string literals (syntax-check enforces this for `virReportError`).
- Use `%1$s`-style positional args in translated format strings (a libvirt
  syntax-check rule); a bare `%s` in a translated message is flagged.

## Includes / headers

- `<config.h>` is included first (via the build system); then system headers,
  then libvirt headers. `internal.h` provides the common macros.

## Comments

- `/* ... */` block comments. Keep them about intent, not narration.

## What to flag (and how)

- Quote the line and name the rule: "tab indentation; libvirt uses 4 spaces",
  "reintroducing the removed `VIR_ALLOC`; use `g_new0`", "format string not a literal",
  "missing `_()` on a user-facing message", "`%s` instead of positional
  `%1$s` in a translated string".
- Keep style points Low and brief. Lead with substantive correctness/security
  findings; don't pad the report with nits.
