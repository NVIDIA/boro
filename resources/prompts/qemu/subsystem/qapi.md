# QAPI / QMP

QAPI defines the QMP/HMP interface and its types via JSON schema, generating
visitors and marshalling code. QMP input is untrusted (a management client, but
still external input) and the schema is a stable ABI.

## Core invariants

- The QMP/QAPI schema is an ABI. Removing or renaming a command/field, or
  changing a type, breaks clients. New optional members must have defaults or be
  `'*'`-optional; gate incompatible changes behind feature flags / deprecation.
- `qmp_*` handlers take an `Error **errp`. Follow the contract: set `*errp` and
  return on failure; don't set an error and proceed. Validate all inputs before
  acting (a partial action then error leaves inconsistent state).
- Generated visitor code allocates; the corresponding `qapi_free_*` must run on
  all paths. Don't hand-free generated structs with `g_free`.
- `visit_type_*` on input can fail on malformed data — check before use.
- Events (`qapi_event_send_*`) must be emitted with correct, fully-populated
  data; missing required members trip asserts in the generated code.

## Common findings

- Schema change that breaks the QMP ABI without deprecation.
- `Error **errp` contract violation (set-and-continue, or never set).
- Leak of a generated QAPI struct on an error path.
- Input not validated before mutating state.
