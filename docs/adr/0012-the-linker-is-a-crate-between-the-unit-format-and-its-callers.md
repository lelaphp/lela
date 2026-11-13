---
status: accepted
date: 2026-10-07
supersedes: third-engine-design.md revision 33 (the linker in `php_unit::link`)
---

# The linker is a crate of its own, `php_link`, between the unit format and the crates that call it

ADR-0001 made `php_unit` the format leaf: the declaration model, op arrays and the opcode enum, a crate whose public surface is types and `dump()`. Revision 33 (#775) then put the linker, `zend_inheritance.c`'s checks, in `php_unit::link`, because the compiler's early bind and the engine's run-time link both call it and `php_unit` was the one crate below both. The linked class record (#2169) followed it there. By 2026-10-07 the linker's four files were 4,020 of the 5,944 lines `xtask budget` counted for `php_unit`, and the record was growing a table per ticket (#2171, #2172, #2192's prototypes, the records the compiler's early bind keeps). So one ceiling was measuring a stable format and a growing linker together. Mitchell ruled on #2193, 2026-10-07: the linker gets a crate of its own.

## Decision

- `php_link` holds the linker and the linked class record: `link`, `linked_class`, `LinkedClass` with its `Rows`, `Row` and `Case`, `Held`, `Linking` and the `ClassMember` set. Its public surface is `link.lock`.
- It depends on `php_unit` and `php_heap` only. `php_compile`, `php_engine` and `php_jit` call it; nothing in `php_unit` depends on it.
- `php_unit` goes back to ADR-0001's surface, the format alone, and `unit.lock` keeps only the format's items.
- Revision 33's reason still holds: there is one linker, and both callers run the same function. The function has moved; nothing about it has been copied.

## Considered options

- **Raise `php_unit`'s ceiling (to 7000).** Rejected: one ceiling would keep measuring two things, and a format change and a linker change would keep competing for the same room.
- **Hold `php_unit` at 6000 and pay for each new table by line-golf.** Rejected: every table of the record would be paid for by squeezing lines elsewhere; #2192 had 56 lines to fit in.
- **Put the linker in `php_engine` and have the compiler call it there.** Rejected for the reason ADR-0001 gives: the engine calls the compiler for `include` and `eval`, so the edge cannot run the other way.

## Consequences

- Each crate has a ceiling that measures one thing: `php_unit` the format (ceiling ruled on #2228), `php_link` the linker and its record (5500).
- A `use` of the linker reads `php_link::` and crosses the `link` seam, which `api-check` holds to `link.lock`.
- ADR-0001 still stands, and this ADR supplements it: its consequence that "reflection, the dumpers, the stub cross-check and the linker read one model" still holds, and that model is `php_unit`'s.
- *Amended 2026-11-13 (#4632):* `php_link` also depends on `php_macros`, for `#[derive(EngineState)]` alone, as #4544 ruled for `php_unit` and `php_ssa`. A macro emits paths and links nothing (ADR-0007), so the linker still links `php_unit` and `php_heap` and nothing else of the workspace.
