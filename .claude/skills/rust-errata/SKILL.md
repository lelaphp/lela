---
name: rust-errata
description: "Rust performance and codegen folklore that MEASUREMENT KILLED — check before asserting any performance, binary-size, or compile-time property of Rust code. Use whenever you are about to say something is zero-cost, free, faster, smaller, vectorised, inlined, or cheaper to compile; whenever a review comment or design argument rests on a performance claim; whenever someone asks 'is this slow', 'will this bloat the binary', 'does dyn cost anything', 'should I worry about monomorphisation'. Fires DEFENSIVELY on your own output, not only on the user's code — the point is to stop a plausible claim before it ships. Every entry is re-derived from committed source, not remembered."
---

# Rust Errata

Confident Rust performance folklore fails measurement roughly **half the time**.
That rate was established over eight expert-adjacent articles, four expert deep
dives, and then a re-derivation harness that refuted six claims made by the deep
dives themselves — including two of their own errata rows.

**So the operating rule is: you are probably about to be wrong.** Check the table
before asserting. If a claim is not below, say what you actually know: the
mechanism, not a multiplier.

Every number here was re-derived on **rustc 1.95.0 / aarch64-apple-darwin** from
source at `docs/learning/rust-fp-async/probes/` in the mahout_php repo
(`RESULTS.md` has the verdicts, `run.sh` reproduces them). Where re-derivation
disagreed with the original study, **the re-derived value is the one printed.**

## The rule about numbers, which matters more than any single number

**Magnitudes do not travel. Directions do.** Two ratios were re-measured on the
same toolchain and same architecture as the original and came out 1.7× and 2.7×
different. In both cases one leg reproduced to within 1% and the other did not.

> Quote mechanisms and orders of magnitude. Never quote a multiplier as if it
> were a property of the language.

## Refuted — do not repeat these

| Folklore | What measurement says |
|---|---|
| "Iterator chains are zero-cost" | **An `-O2` claim only.** ~1.00× at `-O2`/`-O3`; **2.97× slower at `opt-level=0`**, which is where your tests run. Fix with `[profile.dev] opt-level = 1`, not a rewrite. |
| "`filter_map` resists fusion" | **Dead.** `filter_map(f)` and `map(f).filter(is_some).map(unwrap)` compile to one body — the second symbol does not exist in the object file. |
| "`codegen-units = 1` will fix generic bloat" | **Zero bytes moved** across cgu 1/16/256. CGU count does not change the *number of instantiations*, which is fixed upstream of partitioning. |
| "`Box<dyn Fn>` keeps the binary small" | **Bigger at every body size tested**, gap widening: +5 752 B at one statement, +8 952 B at eight. Every closure still pays a `call_mut` shim plus a vtable. |
| "`dyn` is smaller and cheaper to compile than generics" | **Inverted.** 200 call sites: `&dyn` was **2× larger** in text and the generic compiled **faster**. Erasure does not delete the closure bodies; it adds a shim per closure on top of them. |
| "Monomorphisation bloats the binary" | Usually the **wrong worry** — the cost is compile time. But see the ordering below, which is real. |
| "Specialisation will fix two overlapping blanket impls" | **No**, and not with an intersection impl either, and not under `min_specialization`. All give E0119. Coherence is decided on the **self type**, never on the bounds. Open since 2016 (#31844). |
| "Index loops pay a per-access bounds check" | **Half false.** Single slice, monotone index, bound from that slice: **zero** checks, same code as `.iter()`. A second collection reintroduces one check — **and that check costs nothing measurable** (0.99× against `zip`, still fully vectorised). Count checks as a **safety** metric, never a speed one. |
| "`#[inline]` on the inner function inverts the monomorphisation fix" | **No — it is a no-op.** Byte-identical to writing the body per kind. The mitigation has a failure mode where it does nothing, not one where it backfires. |
| "A bound on a marker struct catches nothing" | **Conditional.** Redundant only when something downstream already demands the capability. Otherwise the unbounded version **compiles silently** and the bound is the only tripwire. **Keep the bound.** |

## Confirmed — safe to assert, with the mechanism

| Claim | Evidence |
|---|---|
| `for x in a.iter().chain(b)` is a **large** performance bug | Order of magnitude (6–10× measured on two machines). `Chain::fold` runs two vectorised loops; `for` asks "which half am I on" per element. Use `.fold()`/`.for_each()` or two loops. Same effect, smaller, for `Flatten`. |
| Boxing an iterator deletes the fast path | Every internal-iteration override carries `where Self: Sized`, so it is not in the `dyn Iterator` vtable. |
| `sum::<f32>()` is not vectorised and never will be | IEEE addition is not associative; no fast-math on stable. LLVM emits **21 scalar `fadd`s** into one accumulator. Hand-rolled lane accumulation is ~5.8× faster **and** ~159× more accurate. The trade is not speed-vs-accuracy, it is speed-and-accuracy vs **specification**. |
| `step_by(k)` devectorises | Zero vector registers; a bare post-increment scalar loop. `as_chunks::<K>()` is ~3.1× faster when `K` is a compile-time constant. |
| `into_iter().map(f).collect()` reuses the source buffer; `iter()` does not | Verified by pointer identity. **Same-size-and-align only — narrowing also fails**, with no lint. The reuse **retains the source capacity**: len 333 334, capacity 1 000 000, 5.33 MB of slack invisible in `len()`. The only detector is `assert_eq!(before_ptr, out.as_ptr())`. |
| `Arc::clone` inside a hot loop is catastrophic | Order of magnitude ~10³. An atomic RMW on one shared cache line. Clone before the loop, or use `thread::scope` with a plain `&`. |
| A thin generic shim over an `#[inline(never)]` monomorphic core is a large win | **78.9%** text reduction at 512 instantiations. |
| An `Rc` in the *body* of a public `-> impl Trait` is a breaking change | With a zero-line signature diff. Four lines of `assert_send` turn it into your own compile error. |

## Version-stamped — RE-VERIFY before repeating

Everything in this section decays. It was true on **rustc 1.95.0**. If the
current toolchain is newer, run the probe rather than repeating the claim.

- **Still unstable on 1.95:** `try` blocks, TAIT, `std::async_iter::AsyncIterator`,
  `std::ops::AsyncDrop` (not even nameable), `gen` blocks, return-type notation.
  The deep dives recorded *believing several of these had landed*. They had not.
- `-Zpolymorphize` was **deleted in 1.85**. Monomorphisation cost is permanent
  architecture, not a pending optimisation.
- "Iterator-chain type errors are unreadable" is **substantially dated** — rustc
  traces `Iterator::Item` stage by stage now. Keep the cost, narrow the claim to
  *reported late*.

Re-verify with: `cd docs/learning/rust-fp-async/probes && ./run.sh`

## How to answer when the table does not cover it

1. **Say what the mechanism is**, not what the number is.
2. **Name the condition that flips it.** Almost every entry above has one.
3. **Offer the measurement**, do not assert the result:
   ```sh
   cargo rustc --release -- --emit asm && grep -c panic_bounds_check target/release/deps/*.s
   cargo llvm-lines --release | head -20        # where instantiations actually go
   size -m target/release/<bin> | grep __text   # not `ls -l`, which is mostly debug info
   ```
4. If you cannot do 1–3, say **"I don't know, and here's how we'd find out."**
   That is a better answer than a plausible multiplier, and this whole skill
   exists because someone competent believed a plausible multiplier.
