# QEMU Coding Style

Derived from QEMU's `docs/devel/style.rst`. QEMU style differs sharply from
Linux kernel style — do **not** apply kernel habits. Flag deviations as **Low**
severity (style/cosmetic) unless a style issue also causes a real bug (then use
the bug's severity). Be specific and quote the offending line; do not bikeshed.
`scripts/checkpatch.pl` is the authoritative checker — these are the rules it
and reviewers enforce.

## Whitespace and layout

- Indent with **4 spaces, never tabs** (existing tab-indented files and
  Makefiles excepted). No trailing whitespace. Files end with a newline.
- Aim for ~80 columns; checkpatch warns past 80.
- One statement per line.

## Braces

- Braces are **mandatory on every control block**, including single-statement
  `if`/`else`/`for`/`while`/`do`. This is the opposite of common kernel style —
  a brace-less `if (x) return;` is a QEMU style violation.
- Control statements: opening brace goes on the **same line**
  (`if (cond) {`).
- Function definitions: opening brace goes on **its own line**, immediately
  after the prototype.

## Naming

- Types (structs, enums, typedefs): `CamelCase` (e.g. `VirtIOBlock`,
  `BlockDriverState`). QEMU `typedef`s its structs.
- Scalar typedefs: lower case with a `_t` suffix.
- Functions and variables: `lower_case_with_underscores`.
- Macros and enum constants: `UPPER_CASE`.
- Wrappers around libc/GLib functions take a `qemu_` prefix; otherwise use the
  obvious subsystem-specific prefix for public (header-declared) functions.

## Conditionals

- Put the constant on the **right** in (in)equality tests: `if (a == 1)`, not
  Yoda style `if (1 == a)`.

## Types and literals

- Use the **right** type, not a habitual one. If you're reaching for `int` or
  `long`, there is usually a better choice.
- **Do not** use Linux-kernel internal types (`u32`, `__u32`, `__le32`, …) —
  this is a common mistake for kernel-trained authors. Use C standard types.
- Need a specific width → `int32_t`/`uint32_t`/`uint64_t` (`<stdint.h>`). Use
  `bool` / `true` / `false` for booleans, not `int` 0/1.
- Use QEMU's domain types where they apply: `hwaddr` (guest physical address),
  `ram_addr_t` (RAM offset), `vaddr` / `target_ulong` (CPU virtual address),
  `size_t`/`ssize_t` for host memory sizes.
- Use the matching format macros (`HWADDR_PRIx`, `RAM_ADDR_FMT`, `PRId64`, …)
  rather than guessing `%lx`.

## Includes / preprocessor

- `qemu/osdep.h` must be the **first** include in every `.c` file; it sets
  preprocessor macros that affect the core system headers. Never include
  `qemu/osdep.h` from a header.

## Comments

- Traditional `/* ... */` comments only. **No `//` line comments.**
- Multi-line comments align the leading `*`.

## Declarations

- Declare variables at the **start of a block**; mixing declarations and
  statements mid-block is discouraged.

## Memory and string APIs

- Allocate with the GLib wrappers: `g_malloc`/`g_malloc0`, `g_new`/`g_new0`,
  `g_free`, `g_strdup`. **Do not** use bare `malloc`/`free`/`strdup`.
- `g_malloc`/`g_new` abort on failure — do **not** add a NULL check after them.
  Use `g_try_malloc`/`g_try_new` only when failure must be handled, and then
  check the result. `g_free(NULL)` is fine.
- Match allocator to deallocator (`g_new`↔`g_free`,
  `qemu_memalign`↔`qemu_vfree`); mixing them is a bug, not just style.
- Prefer safe string helpers (`g_strdup_printf`, `pstrcpy`, `g_strlcpy`) over
  `sprintf`/`strcpy`/`strcat`.

## Error handling and APIs

- Functions that can fail in a user-visible way take `Error **errp` and follow
  the `ERRP_GUARD()` / `error_setg()` / `error_propagate()` contract (see the
  qapi guide and technical-patterns).
- `assert()` is for internal invariants only — never for validating
  guest/QMP/migration input.
- Single-statement function-like macros wrap their body in `do { ... } while (0)`.

## What to flag (and how)

- Quote the line and name the rule: "tab indentation here; QEMU uses 4 spaces",
  "single-statement `if` without braces", "`//` comment; QEMU uses `/* */`",
  "bare `malloc`; use `g_malloc`".
- Keep these to Low severity and brief. Don't pad the report with style points
  if there are substantive correctness/security findings — lead with those.
