---
status: accepted
date: 2026-09-26
supersedes: docs/adr/0002 decision "so `php_macros` depends on `php_engine`" and its consequence "Revision 3 §4.2 records the `php_macros → php_engine` edge", docs/adr/0004 consequence "The engine cannot re-export the attributes, because `php_macros` depends on it", third-engine-design.md §4.2's `php_engine ◄── php_macros` edge
---

# `php_macros` depends on no workspace crate, and `php_engine` depends on it, so Core declares its natives with the same attributes

ADR-0002 made every native entry take the engine's concrete `Request`, and it wrote down a consequence it did not need: "so `php_macros` depends on `php_engine`". A procedural macro emits paths as tokens. It never links the crate those paths resolve into, because the crate that expands it does. The edge would buy the macro nothing: the design of revision 34 derives every declaration fact from the Rust types, through `FromArgument` and `IntoReturn` in `api`, and the macro matches no type by name. The edge would also cost two things:

- **A second build of the engine for the host.** Cargo builds a proc-macro crate's dependencies for the host under the build-override profile, which would put `php_engine`, `php_compile`, `php_heap` and `php_syntax` on the host side before any extension can expand.
- **A cycle with Core.** Zend's own natives, classes and constants are the `Core` extension value the engine builds (design §7.1). Declaring them with the same attributes needs `php_engine → php_macros`, which is a cycle beside `php_macros → php_engine`. Without it, Core's natives stay the hand-written declarations and startup stub parser the rest of the port is deleting.

So `php_macros` depends on `syn`, `quote` and `proc-macro2` only, and `php_engine` depends on `php_macros`. The expansion names `::php_ext::abi::` and `::php_ext::__private::` paths in an extension, and `crate::api::` paths inside `php_engine`, chosen by `CARGO_CRATE_NAME` at expansion time. ADR-0002's decision stands: an entry takes `&mut Request`, and the table holds one concrete function type.

## Considered options

- **Keep ADR-0002's edge and take it.** Rejected: the host rebuild, and Core cannot use the macros.
- **Drop the edge and add nothing; Core stays hand-declared.** Rejected: Core's natives and classes (62 functions and 101 methods in php's own `Core`) keep a second way of declaring, beside the one principle 10 names, and the stub reader in `php_engine::natives` survives the port.
- **Keep the edge in the manifest as a permission never taken.** Rejected: a manifest edge nobody takes is a sentence that reads as a fact and is not one, and it still forbids the reverse edge Core needs.
- **A `crate = path` key on every attribute in `php_engine`.** Rejected: the crate being compiled is known at expansion, so the key would be written on every Core native to say what the compiler already knows.

## Consequences

- `php_macros` is the one crate below `php_engine` that `php_engine` depends on and that depends on nothing in the workspace. It stays replaceable without touching an extension body, as ADR-0002 wanted, and it can now be replaced without touching Core's either.
- The facade still has a reason to exist, and it is not the one ADR-0004's last consequence gave. The engine *could* re-export the attributes now. `php_ext` stays because an extension has exactly one workspace dependency and cannot name `php_engine` at all, ADR-0004's own first reason: the "api only" rule is crate visibility, not a lint over the bodies.
- A native declared in `php_engine`'s integration tests or doctests is not supported. There the crate name differs and there is no `php_ext` to resolve against, so the expansion fails to resolve at the site. Engine tests declare natives through the api directly.
- The path switch is pinned by a `php_macros` fixture that expands under `CARGO_CRATE_NAME=php_engine`, so renaming or splitting `php_engine` fails loudly.
- `seams.toml` changes with this ADR, and a person makes those edits: the `[[edge]] from = "php_macros"` row goes, a `php_engine → php_macros` edge is added, and `php_macros` leaves the `unit` seam's consumers.
- *Amended 2026-11-12 (#4621):* the root is chosen by `CARGO_CRATE_NAME` from one table in `php_macros` (`root_for`, and `state_trait_for` for `#[derive(EngineState)]`): a crate that reaches the engine directly names `::php_engine::api`, a crate below the engine and the facade names `php_heap`'s trait, and every other crate, an extension among them, names `::php_ext`. Each entry is pinned by a `php_macros` test beside the table, and a crate added to it is not a reason to amend this ADR.
