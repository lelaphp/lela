---
status: accepted
date: 2026-10-31
---

# The compiled tier is refused as the default on G3: tracing stays opt-in, and Cranelift stands

## Decision

`opcache.jit` defaults to `disable`, as php's does (`zend_accelerator_module.c:352`; `ext/opcache/src/ini.rs:161`). `jit=tracing` is not the default in the CLI or the worker.

## The term that misses

G3: fib takes 4.33× as long under the engine's tracing as under php 8.5.12's JIT. The bar is ≤ 1.1×, and the NO-GO line is > 1.5×.

G2 misses too, more weakly: tracing is 1.88× over the engine's interpreter (geomean 0.5310 over 8 programs), against a bar of ≥ 2.0×, and the reading is NOISE-SWAMPED overall, so G3 is the term the refusal rests on.

All four readings share key `1aa9d548346c6ff2d5cab055238d367707c13ecd` at commit c39550f. Each has 12 reps per arm after 2 warm-up reps, with the order alternated.

| bar | fib, arm a: median (IQR) | fib, arm b: median (IQR) | load at start (1, 5, 15 min) | load after |
|---|---|---|---|---|
| `oracle-jit`: php 8.5.12, `jit-tracing` (a) against `jit-off` (b) | 287.45 ms (7.63) | 588.44 ms (17.35) | 20.41 24.15 16.96 | 7.53 18.43 15.55 |
| `oracle-jit-aa`: `jit-off` against `jit-off` | 590.11 ms (12.70) | 589.09 ms (10.03) | 7.53 18.43 15.55 | 4.09 13.17 13.86 |
| `engine-jit`: the engine built with the `jit` feature, `jit-tracing` (a) against `jit-off` (b) | 1243.72 ms (289.78) | 1097.26 ms (9.32) | 3.93 12.99 13.79 | 6.81 9.72 12.18 |
| `engine-jit-aa`: `jit-off` against `jit-off` | 1093.26 ms (6.05) | 1087.47 ms (16.63) | 6.81 9.72 12.18 | 6.64 7.40 10.46 |

G3(fib) = engine-jit `fib_a_median_ms` ÷ oracle-jit `fib_a_median_ms` = 1243.72 ÷ 287.45 = **4.33**. **This quotient is unpaired.** Its two medians come from two bars, each run on its own, so the protocol's paired delta does not cover it.

The A/A drift, for comparison:
- `oracle-jit-aa`: fib 0.9995, geomean 1.0005, 0 of 8 rows resolved (passes).
- `engine-jit-aa`: fib 1.0018, geomean 0.9984, 1 of 8 rows resolved, mandelbrot (passes; the control fails at 3).

Both controls are within 0.2% of 1.0 on fib.

The engine's fib under tracing spreads widely: IQR 289.78 ms, min 897.79 ms, max 1573.62 ms. Tracing runs fib 12.46% slower than the engine's own interpreter, a resolved paired delta within `engine-jit`, so on fib the tier costs time rather than saving it. Within `engine-jit`, fib under tracing is 1.1246× its own `jit-off` arm, a paired delta of +12.46%, resolved.

Two earlier sets of readings are not cited:
- The first attempt at these bars, at c8e1ae2 with key `718e8dee52013a25eef8275586c6cf26120bfdc6` in main's store. Its `engine-jit` failed on #3684, which is now fixed.
- The figures the ruling set aside.

## The other terms

This departs from design §9.3's run of record, as the 2026-10-26 ruling allows. Each reading is cited by key and commit, or marked not taken:

| gate | bar | reading | figure |
|---|---|---|---|
| G1 door cost | within the A/A drift bound; above 2% revisit | `g1` `0d56845ce3e71dc9a69c0d6e4a1ef48b0dbc5125` at 757e701, with its control `g1-aa` at the same key, both UNLABELLED, load 12.71 11.71 13.60. The newer `g1` at 84b608b (`aff3e367…`) is a 1-rep run with no control, so it is not cited | 0.9947 against a bound of 1.0200; A/A 1.0003 |
| G2 compute | geomean ≥ 2.0× over the interpreter | `engine-jit` `1aa9d548346c6ff2d5cab055238d367707c13ecd` at c39550f, PROVISIONAL | tracing ÷ the engine's interpreter, geomean 0.5310 over 8 programs (1.88× over the interpreter), 7 of 8 resolved, 6 faster and 1 slower (fib); NOISE-SWAMPED overall |
| G3 against php | ≤ 1.1×; NO-GO > 1.5× | above | fib 4.33× (unpaired) |
| G4 memory and code | RSS ≤ +10%; code ≤ 2× php's | `g4-rss` `24e81e1192a86f5eab516a442c5627d0688a50ad` at f348a6c (A/A `g4-rss-aa`, same key); `g4-code` `5898090e1ce0ed6bb3bcf90504d3f9e4b1822638` at 12f6c5a; all PROVISIONAL | RSS 1.0625 (A/A 1.0167); code per opline 0.9894 of php's |
| G5 soundness | zero divergence under `force` and `exit-fuzz` | taken only in part. From `tests/`: `phpt` `tests@force` and `tests@exit-fuzz`, key `26541a3a874624bfe910677bd49b463d43ae17b2` at 58cecb7, both pass (768 passed, 124 skipped, 1 xfailed of 893). Not taken: the witnesses and `Zend/tests` under both | |
| G6 requests | FastCGI throughput ≥ 1.0× and ≥ php's JIT | not taken | |
| G7 recording overhead | reported | not taken | |

## What stays

- ADR-0006's seam.
- php_ssa (ADR-0015, as amended on #3563).
- Cranelift. #2967's profiles put fib's cost in the trace-hop protocol and in the oplines that `handle` and `step` serve. That is lowering, not codegen, so the IR fallback is not triggered.

## What ships, experimental

The `jit` feature is enabled in the cli, fpm and makala release builds, with `opcache.jit=disable` by default (#3676). Tracing is labelled experimental and sits outside every supported combination (#3591, principle 1). That the feature costs nothing while it is off is held by a fresh G1 reading that #3676 takes before the builds carry it. The G1 reading cited above (757e701) is UNLABELLED and predates the change.

## What a later GO needs

A new decision ticket, #3677, blocked by #3176, #2966 and #2967 (the performance pass, and #2140 for G6), with G1, G3, G5 and G6 passing in one run of record. It carries forward the 2026-10-23 question on cold code and the stretch under `Jit`.

## Considered and rejected

- A NO-GO on the figures already on hand: their denominator was taken on php 8.5.9, and their numerator predates #3231.
- Waiting for #2967 and #2966: that misses 9 Nov and #2981's fourth reading.
- The tier as a source-build feature only: nobody tries it, and the performance pass loses user reports.
