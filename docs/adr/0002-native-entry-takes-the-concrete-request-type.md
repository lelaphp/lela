---
status: accepted
date: 2026-09-05
---

# A native entry function takes the engine's concrete request type, not a trait object

The functions `#[php_function]` and `#[php_method]` generate must name the request context, which lives in `php_engine`. They take `&mut php_engine::Request` directly, so `php_macros` depends on `php_engine` and the native face is a set of inherent method groups on the request type. The alternative, a `&mut dyn` facet trait defined below the engine so that entries are type-erased, is the shape of Mahout's `Reentrant`: sixty methods of which extensions used nineteen, a `r.ctx()` hop at 1,210 sites to reach state, and a vtable call per facet access on the hottest path natives have. Extensions are tested with the real request and the `Capture` sink, which is what Mahout's few real extension tests already do; a fake request is not a goal.

## Considered options

- **Trait object below the engine.** Rejected for the reasons above; it also reintroduces the dual-channel macro rule (which parameter types force the "may re-enter" context).
- **Generic entry functions.** Rejected: function pointers stored in a table cannot be generic; the table needs one concrete type.

## Consequences

- A reader will be tempted to "fix" this to a trait for testability. Do not; test through `Capture` and witnesses.
- Revision 3 §4.2 records the `php_macros → php_engine` edge; the macros expand to `api` items only, so the crate remains replaceable without touching extension bodies.
