---
status: accepted
date: 2026-09-05
supersedes: third-engine-design.r2.md §4.1 (`ClassDecl` in `php_engine::api`; `Unit` in `php_compile`), §5.7 (opcode enum generated in the engine's build script)
---

# The unit format, including the opcode enum, is a leaf crate below both the compiler and the engine

Revision 2 described declarations twice: `api::ClassDecl` in `php_engine` for native classes and the class declarations inside `php_compile`'s `Unit` for user code, with `ops.toml` consumed by a compiler opcode table that could not import the engine's generated enum. The dependency rules leave the format no other home: `php_compile` cannot depend on `php_engine`, `php_macros` may not depend on `php_compile` (that would put `php_syntax` under every extension), and `php_heap` depends on nothing and must not know what a class declaration is. So `php_unit` holds the declaration model, op arrays and their tables, the constant-expression tree, and the `ops.toml`-generated opcode enum with its effects and disassembler; it depends only on `php_heap` for interned names and literal values. The compiler and the native macros produce its values, the engine's linker consumes them, and the engine's build script generates only dispatch over `php_unit::Opcode`.

## Considered options

- **Declarations only in a leaf crate, op arrays in the compiler.** Rejected: 8.5 constant expressions contain closures, so the tree must reference function bodies, and the opcode enum would still need a home below both crates.
- **Everything in `php_engine`, compiler depends on engine.** Rejected: the engine calls the compiler for `include` and `eval`; the edge runs the other way.
- **Two generated enums from one `ops.toml`.** Rejected: two definitions of one number set, kept equal by a check; the shape revision 2 already deleted for handler stubs.

## Consequences

- A future reader will ask why the engine does not own its own opcodes. This file is the answer.
- `unit.lock` joins the interface locks; the crate's public surface is types and `dump()`.
- Reflection, the dumpers, the stub cross-check and the linker read one model.
- *Amended 2026-11-11 (#4544):* `php_unit` also depends on `php_macros`, for `#[derive(EngineState)]` alone. A macro emits paths and links nothing (ADR-0007), so the unit format still links `php_heap` and nothing else of the workspace.
