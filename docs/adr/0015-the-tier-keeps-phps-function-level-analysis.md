---
status: accepted
date: 2026-10-21
supersedes: jit-design.md revision 26 (ranges, reference counts and a function-level static inference not kept)
---

# The compiled tier keeps php's function-level analysis, computed on a trace's first pass through an op array, in a crate of its own below the tier

Revision 26 had `types` build php's `zend_jit_trace_build_tssa` with three things left out: ranges, reference counts, and a function-level static inference. It said each omission costs a guard and never a wrong answer. Each such guard is also an exit php does not have, and php's guards and exits are observable under `opcache.jit_debug`, which `trace-dump` holds. The division was the first case large enough to rule on (#3111). Unguarded, `$s = 3.0 / $i;` was stepped on every turn: 18.6e9 instructions for 30,000,000 turns, against php's 3.30e9. So the division gained a guard of its own (revision 60). Every later case would cost another guard and another ruling.

Mitchell ruled on #3149 (2026-10-21) that the omission is reversed and that the port waits on #2968's GO. On #3174 (2026-10-21) he ruled where the analysis lives, which crate holds it, how much of it is ported, and that ranges and reference counts come with it.

What php does, which this ADR copies:
- **When.** php's tracing JIT builds the analysis lazily, once per op array, the first time a trace's build runs through it (`zend_jit_trace_build_ssa`, called from `zend_jit_trace_build_tssa`, `Z/ext/opcache/jit/zend_jit_trace.c:1355`, `:1437`). It keeps the result on the op array's JIT extension.
- **How much.** `opcache.jit=tracing` runs it at `ZEND_JIT_LEVEL_OPT_FUNCS` (`zend_jit.c:3674`). That covers the CFG, the SSA built with `ZEND_SSA_RC_INFERENCE | ZEND_SSA_USE_CV_RESULTS`, the calls analysed within the script (`zend_analyze_calls`), return info for a declared return type, and `zend_ssa_inference`, which always infers ranges as well as types (`zend_inference.c:4915-4917`).
- **What it skips.** There is no analysis for an op array with no function name (the script's top-level code), a generator, an op array with a try/catch, or one with indirect variable access (`zend_jit.c:1098-1150`).

## Decision

- **The crate.** `php_ssa` holds a port of `Zend/Optimizer`'s `zend_cfg.c`, `zend_dfg.c`, `zend_ssa.c`, `zend_inference.c`, `zend_call_graph.c` and `zend_func_info.c`, run as `zend_jit_trace_build_ssa` runs them at `ZEND_JIT_LEVEL_OPT_FUNCS`. Its public surface is a lock of its own. It depends on the unit format and on the linked class record that `zend_inference` reads for a property's declared type (`php_unit`, `php_link`), and on nothing above them. `php_jit` calls it, and nothing else does.
- **When it runs.** An op array is analysed the first time a trace's build runs through it, and never at publication. The result is published write-once into the tier's state for the epoch (`tier::epoch_state`, revision 27), and is immutable and `Sync` from then on. An op array php does not analyse gets no analysis here either, and keeps the trace's own guards.
- **The internal-function table.** The return information php keeps for internal functions (`Z/Zend/Optimizer/zend_func_infos.h`) is extracted from the pinned php-src into `data/`, as `ops.toml` is, and rendered in `build.rs`.
- **What `types` reads.** `types` reads the ranges, the reference counts and the types where `zend_jit_trace_build_tssa` reads the op array's SSA variable info. Each guard that revision 26 placed in their stead retires: the overflow guard on a long that php proves stays long, the entry guard on a loop variable written before its loop, and the division guard (#3111, revision 60).
- **Where reference counts reach the lowering.** Where php's reference-count information changes the code it emits, and not only which guards it places, `lower` follows php there too.

## Considered options

- **Keep revision 26.** Rejected on #3149. Each guard placed in the analysis's stead is a lasting difference that `trace-dump` sees and that needs a ruling of its own, a cost that grows with every trace. A port is a bounded cost.
- **Compute the analysis at publication, in `php_compile`, carried immutable with the unit.** Rejected on #3174. Every compile would pay, including with the tier off and for code that never gets hot, and an analysis php keeps inside its JIT would move into the compiler.
- **Port into `php_jit`, its ceiling raised from 17,500 to about 26,000.** Rejected on #3174, for ADR-0012's reason: one ceiling would measure two things, the tier and php's analysis, and a change to either would compete for the same room. php keeps the two apart too, as `Zend/Optimizer` and `ext/opcache/jit`.
- **Port `ZEND_JIT_LEVEL_OPT_FUNC` only, with no call analysis and no internal-function table.** Rejected on #3174. Wherever a call's return type matters it would leave guards php does not place, each of them another ruling.
- **Port types alone, with ranges at their widest and every value both RC1 and RCN.** Rejected on #3174. It follows #3149's wording to the letter, but it keeps two of revision 26's three omissions, and the differences they cost, for an analysis php runs as one pass.

## Consequences

- `php_ssa` has its own `[[crate]]` row in `seams.toml`, its own ceiling and its own lock, all a person's. `php_jit`'s ceiling goes on measuring the tier alone.
- Under `opcache.jit_debug`, a trace through an analysed op array places php's guards and takes php's exits, so its `TRACE` lines and exit numbers are php's. A trace through the top-level code, a generator or an op array with a try/catch is unchanged, as in php.
- The interpreter, the compiler and compile-identity are untouched. An op array nobody traces is never analysed.
- §3.2 and the maintainer's note hold. The SSA is php's own representation, read by the tier alone and never executed. There is still one executor, and no IR of this engine's own.
- The epoch's tier state gains one slot per analysed op array, written once.
- *Amended 2026-10-26 (#3563):* the build no longer waits on #2968. A NO-GO on #2968 defers the default; the port stands, and any later GO's run of record includes it.
- *Amended 2026-10-27 (#3571):* php does not keep the result on the op array's JIT extension, as the first "When" bullet says: `zend_jit_trace_build_ssa` rebuilds it at every trace build (`Z/ext/opcache/jit/zend_jit_trace.c:541-546`). The epoch's write-once slot is per op array and binding set, the classes its view bound (`named`, `scope` and `engine_named`, as `ClassId`s), so a kept analysis is observably php's rebuild; an analysis that binds a request-scoped class is never kept (#3579).
- *Amended 2026-11-05 (#4190):* `php_ssa` depends on `php_unit` and `php_heap`, not on `php_link`. What an op array does not hold, it asks its caller through `LinkedTable`, whose class handle is the caller's, so the analysis reads what the view bound on a thread with no request.
