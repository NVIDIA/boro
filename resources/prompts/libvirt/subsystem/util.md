# Core Utilities

`src/util/` is the shared C library used by every driver: string/parse helpers,
`virBuffer`, `virHash`, `virJSON`, `virCommand`, `virFile`, event loop, etc. A
bug here is amplified across all callers.

## String and number parsing

- `virStrToLong_*` (e.g. `virStrToLong_ull`) / `virStrToDouble` return -1 on
  overflow/underflow and only set the out param on success — callers must check
  the return, not just read the out param. They reject trailing characters
  **only when the end pointer argument is NULL**; with a non-NULL end pointer
  they succeed and leave the suffix for the caller to validate.
- `vir*TypeFromString` / `VIR_ENUM_IMPL` return -1 for unknown strings; the
  enum→string direction asserts/returns NULL for out-of-range — a count mismatch
  between the enum and its `VIR_ENUM_IMPL` string array is a real bug
  (`VIR_ENUM_IMPL` embeds a `G_STATIC_ASSERT` that the array length equals the
  enum's `_LAST`, so a new enum value must also extend the array).

## virBuffer

- `virBufferAsprintf`/`virBufferAdd`/`virBufferEscapeString` accumulate into a
  growable buffer (`GString *` + indent — there is no error state).
  `virBufferCurrentContent` returns NULL only when the buffer pointer itself is
  NULL and an empty string for an empty buffer, so a "missing error check" on it
  is a false positive.
- Use `g_auto(virBuffer)` for cleanup; emit XML/shell via the escaping variants.

## virCommand

- `virCommand` builds an argv array and runs without a shell: pass args with
  `virCommandAddArg*`, never assemble a `sh -c` string from untrusted data.
- Set up fds/env/working dir explicitly; check `virCommandRun`/`virCommandWait`
  exit status. A non-zero child exit ignored is usually a bug.

## virHash / containers

- `virHash` stores ref'd or owned values with a free function; removing/replacing
  an entry frees the old value per that function — double-free if the caller
  also frees. `g_autoptr(GHashTable)` for GLib hashes.

## Files

- `virFileRewrite` / atomic-write helpers avoid torn writes; prefer them for
  state files. In the root daemon, watch for symlink/TOCTOU on paths derived
  from config.

## Common findings

- `virStrToLong_*` / `vir*TypeFromString` return unchecked (garbage value or OOB
  index).
- `VIR_ENUM_IMPL` string array out of sync with the enum (missing entry).
- `virCommand` child exit status ignored, or a shell string built from untrusted
  input.
- Hash value double-freed because both the hash free function and the caller
  free it.
