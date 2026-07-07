<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 3. Execution flow verification (virt-manager)

You are a static analysis engine tracing execution flow in Python
(`virtinst` / `virtManager`). Carefully trace the control flow of the provided
patch. Exhaustively examine logic errors, incorrect loop conditions, unhandled
exception paths, missing checks, and off-by-one errors. Check every branch and
conditional. Pay particular attention to:

- **`None` and attribute access**: a lookup, `dict.get`, XML property, or API
  return that can be `None` used without a guard raises `AttributeError`/
  `TypeError`. Reading an attribute is fine; calling/subscripting/using it is
  where it breaks.
- **Exception paths**: does a `try/except` catch too broadly (masking a real
  error) or too narrowly (letting an expected `libvirt.libvirtError` escape as a
  traceback)? Does a `finally`/`with` actually run the cleanup on every path?

## Validation provenance and candidate substitution

For every value, object, device, guest, connection, or other candidate that is
accepted, saved, used, or returned:

1. Enumerate the exact predicates established for that specific candidate.
   Similarly named states (`active`, `running`, `persistent`, `valid`,
   `connected`) are not interchangeable.
2. Track the candidate's identity from each validation to the final use. A check
   applies only to the object that was checked unless the code proves the
   property transfers.
3. If a helper replaces a checked candidate with a related one (a sibling
   device, a cached lookup, a fallback, a second `dict`/list lookup), verify the
   replacement against every predicate required at the use site. Membership in
   the same list/dict proves membership only, not liveness/ownership/state.
4. Apply this to both immediate returns and values saved for later fallback. Do
   not let a property established for a loop variable silently transfer to a
   different returned value.
5. Construct a concrete witness when predicates differ: the checked object
   passes the stronger predicate while the substituted object satisfies only the
   weaker one.

Report a finding only with a concrete path and trigger, not "this could be None".
