# TCG / Target Translation

TCG translates guest instructions to host code. Bugs here cause wrong guest
execution, host crashes, or subtle correctness issues. `target/<arch>/` holds
per-architecture decode and helpers.

## Core invariants

- Translation (`translate.c`, `gen_*`, `tcg_gen_*`) must match the architecture
  manual: operand sizes, sign-extension, flag effects, and exception/fault
  semantics. A wrong size or missing sign-extend is a real correctness bug.
- TCG temporaries have lifetimes. A temp used after it's freed, or a global vs
  local temp confusion, produces miscompilation. Check `tcg_temp_free*` pairing
  and that temps aren't live across a branch/label incorrectly.
- Memory accesses go through `MemOp` with explicit size/alignment/endianness.
  Wrong `MemOp` (size or endianness) corrupts loads/stores.
- Helpers (`helper_*`) run with specific GETPC()/exception expectations; raising
  an exception requires the correct PC restore (`cpu_restore_state`).
- End-of-translation-block conditions (branches, exceptions, single-step) must
  set `is_jmp`/exit the TB correctly, or execution runs off the block.

## Common findings

- Operand size / sign-extension mismatch vs the ISA.
- Missing exception or wrong fault address.
- TCG temp use-after-free or leaked temp.
- Wrong `MemOp` size/endianness on a load/store.
- TB not terminated on a control-flow-changing instruction.
