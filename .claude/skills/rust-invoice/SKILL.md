---
name: rust-invoice
description: "Price a proposed Rust abstraction before committing to it — what it costs in compile time, binary size, permanent public API surface, and future freedom to change. Use when adding a generic parameter, a trait, a marker type, a builder, a callback field, `-> impl Trait`, `#[non_exhaustive]`, or a new layer of indirection; when asked 'is this worth abstracting', 'should this be generic or dyn', 'will this bloat anything', 'what does this cost us'; and in review when a diff widens a public signature. Returns an itemised bill and a verdict — it does NOT say no, it says what you are buying. Also use before claiming an abstraction is cheap."
---

# Rust Invoice

Six of eight essays in the corpus this came from argue the functional style in
Rust is free. Two print the invoice. **This skill is the second posture.**

It never answers "don't". It answers **"here is what you are buying, and here is
what you can no longer change."** Then the human decides.

## Output shape

Produce this, in this order. Skip a line only if it is genuinely zero — and say so.

```
ABSTRACTION:  <what is being added, in one line>
RUNTIME:      <free / measured cost / unknown + how to find out>
COMPILE TIME: <the real bill for most abstractions>
BINARY:       <usually not the problem — say so if it isn't>
API SURFACE:  <what becomes permanent>
FORECLOSES:   <what you can never do afterwards without a breaking change>
REVERSIBLE?:  <cheap to undo / breaking change / impossible>
VERDICT:      <rung 1/2/3 + the one-line reason>
```

## Where the money actually goes

**Compile time is the bill.** Not binary size — that is the reflexive worry and
it is usually the wrong one. A capability-marker tower measured **+37–78% front
end** for a **0.01%** byte delta. At 512 instantiations, a generic shim over an
`#[inline(never)]` monomorphic core cut text by **78.9%**; `-C codegen-units`
moved it by **zero bytes**, because CGU count does not change the number of
instantiations. `-Zpolymorphize` was deleted in 1.85, so this is permanent
architecture, not a pending fix.

- Find it with `cargo llvm-lines --release | head -20`. Two functions usually
  dominate.
- The fix is a **thin generic shim over an `#[inline(never)]` monomorphic core**.
  Note `#[inline]` on the inner is a **no-op**, not a backfire — it silently does
  nothing rather than making it worse.

**Erasure is not a size win.** `Box<dyn Fn>` measured bigger than the generic at
every body size tested, gap widening; 200 `&dyn` call sites produced 2× the text
of the generic version and compiled *slower*. Every closure still pays a
`call_mut` shim plus a vtable on top of its body. Erase for **API shape and
compile time at a seam**, never for size.

## The permanent-surface checklist

These are the items people forget to bill, and they are the expensive ones —
because they are paid by everyone downstream, forever.

| You add | You permanently owe |
|---|---|
| A generic parameter on a public fn | Turbofish in your users' code; inference failures they cannot fix |
| A public struct holding `F: Fn()` | The parameter is now API. It propagates to every holder of the struct |
| `-> impl Trait` without `+ Send`/`+ use<..>` | Whatever the body happens to satisfy. An `Rc` added to the body later is a **breaking change with a zero-line signature diff** |
| A trait method generic over anything | The trait can never be `dyn`. Decide now, not later |
| `#[non_exhaustive]` on a public enum | Your semver freedom, bought with your users' exhaustiveness |
| A marker **type parameter** | E0283 the first time one type could satisfy two capabilities — unfixable by whoever holds the error |
| `pub` fields on a validated type | The invariant becomes a comment |

**The four-line guard that converts a future support ticket into your own red
build** — add it whenever a public signature returns `impl Trait`:

```rust
const _: () = {
    fn assert_send<T: Send>(_: &T) {}
    fn probe() { assert_send(&public_api()); }
};
```

Without it, the error names *your public function* while the cause sits in a
private one, and your users find out before you do.

## Reversibility, which is the item most often mispriced

Ask it explicitly, because it usually dominates the decision:

- **Cheap to undo** — internal, no public signature touched. Price it at rung 1
  and move on; you are over-thinking.
- **Breaking change to undo** — anything in the permanent-surface table above.
  This is where the abstraction has to earn its keep against a named consumer.
- **Impossible to undo** — a trait that others now implement, or a type parameter
  users have turbofished. Treat as a one-way door.

> **A refactor with no consumer is speculation.** If you cannot name the concrete
> upcoming work the abstraction makes cheaper, the invoice has a cost column and
> an empty benefit column. File it and move on.

## When the runtime line is "unknown"

Say so, and give the command — do not guess. Guessing is exactly how the corpus
this came from ended up with folklore in it.

```sh
cargo llvm-lines --release | head -20                    # instantiation cost
size -m target/release/<bin> | grep __text               # real text, not `ls -l`
cargo rustc --release -- --emit asm                      # did it vectorise?
```

Before writing any performance figure into an invoice, check `rust-errata`.

## Related

- How far up the ladder to go at all → `rust-ladder`
- Whether the performance claim in your invoice is folklore → `rust-errata`
