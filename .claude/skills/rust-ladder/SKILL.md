---
name: rust-ladder
description: "Decide HOW MUCH type-level machinery a Rust design deserves, and recognise when you are pushing against something Rust cannot express. Use when choosing between newtype/enum/trait/generic/dyn, modelling state or invariants, designing an error taxonomy, structuring an iterator pipeline, or asking 'how should I model X', 'is this over-engineered', 'should this be generic', 'can I enforce this at compile time'. ALSO use on the compiler errors that mean you have hit a wall rather than made a mistake: E0119 conflicting implementations, E0207 unconstrained parameter, E0283 type annotations needed, E0034 multiple applicable items. Answers with a rung (free / bring a reason / write down why) and, at the wall, with the escape or with 'stop, Rust cannot say this'."
---

# The Rust Ladder

Every functional-flavoured technique in Rust is the same move: **take a decision
that would have been made at runtime and make it a type.** The move is not free
and it does not always terminate. This skill decides how far up to go and where
to stop.

Answer with **a rung and a reason**, not a pattern name.

## Rung 1 — Free. Do it. No performance argument needed.

No review debate, no benchmark, no justification.

- **Errors as typed values**, with the taxonomy shaped by *what the caller must do*.
  The enum becomes a one-screen statement of your whole failure policy.
- **`Option`/`Result` over sentinels**, `?` over manual propagation. `?`'s implicit
  `From::from` is why one `impl From` absorbs an entire new error source.
- **Exhaustive `match`. Never `_ =>` over an enum you own** — use `A | B | C =>`.
  Exhaustiveness is a refactoring engine: add a variant, `cargo check` prints
  your worklist. `_ =>` spends that, permanently and silently.
- **Never `matches!` as a total predicate over a domain enum** — it supplies an
  implicit `_ => false`, so a new variant silently answers "no".
- **Iterator adapters over index arithmetic** where they read better. Adapters are
  free; the *consumer* decides your allocation profile.
- **Newtype over a bare primitive** when the primitive has a unit or an invariant.
- **RAII over manual cleanup.** Unwind skips straight-line `disarm()`/`release()`.

## Rung 2 — Worth it, with care. Bring a reason.

State the reason in the PR. If you cannot state it in one line, drop to rung 1.

- **Generic over a trait, instead of `dyn`** — at the leaves. Generic at the leaves,
  `dyn` at the seams. `dyn` outside a loop is free; inside it is not.
- **Type-state** for protocols with a genuinely fixed, small number of states.
- **`PhantomData` markers** for units, taint tracking, or capability tokens.
- **Const generics** for fixed-size kernels — often faster *and* more accurate
  than the dynamic version, not a trade.
- **Sealed traits** when you need the extension point closed.

Each of these lands in your **public API and stays there**. Before committing,
price it — that is what `rust-invoice` is for.

## Rung 3 — Usually a mistake. If you do it, write down why.

- **Marker-type towers to fake overlapping blanket impls.** Ships today and is
  sometimes genuinely the only option — but it is hand-rolled machinery in your
  public API forever, and your users' error messages get worse.
- **Persistent/immutable data structures** under a borrow checker. They cost more
  here than under a GC, and far more under a refcounted COW model.
- **Deep combinator chains carrying error context in the item type.** `?` cannot
  cross a closure boundary, so the context has to ride in the item — and it
  infects the item with a lifetime.
- **Encoding arithmetic in the type system.** You will meet the walls below.
- **Trait objects in a dispatch loop.** See `mahout-hot-path` if this is the engine.

## The three walls — where the move stops working

These are not skill issues. Recognise them and stop pushing.

**1. You cannot quantify over a type constructor.** No HKT. This is why "monad"
has four names in Rust (`and_then`, `flat_map`, `then`, `flatten`) and no shared
trait. If your design needs `M<_>` as a parameter, redesign — it is not coming.

**2. You cannot state an effect.** No effect system. `async` is the visible
consequence: colour exists because the effect is in the type and cannot be
abstracted over.

**3. You cannot state an obligation that must be discharged.** Rust has **affine**
types (use at most once), not **linear** ones (use exactly once). `Drop` is not
guaranteed to run — decided in 2015, and `Pin` is downstream of it. This one gap
is cancel-safety, io_uring, the absence of `AsyncDrop`, and `thread::scope`'s
closure. **You cannot say "this may not be freed until a third party says so."**

## At the wall: the compiler errors that mean "stop", and the escape

| Error | What it means | Escape |
|---|---|---|
| **E0119** conflicting implementations | Coherence is decided on the **self type**, never on the bounds. An associated-type *equality* bound that is provably disjoint to a human is still E0119. | **Make the thing you are dispatching on into a type.** Move the discriminator into the self type — a newtype, or an associated "kind" type. Specialisation will *not* save you: not the naive pair, not with an intersection impl, not under `min_specialization`. Open since 2016. |
| **E0283** type annotations needed | Your marker is a *type parameter* where it should be an *associated type* — inference has nothing to pin it to. | **Marker parameter** when the capabilities are exclusive forever; **associated type** the moment one type could satisfy both. Inference is free until it is ambiguous, and then it is unfixable by whoever is holding the error. |
| **E0207** unconstrained type parameter | You are trying to select an impl on something that does not appear in the trait or self type. | Same escape as E0119 — it has to appear in a position coherence can see. |
| **E0034** multiple applicable items | A facade and its capability trait share a method name. | Rename one. This error also makes an infinite-delegation bug invisible, so it is worth taking seriously rather than turbofishing past. |

E0119 in depth — why every specialisation escape fails, re-verified on nightly — is `docs/learning/rust-fp-async/articles/07-greyblake-blanket-impls-deepdive.md` in the Mahout tree; the repo-local worked example is `crates/mahout_native_abi/src/marshal.rs:115-118`, where `FromArg` is deliberately kept to concrete self types (9 of them) plus disjoint generic constructors, and a deviating case is written down as *"a documented specialization arm INSIDE the production kernel — centralized, never a new marker type"* rather than an overlapping impl.

## Two rules that outrank everything above

- **A type-level guarantee that is not compile-fail-tested is a comment.** Add a
  `compile_fail` doctest or a `trybuild` case, or you have bought nothing.
- **`#[non_exhaustive]` buys your semver freedom with your users' exhaustiveness.**
  Never on an internal enum — it takes the refactoring tripwire away from you.

## Related

- Pricing a rung-2 or rung-3 decision → `rust-invoice`
- About to assert a *performance* property → `rust-errata`
- Inside the Mahout engine's hot path → `mahout-hot-path`
