# The third engine's JIT: module design

This is the design of record for the compiled tier. It is one living document: every section is at its current value, and the ledger below records each revision with what changed, why, and the check that would show it wrong. Git holds the dated versions. `research/jit-design.r1.md`, `research/jit-design.r2.md` and `research/jit-design.r3.md` are the three earlier revisions, kept unedited as records beside `research/jit-research.r1.md` and `research/llvm-research.r1.md`; they are not read for anything this file marks current, and the "highest `N` wins" naming rule no longer applies to this note. Design of record for the engine: `third-engine-design.md`. Research: `research/jit-research.r1.md` for every php-src citation (`§A` to `§F` below name its parts), `research/llvm-research.r1.md` for the LLVM question. Vocabulary: `CONTEXT.md`. Decisions: `docs/adr/0001` to `0006`. Architecture terms (module, interface, seam, adapter, depth, leverage, locality, the deletion test, design-it-twice) are the `codebase-design` vocabulary. "Design §5.8" names a section of `third-engine-design.md`; "r2 §5.8" names the record `research/third-engine-design.r2.md`.

**Question.** Where do the seams of `php_jit` and the `Tier` seam go, what is each module's interface (everything a caller must know: signatures, invariants, ordering, error modes, cost), what stays behind it, and how is each tested through its interface and nothing else?

---

## Ledger: what changed, and why

Each entry is numbered as the revision that made it and names the check that would show it wrong.

**1. Revision 1 (2026-09-06).** Took `research/jit-research.r1.md §E` and placed its seams: `Tier` on `execute` with `Nop` and `Jit`, an engine-level word on `Func` replacing `Func.code`, `TraceIr` as a value seam with two consumers, the kernel face in `php_jit` by one `kernels!` list, code lifetime by `Arc` strong count under one lock, bytes final at publication. Check: G1 within the drift bound; G2 and G3 against a variant that keeps `Func.code`.

**2. Revision 2 (2026-09-06).** One hook instead of two; the recorder is a loop over `step`; `table` owns every published trace; `policy` is pure and `compile` is the pipeline; an exit embeds a `Word`; `Tier::extension()` declares the tier's state; `exits` folded into `enter`; a self-settling `Claim`. Check: G7 for `step`'s cost under recording; a panic inside a fixture recording leaves the word `cold`.

**3. Revision 3 (2026-09-06).** One `dispatch` for both homes; an opaque word per safepoint numbered by the compiler and a one-level `TraceId`; a dual-mapped arena; `step` returns `Stepped`; `lower` owns its backend; compile off the request thread on one worker per engine; `Published` named as the value the unit cache stores; the entry frame as a slot on the VM stack. Check: the day-31 `mov; ret` through two views on aarch64; the concurrent publish test on both targets.

**4. Revision 4 (2026-09-07).** A review of every design, the seams, the month-one to month-two transition and the harness found four things the interpreter must do so that the tier can exist that were stated as goals and not as mechanisms, two of which would have produced wrong behaviour under `jit.mode=force` on the first day, and a set of contracts stated more strongly than the code could keep. Eight changes:

1. **Compiled loops test the interrupt flag** (§4.3, §4.6, §2.2). php emits a `vm_interrupt` test in every compiled loop and tests it again at every exit (`research/data/jit-research/rfc-jit.md:118`; `php-src-jit-exit-points.txt:298`). Revision 3 removed the back-edge safepoint from compiled code and put nothing in its place, so `max_execution_time` and `pcntl` signals could not stop a hot loop, and the design of record's own witness `set_time_limit(1); for (;;) {}` would have hung under `force`. The trace's loop header now loads the request's interrupt flag through `ExitCtx` and exits when it is set; the `Resume` contract gains the interrupt check the loop already does at a back-edge.
2. **Recording is exclusive per request** (§4, §4.9). A kernel called during recording can re-enter PHP, reach a safepoint on another word, win a claim and start a second recording on the same request. php refuses this with a per-process flag and bumps the counter instead (`php-src-jit-hot-root.txt:30-33`). `JitRequest` gains a `recording` bit that `hot` holds through a guard, and `visit` never claims while it is set.
3. **The entry safepoint is raised by frame activation, not by a body handler** (§2.2). Revision 3 said `safepoint` runs "inside the first body opline after the `RECV*` prologue", and that opline can be any opcode: as written it was a compare per opline or a mutable handler table. The call handler that activates a user frame runs the prologue and raises `Site::Entry` before it reports `Pushed`, which is the shape the recorder already assumed at `Stepped::Pushed`.
4. **Request-scoped op arrays carry sealed words** (§3.2). `Jit::safepoint` read `func.word(at)` and cloned `func.published()` for every frame, but an op array from `eval` or an uncached include has no `Published`. A sealed word, all ones, decodes as `blacklisted` and never claims; `Func::published()` returns an option that no path on the hot side reads.
5. **`Mode::Off` touches no word** (§4). Thresholds at `u32::MAX` still bumped the shared counter on every safepoint on every thread, and a saturated counter would eventually have claimed. `Off` is one mode load and `Continue`; that is what G1 measures.
6. **Contracts restated where they were too strong.** The word ordering argument of §3.2 did not follow from a relaxed read; the sound rule is the fallback `run` already had, and it is now the contract. The crossing's invariant in §4.6 is restated per fiber stack. `types` gains the clobber rule for call-effect kernels. `step` and `Site::Loop` gain the fused compare-and-branch rule. `JMPZ` joins the `safepoint` column and the compiler asserts the invariant.
7. **Sizes and names corrected.** `tier.lock` is not a few dozen items once the kernels are in it; the kernels enter as one generated section. `opcache.jit_buffer_size` is the one name. `jit.mode` is accepted from any host and logged, since no startup ini can know its host.
8. **The tier bit question of `research/llvm-research.r1.md` D.9 is answered** (§4.5): a region is a table entry with the same `Entry` ABI, so the id space needs no bit.

Check: G5 under `force` with the time-limit and signal witnesses of §7; one `TRACE start` line while a nested recording would have opened; `Probe` sees `(Entry, 0)` once per call and never when a `RECV` throws; an `eval` op array's words read `Blacklisted`; G1 with the tier bound and `opcache.jit_buffer_size=0`.

**5. Revision 5 (2026-09-07).** The pre-day-1 extraction of `ops.toml` from the pinned tree (design ledger 9, `data/ops-extraction.md`) settled the second and seventh of the "if this is wrong" items and corrected one sentence of §3.1. `JMPZ` is not a pre-optimizer back-edge: `zend_emit_cond_jump` does not invert on `BOOL_NOT`, `do { } while (!$x)` is `BOOL_NOT` then a backward `JMPNZ`, and the column keeps `JMPZ` for optimizer output only; `FAST_RET` jumps to an earlier index without being a back-edge and the compiler's assertion excludes it; the six site opcodes leave `result` unused in every handler body and every emission, so the second item is verified. php's entry counter sits after the `RECV*` run for an untyped function and on opline zero for a typed one (`zend_jit.c:3219-3231`, `zend_jit_trace.c:8985-8999`), so the seventh item is a known difference and not a risk: `Site::Entry` stays after the prologue, a `RECV` that throws is never inside a recording, and a typed function's call whose `RECV` throws is a root in php and not here. That is the third `trace` row of the registry (§4.1). Check: the `TRACE start` diff on a typed kernel reports that site and nothing else; `Probe` sees `(Loop, n)` from a `JMPNZ` on `do { } while (!$x)`; a witness with a `finally` compiles under the assertion.

**6. Revision 6 (2026-09-13).** §4's `Lines` column and its total go (#192). Design ledger 14 took the budget column out of `third-engine-design.md` §4.1 under the rule AGENTS.md states, that a design document carries no number `cargo xtask budget` could disagree with, and this document still ended §4 with "Total 17,450 against the 17.5k ceiling": the 17.5k is `php_jit`'s `[[crate]]` row in `seams.toml`, a second statement of one fact, and the 17,450 was the sum of a per-module column that was a plan and not a measurement, one that would be wrong the first time a module landed at another size with nothing to catch it. The table keeps the modules, their seams, their interfaces, what each hides and which two carry `unsafe`; the sentence under it cites the manifest for the ceiling. §9.2, a crate-map row "in the shape of design §4.1", follows the shape §4.1 now has and drops its `Budget` cell; §9.3's "`xtask budget` keeps the 17.5k ceiling" says what the instrument holds without restating what it holds it to. "About 300 lines" for `code` and `enter` together stays, since it is a claim about the allowance and not a ceiling the instrument reads. Rejected: keeping the column with the manifest named beside it, which is two statements of one fact, the defect itself. Check: `grep -n "17,450\|17\.5k" jit-design.md` finds nothing, and no number in this document is one `cargo xtask budget` reads from `seams.toml`.

**7. Revision 7 (2026-09-26).** §5.3 and §6 follow design revision 34 and ADR-0007. §5.3 said `php_jit` sees more of `php_engine` than `api` "as `php_macros` does (ADR-0002)", and §6's graph drew `php_engine ◄── php_macros`. `php_macros` now depends on no workspace crate and `php_engine` depends on it, so `php_jit` is the only crate that sees past `api`, and the graph draws `php_engine ──► php_macros`. `tier.lock`, `php_jit`'s edges and everything the tier reads are unchanged. Check: §5.3's sentence names no crate but `php_jit` as seeing past `api`, and §6's first line matches the design's §4.2.

**8. Revision 8 (2026-09-30).** §2.4 gains the debugging host as the seam's first production adapter, settled by Mitchell on #1308. #1165 ruled phpdbg into parity, 71 tests under `sapi/phpdbg/tests`, and php's phpdbg sees each opline by replacing `zend_execute_ex` with its own loop, which principle 1 forbids. The `Debugger` is shaped as the recorder is: `safepoint` fires at three sites and not at every opline, so it is the gate, and `step` is the loop that sees each handler, handing back with `Resume`. Watchpoints are compared between steps, which is where php compares them too; its `mprotect` only narrows which pages it compares. The debugger lives in `sapi/phpdbg` over a re-export of the face through `php_sapi`, whose items open `tier.lock`. Rejected: php's second loop; a per-opline flag in the interpreter, the debugger's cost on every request; `mprotect` watchpoints, a process-global SIGSEGV handler beside Rust's stack guard in a process whose requests share it on threads; and a `php_engine::debug` module, debugger policy in the engine. Nothing is reversed, so ADR-0006 stands and there is no new ADR. Check: `sapi/phpdbg/tests` run under the engine's `phpdbg` and read their misses against the 71; G1 holds `execute::<Nop>` within the drift bound; `api-check` finds no `sapi/phpdbg` edge to `php_engine`; `unsafe-audit` finds nothing in `sapi/phpdbg`.

**9. Revision 9 (2026-09-30).** §4 and §4.9 move the recording exclusion from `JitRequest` to the running context's trace number, which the fiber switch saves, settled by Mitchell on #627; design §5.11 says the same in revision 52. php's `EG(jit_trace_num)` is an executor global that `zend_fiber_vm_state` saves across every switch and `zend_fiber_execute` zeroes for a fiber's body, as a generator resume does. The bit in `JitRequest` was the tier's own request state, which revision 29 of the design put outside the switch: a recording suspended with its fiber kept it set, so the fiber that resumed recorded no root, and a fiber suspended during a recording and never resumed, which cancelled tasks and abandoned generators do routinely, would leave a long-lived worker unable to record at all. The number is engine storage the tier alone interprets, so the switch still swaps only engine state; the face gains `Request::trace_num` and `set_trace_num`. Rejected: the bit kept in `JitRequest` with a fourth `trace` registry row, which keeps the stuck exclusion. Nothing is reversed, so there is no ADR. Check, once `php_jit` records: the `TRACE start` diff against the oracle on a witness that suspends a fiber inside a hot loop under `jit.mode=force` and `jit.compile=inline` shows php's roots, and a fiber suspended inside a recording and never resumed leaves the next hot loop on the resumer recorded.

**10. Revision 10 (2026-10-01).** §2.2's `Flow` gains `Returned`, the edge by which a tier hands back a serving's entry frame it ran to its end, settled by Mitchell on #1456. `Resume` with nothing pending dispatches the frame's opline again, so a tier that left the entry frame of an `execute` as returned (php's `HANDLE_EXCEPTION` with nothing pending leaving through `zend_leave_helper`) had no answer but the unwind exit: phpdbg ended a callback's serving, where php's `array_map` gets nothing back and the run goes on. php's JIT has the same edge, `ZEND_VM_RETURN` for a `ZEND_CALL_TOP` frame, so the compiled tier reads it too, and the engine still holds no debugging concept (#1308). Rejected: a `Resume` whose frame's opline is set past its end, a sentinel through an existing edge; and a divergence row, which loses the callback's caller. ADR-0006 stands: the seam is still one method on `execute`; the face gains one variant and `tier.lock` its line. Check: #1456's callback sequence under the engine's phpdbg writes `NULL`, `end` and `[Script ended normally]` as php does.

**11. Revision 11 (2026-10-04).** §4.3 says what compiled code emits at observed calls, returns and native calls, settled by Mitchell on #1776 for #1770, beside design revision 79, which adds seam 9, `Observer`, to design §4.3. This note said nothing about observers, although its traces inline calls and push frames through their own kernels. Now compiled code emits observer events where php's JIT does (`Z/ext/opcache/jit/zend_jit_ir.c:4620-4717`): when an observer was declared at engine build, at every compiled call, return and native call, inlined ones included, it reads the callee's resolved slot at run time and calls begin unless the slot reads none-observed, and calls end when the frame is the current observed one; an observed call keeps its frame. When none was declared, nothing is emitted. Rejected: leaving this note silent until `php_jit` starts; and an ADR, for a seam that reverses nothing. ADR-0006 stands; nothing is reversed, so there is no ADR. Check, once `php_jit` compiles calls: a recording observer sees the same begin and end events under `jit.mode=force` as under `Off`, inlined calls included; with no observer declared, compiled code holds no observer call.

**12. Revision 12 (2026-10-06).** §4.1 keeps the three differences from php's traces as decisions of this note, and the registry keeps only the one the interpreter already shows, settled by Mitchell on #2142. The rows for a nested `execute`'s entry frame and for a full root table were accepted on 2026-09-06 against a recorder and a table that do not exist, so no reading could hold them. The entry-counter citation moves from the function JIT's `zend_jit_setup_hot_counters_ex` (`zend_jit.c:3219-3231`) to the tracing JIT's `zend_jit_setup_hot_trace_counters` (`zend_jit_trace.c:8983-9001`), which places the counter by the same rule. Rejected: both rows kept, reworded as future. Nothing is reversed, so there is no ADR. Check, once `php_jit` records: the `TRACE` diff on a nested `execute` and on a full root table raises exactly these two differences.

**13. Revision 13 (2026-10-07).** §4.3 says what compiled code does at an opcode an extension observes, settled by Mitchell on #2031 (2026-10-07), reading 1, php's, and landed by #2256. Design §7.1 lets an extension observe a declared set of opcodes while the handler always runs (design revision 83), and this note said nothing of it beside *Observed calls*. php's tracing JIT refuses to start when any opcode other than the two silence ops has a user handler, and warns (`Z/ext/opcache/jit/zend_jit.c:3743-3756`); so the tier does not start for an engine whose bundle declared one, bound `Mode::Off` where the tier is bound, with php's warning through the startup diagnostics, and at `ZEND_BEGIN_SILENCE` and `ZEND_END_SILENCE` compiled code tells the declared opcode's observers before the kernel. Rejected, by the ruling: reading 2, which emits the told path before every declared opcode's kernel where php refuses to compile at all. Nothing is reversed, so there is no ADR. Check, once `php_jit` starts: an engine whose bundle declares `ZEND_ADD` binds `Mode::Off` and raises php's warning once at startup, and one that declares only the silence ops compiles traces.

**14. Revision 14 (2026-10-07).** The trace table lives in the epoch and the view holds the epoch once, settled by Mitchell on #2186 (2026-10-07) and landed by #2225, beside design revision 102, with §4.1's owned view, §4.5's lifetime rule and §5.3's `hold_class` typed in the ruling's words. A trace's code embeds addresses from the units it inlined and the published classes it read, all of which live as long as the epoch (design §5.14 *Identities*, §6.3), so the table belongs to the epoch: a restart starts the next epoch with an empty table, as `zend_jit_restart` resets php's buffer, the old table and its code are freed with the old epoch, and when a bound is reached the table stops the hot counters and compiles nothing more until the restart, as `zend_jit_stop_counter_handlers` does (`Z/ext/opcache/jit/zend_jit_trace.c:8164`). The consequences land where they stated the old shape: `sweep` goes from §4.5's listing and `publish`'s doc, §4.4's chunk-reuse row and test sentence, §4.8 and §8's `table` row; `Trace.units` goes and `publish` takes no units; a link is a `TraceId`, never reused within an epoch, and a slot is written once, so `get` borrows the table and `TraceGuard`'s `arc_swap::Guard` and the `ArcSwapOption` slots leave the design; `Holder`, `Claim` and `Job` hold the epoch; `hold_class` holds nothing and accepts a published `ClassId` (`ClassId::is_published`), and `is_engine_immutable` goes; a request-scoped class is one whose `ClassId` is request-local. Two passages the consequences do not settle still have the view own `Arc<Published>` values, `hold`'s sentence in §4.1 and `UnitView`'s doc and `units()` in §5.3; #2269 asks a person for their words. Rejected, by the ruling: split lifetimes, each trace holding the epoch (which keeps the sweep and needs #2215 first), per-class holds, and listing the causes of a request-scoped class. Nothing is reversed, so there is no ADR. Check: no sentence of this note has a trace freed alone, or a published class held by anything but the epoch; once `php_jit` publishes, a table at `opcache.jit_max_root_traces` compiles nothing more and an `opcache_reset` starts an empty one.

**15. Revision 15 (2026-10-09).** §2.4's debugging host reads frames, op arrays and the linked tables through the native face from its own tier, as an extension does, and `tier.lock` keeps the mechanism, the writes and nothing that reads, settled by Mitchell on #1911 (2026-10-07, option 1) and built by #1911. Revision 8 had `php_sapi` re-export the frame reads the debugger needs beside `Tier`, `Site`, `Flow`, `step` and `Stepped`; once #1827 and #1829 put `FrameView`, `OpArrayView` and `Request::op_array` on the native face, a frame and an op array had two read paths, against design §3.2's one function per semantic. The tier face now hands out positions and nothing they hold: `entry`, `below` and `catches` for the stack a host steps and unwinds, `frame` to read a position through the native face, and the writes (`exit`, `set_aside`, `restore`, `evaluate`, `evaluate_code`, `call`, `set_argv`, `set_stdin`); `Identity`, a counted value's place, which a watch list compares, moves to the native face's values. The debugger's digests (`Code`, `Class`, `Position`, `Serves`, and the variable, literal, superglobal, constant, error and backtrace reads) move to `sapi/phpdbg`, built over `FrameView` and `OpArrayView` and the linked tables the native face gains: `Request::functions`, `class_keys` and `constant_table`, with `FrameView::opline`, `Request::frame_var` and `frame_vars`, `Request::dump_op_array` with `OpArrayView::dynamic_functions`, and `Request::backtrace_here`. ADR-0008's edge sentence and design §4.1's `sapi/*` row (design revision 116) say the same. Rejected, by the ruling: the digests kept in `php_engine` as the tier's reads (option 2). Nothing is reversed beyond the sentences amended, so there is no new ADR. Check: `crates/php_engine/tier.lock` names no `tier::` item that reads a frame, an op array or a linked table; `api-check` finds no `sapi/phpdbg` edge to `php_engine`; the `sapi/phpdbg/tests` subset meets every test it met before.

**16. Revision 16 (2026-10-09).** §2.4's debugging host holds the main frame before it executes it, `Tier::HOLDS`, and raises that frame's observer begin with `tier::begin` where it executes it, settled by Mitchell on #2480 (2026-10-09) for what #2359 built (merged at 32bd6ca6). php's phpdbg compiles the program at its prompt and calls `zend_execute` on it only at `run` (`Z/sapi/phpdbg/phpdbg_prompt.c:866`), and `zend_execute` raises the main frame's `ZEND_OBSERVER_FCALL_BEGIN` before `zend_execute_ex`; the engine had raised it as the frame was pushed, so an observer saw a begin for a program never run. Now `tier::execute` raises it before the loop unless `T::HOLDS`, default false, `Debugger` declares `HOLDS` and calls `tier::begin` at `run`, and `Request::push_frame` raises none; design §4.3's seam 8 (design revision 118) says the same. Rejected: no sentence, the declaration left to `tier.rs`'s rustdoc. Nothing is reversed, so there is no ADR. Check: `crates/php_engine/tier.lock` carries `tier::begin`; `sapi/phpdbg/src/debugger.rs` declares `const HOLDS: bool = true`; `sapi/phpdbg/tests/observed.rs` passes.

**17. Revision 17 (2026-10-10).** §2.2's `Tier` listing gains the trait's two consts, `const HOOKED: Hooked = Hooked::NONE;` and `const HOLDS: bool = false;`, each with one doc line naming its revision, as `crates/php_engine/src/tier.rs:99,107` declares them, settled by Mitchell on #2490 (2026-10-09) and landed by #2496 beside design revision 120. Design revision 58 gave the tier `HOOKED` and revision 16 gave it `HOLDS`, and the listing still showed `safepoint` and `extension` alone. Rejected, by the ruling: a region rendered from `tier.rs`, which would lose the listing's annotations, and the listing dropped for a pointer to `tier.lock`. Nothing is reversed, so there is no ADR. Check: §2.2's listing names `HOOKED` and `HOLDS` with the defaults `tier.rs` gives them.

**18. Revision 18 (2026-10-10).** §4.1's `hold` sentence and §5.3's `UnitView` doc and listing say what the view holds under an epoch it holds once, settled by Mitchell on #2269 (2026-10-09): `hold(req, f)` holds nothing, as `hold_class` does, and `units()` goes. Revision 14 had `Recorded.view` hold the request's epoch once and left two passages, which #2269 asked a person about, still having the view own `Arc<Published>` values. Published units live as long as the epoch the view holds, so an `Arc<Published>` per op array was a second hold of what the epoch already keeps. §4.1 now reads "`hold(req, f)` accepts an op array whose unit is published and returns `false` for a request-scoped one, holding nothing", and its last sentence says `UnitView` holds the request's epoch and the ids it accepted. §5.3's doc says the same, and `pub fn units(&self) -> &[Arc<Published>]` leaves the listing with `Trace.units`, its one reader, `publish`'s `units` argument, gone at revision 14. For the same reason §4.8's `pipeline` doc no longer has `publish` take the view's units, as §4.5's `publish(compiled, start)` already did not. Rejected, by the ruling: `hold` keeping an `Arc<Published>` beside the epoch. Nothing is reversed, so there is no ADR. Check: `grep -n 'Arc<Published>' jit-design.md` matches what a request holds (§3.1, §3.2's `Func::published`) and ledger rows alone, and no line of §5.3's listing names `units`.

**19. Revision 19 (2026-10-18).** §4.4's macOS sentences state what #2741's tests showed on macOS 15.6, aarch64, where §4.4 said "unverified", as #2741 asks of its own work: the reused-chunk `isb` question is answered for macOS (a thread that never entered the reused chunk read the new body in every round, with its own `isb` and without, so `code` asks none of it), the `mov w0, #7; ret`, concurrent publish and reused-chunk tests are named as passing there, `sys_icache_invalidate` is named as the macOS maintenance `code` calls, and the `Pages` seam is stated as #2741 built it, `map_pair(len)`, `write(at, bytes)`, `sync(range)` and `rx(at)`. The first "if this is wrong" item says the same. Linux, its two aliases and its `memfd` sealing stay unverified. Nothing is reversed and no ruling is widened, so there is no ADR. Check: `cargo test -p php_jit` on macOS aarch64 runs `table::code::native`'s four tests and passes them; `grep -n unverified jit-design.md` finds no §4.4 sentence about macOS.

---

**If this is wrong, it is wrong in eight places.** First, that the write view of a dual-mapped arena costs nothing on the execute path: it does not touch it, but on aarch64 the cache maintenance after a write through the other alias is the part no source in the note settles (§4.4); on macOS aarch64, whose two views are one address, #2741's `mov; ret`, concurrent publish and reused-chunk tests pass (revision 19), and on Linux aarch64 the same tests through the two aliases are the check. Second, that every loop header and every return can be numbered by the compiler and found by its handler: a backward `JMP`, `JMPZ` or `JMPNZ` and a `RETURN*` leave `result` unused in php's op arrays (§3.1; verified at 8.5.9 in every handler body and every emission, `data/ops-extraction.md`), `ops.toml` says which field per opcode, and the compiler asserts that every backward jump it emits other than `FAST_RET` has a column and a header, so a disagreement is a compile-time failure on the first op array that has one. Third, that a `UnitView` can be owned rather than borrowed, so that `types::build` runs on a thread with no request (§4.1, §4.8); if engine-immutable classes turn out not to be shareable by `Arc`, the compile thread is kept and `build` alone moves back to the recording thread. Fourth, that the entry-frame slot on the VM stack is per fiber (§2.2). Fifth, that a 24-bit one-level id, a segmented read side with a guard per trace entry, and one worker do not measurably move G1, G4 or G6. Sixth, that one interrupt test per compiled loop iteration is enough and costs what php's does: a trace has at most one back-edge, the root's, and a recursive trace is bounded by `opcache.jit_max_recursive_calls`, so every trace without a loop runs a bounded number of oplines before the interpreter's own check; G2 carries the cost and the §7 witnesses the sufficiency. Seventh, that raising `Site::Entry` at activation is where php's counter fires: it is, for a function with no typed parameters, where php installs the counter after the `RECV*` run it skips (`zend_jit_trace.c:8983-9001`, `zend_execute.c:4426-4437`); for a typed function php's counter is on opline zero, before the `RECV` that may throw, so php counts such a call and this engine does not. That is no longer an open item but a `trace` row of the registry (§4.1), kept so that a throwing `RECV` is never inside a recording. Eighth, that all ones decodes as `blacklisted` in every future layout of the word: it is a one-line invariant of `policy` with a test, and the sealed word is the only bit pattern `php_engine` knows.

---

## 1. The shape in one paragraph

`php_engine::execute` is generic over a trait `Tier` with one hook, `safepoint`; `Nop` is the month-one adapter and `php_jit::Jit` is the month-two adapter, bound once per `Engine` as a function pointer, with the tier's state declared by the tier itself through the extension mechanism. The compiler numbers the loop headers, the entry and the returns of every op array; publication allocates one opaque zero `Word` per safepoint inside `php_engine`'s `Func`, a request-scoped op array gets sealed words, and `php_jit::table` allocates one word per exit of every published trace; `php_jit::policy` alone gives the bits meaning, in one state machine, and a `TraceId` is one number in one space whichever word it is read from. `php_jit` has ten modules: `dispatch` drives the one state machine and the trace-to-trace loop, `record` is a loop over the interpreter's `step` and returns a record with an owned view of what it touched, `types` and `lower` are pure and meet at the value `TraceIr`, `lower` owns its backend and emits the interrupt test at the trace's loop header, `table` owns every published trace, the one lock, and a dual-mapped arena, `compiler` is the pipeline from a record to a published trace and runs it on its own thread, `policy` is thresholds and transitions with no side effects, `enter` is the one `unsafe` crossing, and `lib` is `Jit`. Externally the crate exports one item.

---

## 2. The `Tier` seam

### 2.1 Design it twice: what `step` returns

| | (A) `Option<FrameRef>` | **(B) Chosen: `Stepped`, the edge the loop took** | (C) The recorder installs a frame-chain observer |
|---|---|---|---|
| What the recorder learns | a frame, and infers push, pop, move and the entry pop by comparing it with a `prev` it read before stepping | one enum arm | a callback per push and pop |
| Where frame-chain knowledge lives | in `record`, outside the module that manages the chain | in the loop, which pushed, popped, moved or threw | in `php_heap::VmStack`, behind a hook every push pays |
| The generator case (`GENERATOR_CREATE` moves the frame to the heap) | a special case the recorder must know | `Stepped::Moved` | a third callback |
| The entry frame | a bit in the frame header the recorder reads | `Stepped::Done`; the loop knows its entry frame | the callback must be told |
| Cost per step | one pre-read, one or two compares | none beyond what the loop already knows | a hook on every push in every request |

(B) is chosen. The loop's body already knows which of five things a handler did, because it is the loop that did it; returning that fact costs a discriminant. (C) is Mahout's door again, on the heap this time.

The earlier design-it-twice tables, one hook against two (revision 2) and the recorder as a loop over `step` against a third `execute` instantiation or a per-opline flag (revision 2 §2.1), stand in the records; their conclusions are the interface below.

### 2.2 Interface

In `php_engine::tier`, on the tier face (§5.3):

```rust
pub trait Tier: 'static {
    /// Called at three sites and nowhere else: in a backward `JMP*` handler with the jump
    /// target as `frame.opline` (Site::Loop); by the activation of a user frame, after its
    /// `RECV*` prologue has run without raising (Site::Entry); in `RETURN*` before the frame
    /// pops (Site::Return). `frame` is the frame the loop is executing.
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow;
    /// Which of php's execution hooks the tier replaces, as phpdbg's does (design revision 58).
    const HOOKED: Hooked = Hooked::NONE;
    /// Whether the tier holds the frame `execute` enters until it executes it (revision 16).
    const HOLDS: bool = false;
    /// State the tier needs, declared as an extension would declare it (design §4.4).
    /// Registered by `EngineBuilder::tier`; `Nop` declares none.
    fn extension() -> Option<Extension> { None }
}

/// The handler that has the number passes it: a backward `JMP*` carries its header's number, a
/// `RETURN*` carries its own, and the entry is always zero. A frame cannot know which site it
/// is at, so the number does not come from the frame.
#[derive(Clone, Copy)] pub enum Site { Loop(SafepointIdx), Entry, Return(SafepointIdx) }
impl Site { pub fn idx(self) -> SafepointIdx; }

pub enum Flow {
    /// Nothing happened; run `frame.opline`.
    Continue,
    /// The tier ran code. `frame.opline` and the frame chain were changed and are consistent.
    /// The loop performs the interrupt check it performs at a back-edge, then the pending-exception
    /// check it performs after a `throw`-effect op, then dispatches from the given frame.
    Resume(FrameRef),
    /// The tier ran the serving's entry frame to its end: the serving is over and its return
    /// value is set. `execute` takes it as `Stepped::Done`, as php's JIT returns `ZEND_VM_RETURN`
    /// to `execute_ex` for a `ZEND_CALL_TOP` frame (`zend_jit_ir.c:2104`, `:2483`).
    Returned,
    /// A fatal, `exit` or memory-limit bailout was raised inside tier code.
    Bail(Bailout),
}

pub struct Nop;
impl Tier for Nop {
    #[inline(always)] fn safepoint(_: &mut Request, _: FrameRef, _: Site) -> Flow { Flow::Continue }
}

pub fn execute<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Bailout>;

/// Run exactly one handler, the one for the opline at `frame.opline`, with `Nop` at the
/// safepoints, and report what it did. An exception the handler raised is left pending on
/// `req` and is not dispatched: `step` never runs `HANDLE_EXCEPTION`, so one call is one
/// handler and never an unwind. A fused compare-and-branch handler is one handler over two
/// oplines (design §5.8), and `Same` then names the opline after both.
pub fn step(req: &mut Request, frame: FrameRef) -> Result<Stepped, Bailout>;

pub enum Stepped {
    /// The same frame; continue at `frame.opline`.
    Same(FrameRef),
    /// The opline pushed and activated a callee: its `RECV*` prologue has run, `Site::Entry` was
    /// raised with `Nop`, and `frame.opline` is the callee's first body opline. The loop would
    /// continue in it.
    Pushed(FrameRef),
    /// The opline popped `frame` and the loop would continue in its caller.
    Popped(FrameRef),
    /// The opline moved `frame` to the heap (`GENERATOR_CREATE`) and the loop would continue in the caller.
    Moved(FrameRef),
    /// The handler left an exception pending. `Threw` wins over a push or a pop the same handler
    /// did (a `RECV` type error during activation; a destructor throwing during a `RETURN*`), and
    /// its frame is the current frame after that push or pop: the frame `HANDLE_EXCEPTION` runs in.
    Threw(FrameRef),
    /// The opline popped the entry frame of the enclosing `execute`; there is no frame to continue
    /// in. An exception may be pending: php's leave helper returns to the native caller with
    /// `EG(exception)` set, and so does this.
    Done,
}
```

What a caller must know, beyond the signatures.

*Ordering.* `safepoint` runs inside two handlers and one activation: a backward `JMP*`, after operand reads and before the jump; `RETURN*`, after operand reads and before the pop; and the activation of a user frame by the `DO_UCALL` and `DO_FCALL*` handlers, and by `execute` for its own entry frame, after the `RECV*` prologue and before the first body opline. `step` runs one handler and nothing else, so a caller that wants safepoint behaviour during stepping reads the words itself (§4.1), and a caller that wants exceptions dispatched hands a `Threw` frame back to `execute`. `Site::Return` is never raised for the entry frame of an `execute`: the `RETURN*` handler compares `frame` with the stack's entry slot before calling the tier, so no adapter has to. `Site::Entry` is raised for the entry frame of an `execute`, as php's counter fires whatever the caller is; a trace that starts there ends at that frame's return (§4.1).

*The prologue.* Activation is the call kernel's: it pushes the frame, positions `frame.opline` as php's `i_init_func_execute_data` does, skipping the whole `RECV*` run when the function has no typed parameters and was passed no more arguments than it declares, and starting at opline zero otherwise (`zend_execute.c:4426-4437`), runs the remaining `RECV*` handlers in a loop bounded by `OpArray.safepoints[0]`, and, if none raised, calls `T::safepoint(req, callee, Site::Entry)` and reports `Pushed`. If a `RECV*` raised, the exception is pending, `Entry` is not raised, and the report is `Threw`; php's counter on a typed function sits on opline zero and would have counted that call (design §5.6), which is the registry's third `trace` row (§4.1). A generator frame is activated the same way before `GENERATOR_CREATE` moves it, and is not activated again on resume. The comparison per prologue opline is paid once per call and never per body opline.

*Invariants a `Resume` must satisfy.* The resume frame is on the VM stack (frames never move, design §5.6); `frame.opline` is an opline of that frame's op array; every slot the tier wrote holds a well-formed zval with its type byte; a pending exception, if any, is on `req` exactly as a kernel would have left it; and the request's interrupt flag, if set, has not been consumed, because the loop consumes it after the `Resume`. `execute` returns `Ok(())` exactly when the entry frame has returned; there is no second meaning.

*Error modes.* `Bail` carries the same `Bailout` a kernel returns; there is no other error.

*Cost.* `Nop` is one inlined function returning a constant; the branch on `Flow` folds; `Stepped` is a discriminant the loop already has in a register; `Site`'s payload is a `u32` the handler already loaded. The interrupt check after a `Resume` is one load per exit, never per opline.

*The entry frame.* What the loop needs at `RETURN*` is to know whether the frame returning is its entry frame. That is a slot on the VM stack:

```rust
// php_heap, heap.lock items
impl VmStack {
    /// The entry frame of the innermost `execute` on this stack. Per fiber, because the stack is.
    pub fn entry(&self) -> FrameRef;
    /// Pushed by `execute` on entry. The guard pops on drop, so an `Err(Bailout)` pops too:
    /// shutdown functions and destructors run `execute` again after a bailout.
    pub fn push_entry(&mut self, f: FrameRef) -> EntryGuard<'_>;
    /// The frame the loop is executing.
    pub fn current(&self) -> FrameRef;
}
```

Design it twice on where the fact lives: a bit in every frame header is written once per `execute` and read once per return, and costs a bit the heap does not otherwise need; a field on `Request` is the same two writes but must be saved and restored on every fiber switch, because two fibers are two `execute` nestings; a slot on the VM stack is the same two writes and is per fiber because fibers own their stack pages (design §5.11). The stack is chosen. A generator resumed by a nested `execute` is a heap frame; the slot holds its `FrameRef`, which the running resume keeps alive, and frames never move while they live.

### 2.3 How the tier reaches `execute` without a generic `Engine`

`execute` is called at request start and at every native-to-PHP re-entry (`__toString`, `usort` callbacks, destructors, autoloaders), and the re-entry sites are non-generic `api` code. So the choice is bound once:

```rust
pub type ExecuteFn = fn(&mut Request, FrameRef) -> Result<(), Bailout>;

impl EngineBuilder {
    /// Default is `Nop`. Binds `execute::<T>` for this engine's lifetime and records
    /// `T::extension`; `build()` appends what it returns after the bundle's extensions,
    /// so `.tier()` and `.extensions()` may be called in either order.
    pub fn tier<T: Tier>(self) -> Self;
}
```

One indirect call per entry and per re-entry, never per opline; php pays the same call through `zend_execute_ex`, and refuses to JIT when something else has taken that pointer (§A.11). The loop body is fully monomorphised. The tier's state is built and dropped in the same order and by the same code as every extension's (design §4.4, §7.1): last at startup and request start, first at request end and engine drop. A tier bound to an engine can therefore not lack its state.

### 2.4 Adapters, and the deletion test

| Adapter | Where | `safepoint` | `extension` |
|---|---|---|---|
| `Nop` | `php_engine::tier` | `Continue` | `None` |
| `Jit` | `php_jit`, public | `dispatch::safepoint` (§4.9): under `Mode::Off`, `Continue` before any word; otherwise the one machine over the `Func` word | `JitEngine` and `JitRequest` (§4) |
| `Probe` | `php_engine` tests only | pushes `(site, number, func)` to a `Vec` in a test request state | that request state |
| `Debugger` | `sapi/phpdbg`, through `php_sapi`'s re-export of the face | the gate: `Continue` unless a breakpoint is in the frame at `Site::Entry`, the session is stepping, or a watchpoint is live; then a loop over `step` that checks breakpoints, stepping and watchpoints between handlers and hands back with `Flow::Resume` | the debugging session's state |

**The debugging host is a `Tier` too** (revision 8, #1308): its `safepoint` is the gate and `step` is its loop, as the recorder's is (§4.1). php's phpdbg replaces `zend_execute_ex` with a loop of its own (`Z/sapi/phpdbg/phpdbg_prompt.c`), which is principle 1's second loop; here `execute::<Debugger>` is the same loop over the same kernels, instantiated only in the `phpdbg` binary, and `execute::<Nop>` pays nothing for it. php's phpdbg compiles the program at its prompt and executes it only at `run`, so `Debugger` declares `Tier::HOLDS` (false by default): `execute`, php's `zend_execute`, then raises no observer begin for the main frame, and the host raises it with `tier::begin` at `run`, so a program never run is never begun and never ended; `Request::push_frame` raises none (revision 16). A watchpoint is compared between steps and not found by page protection: php's `mprotect` handler only marks the page it faulted on and compares at the next opline boundary (`phpdbg_watch.c:47-51`, `phpdbg_print_changed_zvals` from `phpdbg_prompt.c:1773`), so the report is the same opline with the same old and new values, and a process-wide fault handler has no place in an engine whose requests share a process on threads. Page protection can come later as a filter in front of the same comparison without touching the seam. The debugger's policy lives with its host: `php_sapi` re-exports the tier face (`Tier`, `Site`, `Flow`, `step`, `Stepped`, the stack the host steps and the writes it makes) as locked items, since ADR-0008 keeps `sapi/*` off `php_engine`; a host reads frames, op arrays and the linked tables through the native face from its own tier, as an extension does, and `tier.lock` keeps the mechanism, the writes and nothing that reads (revision 15).

Two production adapters make the seam real by the two-adapter rule; `Probe` is how the interpreter's safepoint placement is asserted, and it is the first user of `Tier::extension`, which is what tests the registration path in `php_engine` before `php_jit` exists.

Deletion test. Delete `Tier` and either `php_engine` checks a `cfg(feature = "jit")` word in three places and depends on `php_jit` (a cycle), or the check is a `dyn` call per back-edge (the door cost, §B.9). It earns its keep.

Month one. `execute::<Nop>` is the shipped loop. `step` exists from day 1 because the recorder, the unknown-opline path and, before either, `xtask`'s single-step debugging of a witness all want it; it is the `match` body called once, and its cost is nobody's but its callers'. The prologue-at-activation shape is month one's too, because the interpreter needs it to raise `Entry` at all.

---

## 3. The engine-level state word

### 3.1 Owner and count: design it twice

`php_engine::Func` is the owner, over the op array and a side table (revision 1 §3.1's argument: allocation at publication beside the `MapPtr` offsets the engine already assigns; one load at a safepoint, since `Func` is what the frame already holds; mutable-by-atomics state never enters the immutable unit format). One sentence three modules depend on: the unit cache stores `Published`, the engine-owned value that holds an `Arc<Unit>` beside the `Func` for each of its op arrays and the map-pointer offsets assigned at publication; a request holds an `Arc<Published>` for every unit it fetched, and a frame's `FuncRef` names a `Func` inside one of them or, for a request-scoped op array, a `Func` in request memory (§3.2). `Func`, and so the words, live in `Published` and not in the unit, which is what ADR-0001 requires.

| | (A) One word per opline | **(B) Chosen: one word per safepoint, numbered by the compiler** | (C) One word per safepoint, found by search |
|---|---|---|---|
| Words per `Func` | `oplines.len()`; three kinds of opline ever non-zero | `safepoints.len()`: loop headers, the entry, and returns | the same |
| How a safepoint finds its word | `words()[frame.opline_idx()]` | the entry is number zero; every backward jump to one header carries that header's number, and a `RETURN*` carries its own, in a field php leaves unused for that opcode, named per opcode in `ops.toml`; the handler passes it on `Site` | binary search of `safepoints` by opline index at every back-edge |
| What the unit learns | nothing | `OpArray.safepoints: Box<[OplineIdx]>`, the headers, the entry and the returns, immutable, produced by the compiler; one `u32` in an unused field | `OpArray.safepoints` |
| Op-array identity (design §6.1) | unchanged | unchanged: `Unit::dump` prints operands by the opcode's kinds from `ops.toml`, and an unused field is not printed; the field is compared as zero by `compile-identity` and as its number by nothing | unchanged |
| Cost at a safepoint | one indexed load | one indexed load from a field already in the opline | a search |
| Memory per engine | 4 bytes × oplines | 8 bytes × safepoints, plus 4 bytes × safepoints in the unit | 8 bytes × safepoints |

(B) is chosen. Which oplines are safepoints is a fact about the op array (a loop header is a backward jump target, the entry is the opline after the `RECV*` prologue, a return is a `RETURN*`), so it belongs in `php_unit` beside the line tables, under ADR-0001's rule that the unit format is a leaf below both the compiler and the engine; the mutable words stay in `Func`. The compiler numbers headers, not back-edges: a `foreach` with a `continue` has two backward jumps to one `FE_FETCH_R`, and php keeps one counter at the header (§A.3), so both jumps carry the header's number and one root is recorded, not two. `ops.toml` has one column, `safepoint`, naming the field for the opcodes that are sites: `result` on the backward jumps `pass_two` leaves, `JMPNZ` and `JMP`, on `JMPZ`, which is backward only after the optimizer's `INV_COND` (`zend_emit_cond_jump` does not invert on `BOOL_NOT`; `do { } while (!$x)` is `BOOL_NOT` then a backward `JMPNZ`), and on `RETURN`, `RETURN_BY_REF` and `GENERATOR_RETURN`, all six verified to leave `result` unused in every handler body and every emission at 8.5.9 (`RETURN_BY_REF` reads `extended_value`, which is why `result` is the field; design §5.7 has the full list of forward-only jumps). `FAST_RET` jumps to an earlier index and is not a back-edge. **The compiler asserts the invariant on every op array it emits**: every backward jump's opcode other than `FAST_RET` has a `safepoint` column and its target is a numbered header; a violation is a compile error in `php_compile`'s own tests and a failure of the `compile-identity` corpus sweep, never a loop that silently does not compile. The `Site::Entry` activation reads word zero and no field, which holds for a function with no `RECV*` whose entry is opline zero, and for a function whose only opline is a `RETURN`, whose entry word is number zero and whose return word is number one. A function with no loop and one return has two words.

### 3.2 Interface, and the second home

```rust
// php_engine::tier. Allocated by Engine::publish inside Published, one per safepoint of the op array, all bits zero.
#[derive(Default)]
pub struct Word { halves: [AtomicU32; 2] }
impl Word {
    pub fn halves(&self) -> &[AtomicU32; 2];   // the one accessor; the meaning of the bits is php_jit::policy's
    /// A word no tier ever acts on: all ones. Allocated instead of zero for every safepoint of a
    /// request-scoped op array (`eval`, an uncached include, a closure declared in one).
    pub fn sealed() -> Word;
}
#[derive(Clone, Copy)] pub struct SafepointIdx(pub u32);   // 0 is the entry
impl SafepointIdx { pub const ENTRY: Self; }
impl Func {
    pub fn word(&self, i: SafepointIdx) -> &Word;           // i < op_array.safepoints.len()
    pub fn published(&self) -> Option<&Arc<Published>>;     // None for a request-scoped op array
}
```

`php_engine` knows this much and no more: a `Func` carries one `Word` per safepoint, zero at publication or all ones when request-scoped, two atomics, never read by `Nop`. `Word` is `Default` because `php_jit::table` builds one per exit of every published trace. It is not `#[repr(C)]`, because compiled code never touches a word: every bump is `dispatch::visit` in Rust. The layout is `php_jit::policy`'s and is the same in both homes:

| Half | Bits | Meaning | Written by | How |
|---|---|---|---|---|
| `halves[0]` | 32 | count of visits while `cold`; saturating; reset to zero when a claim settles as `Abort`, as php's counter resets when it fires (§A.3) | `dispatch::visit` for a `Func` word and for an `Exit` word; `policy::settle` | relaxed load, add, relaxed store; lost updates accepted (§A.3, §B.3) |
| `halves[1][0..2]` | 2 | `cold` 0, `recording` 1, `compiled` 2, `blacklisted` 3 | `policy::claim`, `policy::settle` | compare-and-swap for `cold → recording`; release store for the rest |
| `halves[1][2..8]` | 6 | failed attempts (php's 16 fits) | `policy::settle` | same store as the transition that counts it |
| `halves[1][8..32]` | 24 | when `compiled`: a `TraceId`, the same number in either home | `policy::settle` | the release store that publishes |

The sealed word is the one bit pattern `php_engine` knows: all ones decodes as `blacklisted` with attempts and id saturated, so `visit` returns `Continue` for it on the same arm as any blacklisted word, and no code in `php_jit` names the case. That `blacklisted` is the all-ones state is a layout invariant of `policy` with its own test (§4.7). A request-scoped `Func` therefore costs its words and nothing else, `Func::published()` is read only inside the holder closure a claim would build, and a claim is never built on a sealed word.

The two-halves argument stands, at 32 bits each: a racy store of a stale count touches only `halves[0]`. A `TraceId` is one number; a `Func` word names the root it enters and an `Exit` word names the side trace it enters, and neither `enter` nor `dispatch` composes anything. php's 1,024 roots and 128 sides per root are `Policy` limits that `table.publish` enforces in `Counts`; 24 bits hold sixteen million traces and are never the bound.

Ordering, stated as the contract it is. `policy::read` is a relaxed load, so a cold back-edge on aarch64 pays no `ldar`. `table.get`'s slot load is acquire and pairs with the release store `publish` makes after the code, the exits and their zero words are complete, so a thread that finds the slot filled finds the trace whole. A relaxed read of `compiled(id)` does not by itself guarantee that the same thread's slot load finds the slot filled, because the two locations are ordered only on the writer; what makes the design sound is `dispatch::run`'s rule that a missing slot is "not compiled", which returns to the interpreter at the site with nothing run (§4.9). In practice the window is the length of one release store and is never observed on the two targets, and the escape, if G5 ever names it, is an acquire load on the `compiled` arm alone.

Only safepoint oplines have a word, so every word belongs to one of three kinds. Cost per engine: 8 bytes per safepoint plus 4 bytes per safepoint in the unit's `safepoints` table, shared by every request, plus 8 bytes per exit per published trace; G4 sees both in RSS. A word while cold is written by every thread that visits it, so a loop header hot on eight request threads at once is a contended line for the length of the threshold and a shared read-only line afterwards; the threshold bounds the contention, and `Mode::Off` writes nothing (§4).

### 3.3 Per-request, engine, or both

| | Per request: `MapPtr<[Feedback]>` (r2 §5.14) | **Engine: `Word` in `Func` (chosen)** | Both: heat per request, state in the engine |
|---|---|---|---|
| Allocation | one map-table entry per op array per request on first touch, before any heat exists (§F.6) | one allocation per publication | both |
| Can hold "is this compiled" | no: a request cannot see another's compile | yes | yes |
| Cross-request heat | none; php resets per request too (§A.3) but keeps its traces | accumulates; requests shorter than 127 calls still reach a compile | none |
| Cost at a safepoint | one map-base load plus an indexed load | one load | two |
| Decision criterion | G1 within the drift bound and F.3 shows every root heats within one request | G1 within the drift bound | G1 fails on the engine word alone, or F.3 shows per-request heat is enough and the map entry is cheaper |

The engine word. The design of record deleted r2 §5.14's feedback sentence (design ledger 7); type feedback is the trace record plus the run-time cache (§E.3). The third column is the written fallback, chosen by F.3's measurement on the Laravel skeleton under FastCGI (does any root cross `opcache.jit_hot_func` inside one request), not by argument.

---

## 4. `php_jit`'s modules

| Module | Seam | Interface (items a caller learns) | Behind it | `unsafe` |
|---|---|---|---|---|
| `lib` | external | `Jit: Tier` | the state types | no |
| `dispatch` | internal | `safepoint(req, frame, Site) -> Flow`; `visit(req, frame, &Word, holder, Start) -> Next`; `run(req, frame, TraceId) -> Flow`; `Next` | the one state machine over a word, the trace-to-trace loop, `hot`, the recording guard | no |
| `record` | internal | `run(req, frame, Start) -> Result<Recorded, Bailout>`; `Recorded { record: TraceRecord, view: UnitView }`; `Start`, `Stop`; `dump`/`parse` | the loop over `step`, type sampling, php's stop list, depth and length limits, aliasing marks, the owned view | no |
| `types` | internal | `build(&TraceRecord, &UnitView) -> Result<TraceIr, Abort>`; `ExitSpec`, `Landing`, `ExitId` | trace SSA over slots, lattice, guard placement, `SMART_BRANCH` refusion, the clobber rule | no |
| `lower` | internal, real (two consumers of `TraceIr`) | `Backend::new(&Policy) -> Result<Backend, Unsupported>`; `Backend::compile(&TraceIr) -> Result<Compiled, Abort>`; `Compiled`; `dump::render(&TraceIr) -> String` | the ISA and its flags, CLIF construction, regalloc, exit blocks, the interrupt test, the one kernel-call emitter, the opline marks | no |
| `table` | internal | `TraceTable::new`; `publish(Compiled, Start) -> Result<TraceId, Full>`; `get(TraceId) -> Option<&Trace>`; `Trace`, `Exit`, `TraceId` | the compile lock, the arena, one segmented id space, the exit-counter and per-root bounds, the lifetime rule, the `PERF` and `PERF_DUMP` writers | no |
| `code` | internal, private to `table` | `Arena::new`, `Arena::publish(&[u8], align) -> Result<Code, Full>`; `Code::entry()` | dual-mapped arena, `memfd` and `MAP_JIT`, cache maintenance, chunk reuse | yes |
| `enter` | internal | `enter(req, frame, &Trace) -> Result<Exited, Bailout>`; `Exited { exit: ExitId, frame: FrameRef }`; the `kernels!` macro; `Status` | the trampoline, `ExitCtx`, the raw-pointer crossing, panic catch and resume, map-pointer first touch, the landing | yes |
| `compiler` | internal | `Compiler::new(Arc<Shared>)`; `submit(Job)`; `drain()`; `Job { recorded, claim }`; `pipeline(&Shared, Job) -> Outcome` | the queue, the worker, the inline mode, build, lower, publish, settle, every failure mapped to an `Outcome`, the dump events | no |
| `policy` | internal | `Policy::from_ini`; `Policy::mode()`; `Policy::threshold(Start)`; `read`, `bump` over `&Word`; `claim(Holder, &Policy, Start) -> Option<Claim>`; `Claim`, `Holder`, `settle(Claim, &Outcome)`; `INI`; `dump(sink, event)` | thresholds, `opcache.jit*` parsing, the state machine, the `jit_debug` vocabulary | no |

`php_jit`'s line ceiling is its `[[crate]]` row in `seams.toml`, which `xtask budget` holds and no design document restates. `php_jit` is `#![deny(unsafe_code)]` with `#[allow(unsafe_code)]` on `code` and `enter`. Externally the crate exports one item. The tier's state:

```rust
impl Tier for Jit {
    #[inline(always)]
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow { dispatch::safepoint(req, frame, site) }
    fn extension() -> Option<Extension> {
        Some(Extension::new("jit")
            .ini(policy::INI)                      // php's opcache.jit* names, plus jit.mode and jit.compile
            .engine_state(JitEngine::new)          // fn(&StartupIni) -> JitEngine
            .request_state::<JitRequest>())
    }
}
pub(crate) struct Shared { policy: Policy, table: TraceTable, backend: Option<Backend>, log: DiagnosticsSink }
pub(crate) struct JitEngine { compiler: Compiler, shared: Arc<Shared> }            // field order: the worker is joined before the table drops
#[derive(Default)] pub(crate) struct JitRequest {
    touched: BitSet,        // one bit per TraceId, first touch of the trace's map pointers
    // The recording exclusion is not here: it is the running context's trace number, `Request::trace_num`,
    // engine storage the fiber switch saves and a fiber's body starts at zero (revision 9)
}
```

`JitEngine` holds no lock of its own; the one lock is inside `TraceTable`, and the compiler's queue is a second, private one that no safepoint takes (§4.8). `Shared` exists because the worker thread needs the table, the policy, the backend and a sink by `Arc` and `JitEngine` is owned by the engine, not the other way round; `log` is the engine's diagnostics sink, because the worker has no request to write to. When `Backend::new` or `TraceTable::new` fails at `JitEngine::new`, or `opcache.jit_buffer_size` is zero, or `opcache.jit` is off, the tier runs with `Mode::Off`: `dispatch::safepoint` reads the mode and returns `Continue` before touching any word, no worker is spawned, `backend` is `None` and never read, and one line is logged at startup (that php does the same for an unsupported `opcache.jit` is unverified). `req.engine::<JitEngine>()` is one pointer load and `mode` and `threshold` are one field load each; if G1 names the pair, the mode and the four thresholds are copied into `JitRequest` at request start. The tier is not an extension: it is not under `ext/*`, does not depend on `php_ext`, and is not in the bundle's list. It uses the extension mechanism for its state because design §4.4 is the one mechanism for per-engine and per-request state and a second would fail its own deletion test.

The lock, stated where the rule lives. Design §4.4's "`E` may not contain a lock" names the trace table and the compiler's queue as the two engine-owned exceptions beside the unit cache; both are reached through the tier's own state and never through an extension's.

### 4.1 `record`

```rust
pub(crate) enum Start { Loop, Enter, Return, Side { parent: TraceId, exit: ExitId } }
pub(crate) struct TraceRecord { start: Start, root: FuncRef, events: Vec<Event>, stop: Stop }
pub(crate) struct Recorded { record: TraceRecord, view: UnitView }
pub(crate) enum Event {
    Op { func: FuncRef, opline: OplineIdx, op1: Sample, op2: Sample, data: Sample, branch: Option<bool> },   // `branch` for a fused compare-and-branch
    InitCall { target: FuncRef },
    Enter { func: FuncRef },                 // a PHP-to-PHP call followed
    Back,                                    // its return
}
#[derive(Clone, Copy)] pub(crate) struct Sample { tag: Tag, packed: bool, reference: bool, indirect: bool, class: Option<ClassId> }
pub(crate) enum Stop { Loop, Link(TraceId), Return, RecursiveCall, RecursiveRet, Interpreter, Trampoline, PropHookCall, BadFunc,
    NotSupported, Exception, TooLong, TooDeep, TooDeepRet, DeepRecursion, LoopUnroll, LoopExit, RecursionExit, InnerLoop, CompiledLoop,
    TooManyTraces, TooManyChildren, TooManyExits, CompilerError, MovedFrame, RequestUnit }
pub(crate) fn run(req: &mut Request, frame: FrameRef, start: Start) -> Result<Recorded, Bailout>;
impl TraceRecord { pub(crate) fn dump(&self) -> String; pub(crate) fn parse(s: &str) -> Result<Self, ParseError>; }
```

`run` is one loop. Before each opline it samples the three operands from the frame, following `IS_INDIRECT` and `IS_REFERENCE` and marking both, and pushes one `Op`; then it decides whether to step at all: it stops with `NotSupported` at `CATCH`, `FAST_CALL`, `FAST_RET`, with `Interpreter` at megamorphic calls and `INCLUDE_OR_EVAL`, with `MovedFrame` on a heap frame. It reads the `Func` word at the loop header, entry or return it has reached (the word is at the number the opline carries, or number zero for an entry): `compiled` at a loop header inside the trace stops with `CompiledLoop`, and `compiled` at a root it reaches stops with `Link(id)`, as php does at a handler that is a trace (§A.4); the id becomes the `link` of the trace's last exit (§4.2). A fused compare-and-branch is one `step` and one `Op`, with the direction taken in `branch`, as php's record keeps the branch on the comparison (unverified against `zend_jit_trace_record`). Then it calls `step` and matches:

| `Stepped` | The recorder |
|---|---|
| `Same(f)` | continues at `f`; at the root opline after a `Loop` start, closes the trace |
| `Pushed(f)` | tries `view.hold(req, f.func())`; a request-scoped op array cannot be held and stops with `RequestUnit`; otherwise pushes `Enter { func: f.func() }`, depth + 1, and reads `f`'s entry word |
| `Popped(f)` | pushes `Back`, depth − 1. At depth zero the root frame returned: for a `Loop` or `Enter` start the trace ends after it, and the lowering places php's IP guard on the unknown caller (§A.5); for a `Return` start this first `Popped` is the start's own return, recorded as the trace's first opline, `f` becomes the depth-zero frame, the IP guard is placed at the same point, and recording continues in the caller |
| `Moved(f)` | stops with `MovedFrame` |
| `Threw(f)` | stops with `Exception`; `hot` hands `f` back to `execute` through `Resume` |
| `Done` | cannot occur: see below |

The rule about an entry frame, stated with the stack's slot: a `RETURN*` at depth zero is stepped only when the root frame is not `req.stack().entry()`, because when it is, the caller is native and there is nothing to follow; and a `Return` start never starts on an entry frame because `Site::Return` is never raised there (§2.2). So `Done` is unreachable from the recorder, and the arm is `unreachable!()` with that sentence beside it. Before stepping the recorder reads nothing it needs afterwards, because `Stepped` carries the frame to continue in and the arm says what became of the old one. A native call that re-enters the interpreter through a nested `execute` returns `Same` and is opaque, as php's `DO_ICALL` is; whatever that nested `execute` did at its own safepoints, it did not record, because `JitRequest.recording` is set for the length of this `run` (§4.9). After each step it checks the length limit and stops with `TooLong`. A `Bailout` from `step` ends recording with no record and is the `Err` of `run`. The buffer is php's flat record (`zend_jit_internal.h:365-373`), one `Vec` bounded by `opcache.jit_max_trace_length`.

The owned view. `record::run` builds `Recorded.view` as it goes through the tier face's builders (§5.3): `hold(req, f)` accepts an op array whose unit is published and returns `false` for a request-scoped one, holding nothing. `Recorded.view` holds the request's epoch once. `hold_class(req, c)` holds nothing and accepts a class whose `ClassId` is published (`ClassId::is_published`), since a published row lives as long as the epoch (design §5.14 *Identities*). A request-scoped class is one whose `ClassId` is request-local: its link read a request-owned class, or the cache did not hold its unit (design §6.2). It is recorded as its id and guarded through the request's cache slot, and `UnitView::class` is defined only for a class `hold_class` accepted. `UnitView` is therefore a value that holds the request's epoch and the ids it accepted, and is `Send`; `types::build` needs nothing else, and runs on the compiler's thread (§4.8).

What recording costs, per opline, is one call into `step` and the `match` it runs; G7 reports it.

Tested: `dump` and `parse` are inverses, so a recorded trace is a fixture; `run` runs on a real request under `Capture` with `jit.mode=force`, and its `TRACE start/stop` lines are diffed against the oracle's on the seven kernels through `oracle::compare` with the divergence registry (`docs/divergence.toml`, kind `trace`). Three differences from php's traces are decided here, ahead of the recorder: a trace whose root is the entry frame of a nested `execute` stops with `Return` one opline earlier than php's (whether php's does the same at a native caller is unverified); when the root table fills this engine blacklists the one root that hit `Full` where php stops every counter handler at once (`zend_jit_stop_counter_handlers`, §A.4); and a typed function's call whose `RECV` throws is a root in php, whose counter is on opline zero, and not here, where `Site::Entry` is raised only after the prologue ran without raising (design §5.6, ledger 9). The third is a registry row today, because the interpreter already raises `Site::Entry` after the prologue. The first two have no engine half until `record` and `table` exist, and each enters the registry as an `open` row the first time the `TRACE` diff raises it, for a person to accept against that reading. The recorder's classification is a table over `Stepped`, and each arm is a fixture that drives `step` on a witness which takes that edge (a call, a return, a `yield` that creates a generator, a throw, a return trace, a `do { } while (!$x)` loop, a fused branch taken each way).

### 4.2 `types`

```rust
pub(crate) struct TraceIr { entry: Entry, values: Vec<Value>, ops: Vec<Op>, guards: Vec<Guard>, exits: Vec<ExitSpec>, map_ptrs: Vec<MapPtrRaw>, loop_header: Option<usize> }
pub(crate) struct ExitSpec { opline: OplineIdx, landing: Landing, stores: Vec<Store>, link: Option<TraceId> }
#[derive(Clone, Copy)] pub(crate) enum Landing { Plain, FreeTemps { op1: bool, op2: bool }, RestoreCall }
#[derive(Clone, Copy)] pub(crate) struct ExitId(u16);
pub(crate) fn build(rec: &TraceRecord, view: &UnitView) -> Result<TraceIr, Abort>;
```

Pure: a record in, an SSA over frame slots out, with php's guard set placed where a recorded type is narrower than what the op array alone implies (§E.5), aliased variables kept at guard strength (§A.4), `SMART_BRANCH` refused into one compare-and-branch whose mispredict exit lands on the branch opline with the comparison's result written as a constant (§E.5), and an `ExitSpec` per guard naming the slots that must be stored and the `Landing`. `loop_header` names the position the root's back-edge jumps to, for a `Loop` start; the lowering puts the interrupt test there (§4.3). `Landing` is decided at build time (whether a temporary is live at the exit opline is a fact about the op array; whether an `INIT_FCALL` sequence is open is a fact about the record), and a link is a build-time fact on the exit a `Link` stop ends on. **The clobber rule.** A kernel call whose opcode carries the `call` effect in `ops.toml` may run user code, and user code may write any array, object or reference the trace holds a derived fact about; after such a call every memory-derived value in the lattice (an array's length or packed flag, a slot's type where the slot is reachable through a reference or a symbol table, a property offset's class) is reloaded or re-guarded, and only register-resident scalars the trace itself owns survive. A kernel with the `throw` effect and no `call` effect clobbers nothing but may exit. `Abort` is the recoverable half of `Stop` and counts toward the blacklist. Tested from fixtures: `build` over a parsed record, `lower::dump::render` over the result, diffed against a golden file, and the golden files checked once against php's `TRACE_TSSA` dump for the same programs to fix the guard placement vocabulary; one fixture per clobbering kernel class asserting the reload.

### 4.3 `lower`: a real seam as a value, and the backend behind it

```rust
pub(crate) struct Compiled {
    bytes: Vec<u8>, entry_offset: usize, exits: Box<[ExitSpec]>, align: usize,
    marks: Box<[(FuncRef, OplineIdx, u32)]>,     // code offset at which each recorded opline's code begins; a trace inlines
                                                 // calls, so an opline is named with its function
}
pub(crate) struct Backend { /* private: the host ISA, its flags, the kernel symbol table */ }
impl Backend {
    /// Detects the host ISA through cranelift-native and applies the flags `Policy` asks for.
    /// `Unsupported` when the host has no backend; the tier then runs with `Mode::Off` (§4).
    pub(crate) fn new(policy: &Policy) -> Result<Backend, Unsupported>;
    pub(crate) fn compile(&self, ir: &TraceIr) -> Result<Compiled, Abort>;
}
pub(crate) mod dump { pub(crate) fn render(ir: &TraceIr) -> String; }
```

The seam is the value `TraceIr`, not a builder trait, and `dump::render` is its second real consumer; a builder trait wide enough to hide Cranelift's `FunctionBuilder` is nearly as wide as Cranelift and is Dax's `LowerHost` again. `Backend` is built from `Policy` and holds the `OwnedTargetIsa` and its flags, so no module but `lower` names a Cranelift type, and a Cranelift version bump is one module's change. `Backend` is `Send + Sync` because it holds no per-compile state (that `OwnedTargetIsa` is both is unverified here); each `compile` builds its own `Context`. The §D fallback, the IR backend, is a second body of `Backend` under a feature, behind the same two functions and the same `Compiled`; an LLVM backend would be a third (`research/llvm-research.r1.md` D.1) and is not built before G8 has a reading.

What the code is. Frame slots are loads and stores at `frame + const`; recorded scalars are `i64`/`f64` SSA values; each guard is `icmp` and `brif` to a cold block that stores the live scalars with their type bytes and returns its `ExitId` (§E.6); kernel calls go through one private emitter, `call_kernel(builder, symbol, args, exit)`, that emits the indirect call to the 64-bit constant, tests the returned `Status`, and branches to the exit block when it is non-zero, so a new line in `kernels!` needs no mirrored rule here. `Compiled.bytes` are final and hold kernel addresses as 64-bit constants; a 32-bit host is `Unsupported`. **The interrupt test.** For a trace with a `loop_header`, the lowering loads the interrupt pointer from `ExitCtx` once at entry and, at the header, loads the byte it points to and exits when it is non-zero, through an `ExitSpec` at the header opline with `Landing::Plain`; the loop then performs the interrupt check under the `Resume` contract (§2.2) and the interrupt helper runs as it would have at the back-edge. One load and one branch per iteration, which is php's cost (`rfc-jit.md:118`); a trace without a loop has no test, because it runs a bounded number of oplines before the interpreter's own check, and a recursive trace is bounded by `opcache.jit_max_recursive_calls`. Links and side traces are not jumped to but returned as exits, and the trampoline dispatches (§4.9), which keeps bytes final; whether `return_call` in the `Tail` convention pays for itself is F.7.

**Observed calls** (revision 11; design §4.3 seam 9). Compiled code emits observer events where php's JIT does (`Z/ext/opcache/jit/zend_jit_ir.c:4620-4717`): when an observer was declared at engine build, at every compiled call, return and native call, inlined ones included, it reads the callee's resolved slot at run time and calls begin unless the slot reads none-observed, and calls end when the frame is the current observed one; an observed call keeps its frame. When none was declared, nothing is emitted.

**Observed opcodes** (design §7.1). When an extension declared an opcode other than `ZEND_BEGIN_SILENCE` and `ZEND_END_SILENCE`, the tier does not start for that engine, bound `Mode::Off` where the tier is bound, and php's warning is raised through the startup diagnostics, as `zend_jit_startup` refuses (`Z/ext/opcache/jit/zend_jit.c:3743-3756`); at those two, compiled code tells the declared opcode's observers before the kernel. When none was declared, nothing is emitted.

Targets. `jit` is a default cargo feature on Linux and macOS for x86-64 and aarch64, the two platforms `code` has a `Pages` adapter for (§4.4); "which hosts" within them is answered by `Backend::new`, not by a feature list, and another operating system is a third `Pages` adapter if ever wanted. How Cargo expresses it: `php_jit` is an optional target-specific dependency of `php_bundle`, and the `Tier` alias is gated on `all(feature = "jit", any(the four targets))`, so the feature on a fifth target is `Nop` and not a build failure.

`marks` is what a profiler needs and what a debugger of a bad trace needs: `table` turns it into php's `PERF` map line and `PERF_DUMP` record (§4.5), and `xtask` prints a disassembly annotated by function and opline from the same table. It costs three words per recorded opline in `Trace` and nothing in the code.

Tested at the seam: CLIF text of `compile` for each fixture is a golden file; the compiled bytes run under `force` against the oracle; `Backend::new` on a host with no backend is a unit test with a stubbed ISA lookup; the interrupt test is one line of the golden for every loop fixture.

### 4.4 `code`: the arena, sealed by construction

```rust
pub(crate) struct Arena { /* one region, mapped twice; chunks of 64 KiB carved inside it */ }
pub(crate) struct Code { entry: NonNull<u8>, len: usize, chunk: Arc<Chunk> }   // entry is in the execute view
pub(crate) type Entry = unsafe extern "C" fn(*mut ExitCtx, *mut Frame) -> u32;

impl Arena {
    pub(crate) fn new(capacity: usize, map_jit: MapJit) -> io::Result<Arena>;
    /// Copies `bytes` into a chunk through the write view, performs the cache maintenance the
    /// target needs against the execute view, and returns a handle into the execute view.
    /// Bytes are final: there is no API to write to a `Code` again.
    pub(crate) fn publish(&mut self, bytes: &[u8], align: usize) -> Result<Code, Full>;
}
impl Code { pub(crate) fn entry(&self) -> Entry; }
```

Design it twice on the protection rule:

| | (A) One mapping, flipped RW around each publish | **(B) Chosen: two views of the same pages** | (C) One mapping, written only before it is ever executable |
|---|---|---|---|
| macOS, `MAP_JIT` | sound: `pthread_jit_write_protect_np` is per thread | the same call; the two "views" are one address and the per-thread toggle, so the shape is one | sound |
| Linux, `mprotect` | unsound: protection is per mapping, so for the length of a compile the chunk is RW for every thread, including one executing a trace in it | sound: the execute view is `PROT_READ | PROT_EXEC` from `new` and is never remapped; the write view is `PROT_READ | PROT_WRITE`, is held only by the compile lock's owner, and is never executed | sound |
| Tail of a chunk | usable | usable | wasted once the chunk is sealed |
| Mappings | one | two | one |
| What the test can show | nothing: `HeapPages` has no protection | thread A loops in a trace in chunk 7 while thread B publishes into chunk 7, natively, on both targets | |

(B) is chosen. On Linux the arena is one `memfd_create` file of `capacity` bytes, created with `MFD_ALLOW_SEALING` and sealed against shrinking and growing, mapped twice as a whole with `MAP_SHARED`, once RW and once RX, and chunks are carved inside the region. On macOS it is one `MAP_JIT` anonymous mapping, the one-region rule of §B.8, and the per-thread toggle around each write, which is the same shape with the two views at one address; there is no process-wide protection change after `new` on either platform. `HeapPages`, the test adapter, is one allocation of `UnsafeCell<u8>` handing out two raw pointers to it; it tests the arena's bookkeeping, which is all a heap adapter can. The internal `Pages` seam is `map_pair(len)`, one span and its two views, `write(at, bytes)` through the write view, and `sync(range)` and `rx(at)` on the execute view's addresses, with the same two adapters (revision 19). Cache maintenance, stated once: on x86-64 nothing; on aarch64 the data cache is physically tagged, so after the write through the write view one sequence on the execute view's addresses suffices, `dc cvau` per line, `dsb ish`, `ic ivau` per line, `dsb ish`, `isb`, done by an inline-assembly routine in `code` or by `wasmtime-jit-icache-coherence`, and on macOS by libSystem's `sys_icache_invalidate`, which `code` calls over the bytes written. Whether a thread that later enters a reused chunk needs its own `isb`: on macOS aarch64 it was not needed in #2741's test (macOS 15.6), where a body was entered on four threads and dropped, a different body was published at the same address of the reused chunk, and a thread that had never entered the chunk, waiting on an atomic pointer with no system call before its entry, read the new body in each of 500 rounds with its own `isb` and in each of 500 without, and again over five further runs of both; so `code` asks no `isb` of an entering thread on macOS. On Linux aarch64, with two aliases, it is still unverified and is settled by the same test on that target. The three lifetime rules are carried by types inside `table`: publication under the compile lock, no write after publication, code outlives every call. `Full` is php's `TOO_MANY_TRACES`. That the Linux mapping and sealing calls behave as described is unverified in the research note; §B.8 covers macOS only.

A stricter posture, in which the write view exists only between `publish`'s first and last store, is a `Pages` policy behind the same seam and is not the default: it costs two syscalls per publish and defends against an attacker who already has an arbitrary write, which is not a threat model the design claims. php leaves its whole buffer RWX under `PERF_DUMP` and `GDB` (`research/data/jit-research/php-src-jit-startup-protect.txt:46,121`); the dual map makes that unnecessary here. `MapJit` is a startup value from the policy.

Tested at the seam under Miri with `HeapPages`: chunk fitting, alignment, live counts, and that a `Code` dropped last frees its chunk. Natively, on both targets: `mov eax, 7; ret` on x86-64 and `mov w0, #7; ret` on aarch64 through the two views; the concurrent test above, a spinning trace in one chunk on one thread and a hundred publishes into the same chunk on another, asserting the trace's result on every iteration; and the reused-chunk test. On macOS aarch64 all three pass (#2741): `mov w0, #7; ret` returns 7, and the spinning body answered 7 on every one of its calls while the hundred landed and were entered.

### 4.5 `table`: the owner of every published trace

```rust
#[derive(Clone, Copy, PartialEq, Eq)] pub(crate) struct TraceId(u32);   // one space; < 2^24, the word's width

pub(crate) enum Kind { Trace /* , Region: month three, the same Entry ABI */ }
pub(crate) struct Trace {
    code: Code, id: TraceId, kind: Kind, start: Start, root: TraceId, exits: Box<[Exit]>, map_ptrs: Box<[MapPtrRaw]>,
    marks: Box<[(FuncRef, OplineIdx, u32)]>,
    links: Box<[TraceId]>,            // every trace an exit links to; an id is never reused within an epoch
}
pub(crate) struct Exit { opline: OplineIdx, landing: Landing, link: Option<TraceId>, word: Word }

pub(crate) struct TraceTable { /* one per epoch; read side: segments of slots, each written once under the lock and read without it;
                                  write side: compile: Mutex<(code::Arena, Counts, Segments)>, taken with PoisonError::into_inner */ }
impl TraceTable {
    pub(crate) fn new(policy: &Policy) -> io::Result<TraceTable>;
    /// Takes the compile lock, copies the bytes into the arena, builds one `Exit` per `ExitSpec`
    /// with a zero `Word`, resolves each link to its `TraceId`,
    /// assigns the next id, writes the `PERF` line and `PERF_DUMP` record if `jit_debug` asks, and
    /// publishes with a release store. Transactional: the arena cursor and `Counts` advance only
    /// after the copy, so a panic inside leaves the table as it was. `Full` is php's
    /// `TOO_MANY_TRACES`, `TOO_MANY_CHILDREN` or `TOO_MANY_EXITS` by which bound was hit.
    pub(crate) fn publish(&self, compiled: Compiled, start: Start) -> Result<TraceId, Full>;
    /// Lock-free: two loads, the second acquire. A slot is written once, so the borrow is the
    /// table's. `None` in the window §3.2 names, which means "not compiled" to the caller.
    pub(crate) fn get(&self, id: TraceId) -> Option<&Trace>;
}
```

The id space is one. A root and a side trace are numbered from the same counter; `Trace.root` names the root a side belongs to (itself, for a root), and `Counts` keeps php's three bounds, `opcache.jit_max_root_traces`, `opcache.jit_max_side_traces` per root, and `opcache.jit_max_exit_counters`, as numbers, not as widths. The read side is a segmented array, a fixed table of segment pointers each holding 1,024 slots, allocated on demand under the compile lock and read with two loads. A slot is written once and never cleared within its epoch, so `get` borrows the table, and entering a trace clones nothing and changes no count. A `Func` word's 24 bits and an `Exit` word's 24 bits hold the same kind of number, and `get` is the only reader of either. **`Kind` is the answer to the tier bit question** (`research/llvm-research.r1.md` D.9): a month-three region over hot traces is a table entry with the same `Entry` ABI, entered from the same word by the same id, and the word's layout does not change for it.

The lifetime rule. A trace's code embeds addresses from the units it inlined and the published classes it read, and all of them live as long as the epoch (design §5.14 *Identities*, §6.3). So the trace table belongs to the epoch. A request reaches only the table of the epoch it holds, and a restart starts the next epoch with an empty table, as `zend_jit_restart` resets php's buffer. The old table and its code are freed with the old epoch, once no request, job or claim holds it. No trace is freed alone. When a bound is reached (`opcache.jit_max_root_traces`, or a full code arena), the table stops the hot counters and compiles nothing more until the restart, as `zend_jit_stop_counter_handlers` does.

Observability. php's `opcache.jit_debug` bits `PERF` (0x10) and `PERF_DUMP` (0x20) (`research/data/jit-research/php-src-jit-header.txt:59-60`) are honoured with php's outputs: a line per trace in php's `/tmp/perf-<pid>.map` in php's symbol form, and a jitdump record with the code bytes and the `marks` as line entries with the function's file and line (that php's map path and jitdump file are as named here is unverified). Both are written by `publish` under the compile lock. Nothing is written when the bits are clear.

Tested at the seam under Miri with `HeapPages`: publish, get, ids across a segment boundary, `Full` for each of the three bounds, a full table that stops compiling, a new epoch's table that starts empty, a publish that panics and leaves `Counts` unchanged, and that a dropped trace returns its chunk. Natively, `mov eax, 7; ret` runs through `publish` and `get(..).code.entry()`. The `PERF` line is diffed against php's for one trace of one witness.

### 4.6 `enter`: the crossing

```rust
pub(crate) struct Exited { exit: ExitId, frame: FrameRef }
/// Touch the trace's map pointers on first entry for this request, call the code, resume a panic
/// a wrapper caught, return a bailout a wrapper caught, and otherwise land the exit: set
/// `frame.opline`, release the temporaries a `FreeTemps` landing names, restore an interrupted
/// `INIT_FCALL` sequence for `RestoreCall`, and read the current frame from the VM stack.
/// Reads no word and follows no link: what happens next is `dispatch`'s.
pub(crate) fn enter(req: &mut Request, frame: FrameRef, trace: &Trace) -> Result<Exited, Bailout>;

/// The first argument of every trace and every kernel wrapper. A local of `enter`. Compiled code
/// reads `req` at offset zero and `interrupt` at offset eight and nothing else; `pending` has no C
/// layout and is the wrappers' alone.
#[repr(C)] pub(crate) struct ExitCtx { req: *mut Request, interrupt: *const AtomicBool, pending: Option<Pending> }
pub(crate) enum Pending { Bailout(Bailout), Panic(Box<dyn Any + Send>) }

#[repr(transparent)] pub(crate) struct Status(u32);
impl Status { pub(crate) const CONTINUE: Self; const EXCEPTION: Self; const BAILOUT: Self; const PANIC: Self; }

kernels! {
    add_slow(a: &Zval, b: &Zval) -> Zval;
    concat_slow(a: &Zval, b: &Zval) -> Zval;
    fetch_dim_r_slow(container: &Zval, dim: &Zval) -> Zval;
    release(slot: Slot);
    push_frame(func: FuncRef, num_args: u32) -> FrameRef;
    pop_frame();
    step();                                   // run one opline at frame.opline through php_engine::step
    /* ... one line per kernel a trace may call */
}
```

Each `kernels!` line expands to `pub(crate) unsafe extern "C" fn php_jit_<name>(cx: *mut ExitCtx, frame: *mut Frame, opline: u32, ...args, out: *mut Zval) -> Status` and one entry in a `Symbols` table that `lower` reads for the address constant: the wrapper reconstitutes `&mut Request` from `cx.req` and `&Frame` from the pointer, finds `&Opline` from `frame.func().op_array().oplines[opline]`, calls the kernel inside `catch_unwind(AssertUnwindSafe(..))`, and maps the result to a `Status`, storing a `Bailout` or a panic payload into `cx.pending`. `interrupt` is `req.interrupt()`, the flag the engine's timer and signal delivery set (design §5.8), taken by `enter` once so that compiled code needs no layout knowledge of `Request`. The wrappers are `extern "C"`, not `"C-unwind"`, because every panic is caught before it reaches compiled code (`research/llvm-research.r1.md` B.4).

Where `pending` lives. An `ExitCtx` is a local of the `enter` that made it, compiled code holds its address for exactly the duration of that call, and a nested `enter` has its own. The nested case, walked: an inner wrapper stores its kernel's panic in the inner context; the inner `enter` resumes it; the unwind crosses the nested `execute` and the outer kernel; the outer wrapper's `catch_unwind` catches it into the outer context; the outer `enter` resumes it; one panic, the inner payload, reaches the request boundary. A bailout in the inner trace is the inner `enter`'s `Err`, the nested `execute`'s `Err`, the outer kernel's `Bailout`, which every kernel propagates from a nested `execute` as it does from any callee, and the outer wrapper's `BAILOUT`. The only request state the crossing still touches is `JitRequest.touched`, which is monotone.

The raw-pointer invariant, restated per fiber stack. `&mut Request` crosses as a raw pointer under the interpreter's own invariant: on one fiber stack exactly one frame chain executes on this request, and compiled code returns before anything else on that stack touches the request. A kernel called from a trace may suspend the fiber, through a native that calls `Fiber::suspend`; the trace's machine frame then waits on the fiber's stack while another fiber's code, interpreted or compiled, touches the request, exactly as an interpreter handler's frame waits across the same switch. Nothing the trace holds is invalidated by that: its frame is on the fiber's own VM stack pages (design §5.11), its `ExitCtx` is a local on the fiber's own machine stack, and the register-resident scalars it holds belong to slots only its own frame can name. What the switch does invalidate, a memory-derived fact about a shared array or object, is what the clobber rule (§4.2) already reloads after any call-effect kernel, and a suspend is inside such a kernel. So the invariant is per stack, and the design writes it that way.

`enter` does what no exit block can and nothing else. It does not read the exit's word, does not follow a link, and does not enter a side trace: it returns `Exited` and `dispatch` decides (§4.9). `CHECK_EXCEPTION` remains the `Resume` contract, `PACKED_GUARD` remains the side trace's entry type in its record, and `INVALIDATE` still does not exist.

Tested at the seam: a hand-assembled body calling `php_jit_step` through an `ExitCtx`; a panicking test kernel resumed at the request boundary; every wrapper under `force`; `jit.mode=exit-fuzz`, in which every guard fails once, the interrupt guard included, the interpreter continues at the landing, and the run must match the oracle byte for byte (G5); a kernel that re-enters PHP into a compiled root whose kernel panics, asserting one panic at the request boundary with the inner payload; `set_time_limit(1); for (;;) {}` under `force`, whose fatal must be php's to the byte; a `pcntl_async_signals(true)` handler delivered into a compiled loop; and a fiber that suspends from inside a kernel called by a trace and is resumed from another compiled loop.

### 4.7 `policy`, pure

```rust
pub(crate) struct Policy { mode: Mode, compile: Compile, hot_loop: u32, hot_func: u32, hot_return: u32, hot_side_exit: u32,
    blacklist_root: u8, blacklist_side: u8, max_trace_length: u32, max_root_traces: u32, max_side_traces: u32,
    max_exit_counters: u32, max_loop_unrolls: u8, max_recursive_calls: u8, max_recursive_returns: u8,
    max_polymorphic_calls: u8, buffer_size: usize, debug: Debug, map_jit: MapJit }
pub(crate) enum Mode { Off, Tracing, Force, ExitFuzz }
pub(crate) enum Compile { Thread, Inline }                                     // jit.compile; Force and ExitFuzz imply Inline
pub(crate) enum State { Cold, Recording, Compiled(TraceId), Blacklisted }
pub(crate) enum Outcome { Published(TraceId), Abort(Stop), Blacklist(Stop) }   // Abort counts an attempt; Blacklist does not wait for the count

/// Where a claimed word lives. Owned, so a claim can leave the thread that made it.
pub(crate) enum Holder { Root { epoch: Epoch, func: FuncRef, at: SafepointIdx }, Exit { epoch: Epoch, trace: TraceId, exit: ExitId } }   // Epoch: a hold on the epoch (design §5.14)
impl Holder { pub(crate) fn word(&self) -> &Word; }

impl Policy {
    pub(crate) fn from_ini(ini: &StartupIni) -> Result<Policy, IniError>;
    #[inline(always)] pub(crate) fn mode(&self) -> Mode;
    #[inline(always)] pub(crate) fn threshold(&self, start: Start) -> u32;
    pub(crate) fn attempts(&self, start: Start) -> u8;
}
#[inline(always)] pub(crate) fn read(word: &Word) -> State;                 // relaxed; all ones is Blacklisted
#[inline(always)] pub(crate) fn bump(word: &Word) -> u32;                   // racy: load, saturating add, store; returns the new heat
/// CAS cold → recording on `holder.word()`. `None` if another thread won. Dropped without `settle`,
/// the claim settles as `Abort(Stop::CompilerError)`. `Send`: it holds the epoch, not borrows.
pub(crate) fn claim(holder: Holder, policy: &Policy, start: Start) -> Option<Claim>;
#[must_use] pub(crate) struct Claim { /* holder, start, attempts limit; Drop settles */ }
impl Claim { pub(crate) fn start(&self) -> Start; }
/// One release store. `Abort` also stores zero to the heat half, as php's counter resets when it fires (§A.3).
pub(crate) fn settle(claim: Claim, outcome: &Outcome);
pub(crate) static INI: &[IniEntry];
pub(crate) fn dump(sink: &mut dyn Write, event: &TraceEvent);               // TRACE n start/stop/abort/blacklisted, php's words
```

Nothing here touches a request, a frame, a lock or a table. A `Claim` owns its word's holder instead of borrowing the word: a claim made on a request thread is settled on the compiler's thread (§4.8), after the request that made it may have ended, and the epoch its holder holds keeps the word alive for at least as long as the claim does. Why a claim is a value and not a boolean: the word is engine-shared, and if the thread that won the compare-and-swap leaves without a release store, because a kernel panicked under `step` or a `Bailout` came out of recording, the root stays `recording` for every request that follows, never compiled, never blacklisted, and silent. A `Claim`'s `Drop` is the store a panic would otherwise skip; the stop it records is php's `COMPILER_ERROR` (§A.4), and it counts an attempt, so a root that panics sixteen times is blacklisted.

Directives keep php's names and defaults so an oracle configuration replays unchanged: `opcache.jit` (php's CRTO number or word; `function` and any trigger other than `HOT_TRACE` are accepted, logged once at startup, and run as tracing, because there is one tier and no optimizer, §C.2 item 4; `off`, `disable` and `0` are `Mode::Off`), `opcache.jit_buffer_size` 64M (zero is `Mode::Off`), `opcache.jit_hot_loop` 61, `opcache.jit_hot_func` 127, `opcache.jit_hot_return` 8, `opcache.jit_hot_side_exit` 8, `opcache.jit_blacklist_root_trace` 16, `opcache.jit_blacklist_side_trace` 8, `opcache.jit_max_root_traces` 1024, `opcache.jit_max_side_traces` 128, `opcache.jit_max_exit_counters` 8192, `opcache.jit_max_loop_unrolls` 8, `opcache.jit_max_recursive_calls` 2, `opcache.jit_max_recursive_returns` 2, `opcache.jit_max_polymorphic_calls` 2, `opcache.jit_max_trace_length` 1024, `opcache.jit_debug` (php's bit names). Two engine-only directives: `jit.mode` (`tracing`, `force`, `exit-fuzz`) and `jit.compile` (`thread`, `inline`). Both are accepted from any host, because a startup ini has no way to know its host and php's own `opcache.jit_debug` is likewise available everywhere; `force` and `exit-fuzz` log one line at startup, and `inline` is a legitimate production choice for a short-lived CLI process. All are system ini: engine state is immutable after `Engine::new` (design §4.4), so `ini_set('opcache.jit', ...)` at run time is not honoured and is a row of kind `phpt` in the divergence registry.

Tested at the seam with no engine: `from_ini` against php's `zend_jit_parse_config_num` cases (`on` is 1254, `function` is 1205, `disable`); the state machine as a table, one row per `(State, Start, Outcome)`, driven over a `Word::default()` through a `Holder::Root` on a fixture `Published` and asserting `read` after each `settle`, including that sixteen aborts take sixteen threshold crossings and blacklist a root, that eight blacklist a side, that `claim` returns `None` when the word is not cold, that a `Claim` moved to another thread and dropped there leaves the word `cold` with one more attempt and zero heat; that `read(&Word::sealed())` is `Blacklisted` and `claim` on it is `None`, the layout invariant; dump lines diffed against the oracle.

### 4.8 `compiler`: the pipeline, on its own thread

```rust
pub(crate) struct Job { recorded: Recorded, claim: Claim }
pub(crate) struct Compiler { /* mode; queue: Mutex<VecDeque<Job>> with a Condvar; worker: Option<JoinHandle<()>> */ }
impl Compiler {
    pub(crate) fn new(shared: Arc<Shared>) -> Compiler;      // spawns the worker under Compile::Thread and not under Mode::Off
    /// `Inline`: runs `pipeline` here and returns after it. `Thread`: enqueues and returns at once.
    pub(crate) fn submit(&self, job: Job);
    /// Runs every queued job to completion on the calling thread. Tests.
    pub(crate) fn drain(&self);
}
/// `types::build` over the record and its view, then `shared.backend.compile`, then
/// `shared.table.publish`, then `policy::settle(claim, &outcome)`.
/// Each failure is mapped: an off-list `Stop` is `Abort(stop)`; an `Abort` from `build` or
/// `compile` is `Abort(Stop::NotSupported)` or the stop it carries; `Full` is
/// `Blacklist(Stop::TooManyTraces)` or its sibling. `policy::dump` writes the end event to `shared.log`.
pub(crate) fn pipeline(shared: &Shared, job: Job) -> Outcome;
```

Design it twice on where the pipeline runs:

| | (A) On the request thread, inside the safepoint | **(B) Chosen: one worker per engine, inline on request** | (C) A pool |
|---|---|---|---|
| What the winning request pays | record, build, lower, publish, in its own latency | record, and a queue push | the same as (B) |
| What other requests see | `recording` at the word, `Continue` | the same, for as long as the queue is deep | the same |
| Determinism for tests | complete | complete under `inline`; `force` and `exit-fuzz` imply it | complete under `inline` |
| Memory limits | the compile's allocations count against the request's `memory_limit` | they do not, as php's buffer does not (unverified) | the same |
| Threads per engine | none | one | N, and a scheduler |
| Where a compile panic surfaces | the request boundary | the worker's `catch_unwind`, reported through `shared.log` as php reports a JIT failure, the claim settled as `CompilerError` | the same |

(B) is chosen. A trace is entered on the next visit to its root in every column, which is php's behaviour, so a request never waits for a compile in any of them; what (A) makes it wait for is its own. (C) is a scheduler for a queue that G7 says holds one job at a time on the kernels; if a request-shaped corpus fills it, the worker count is a `Policy` number and nothing else changes.

What the worker touches. A `Job` is a `Recorded`, which holds the epoch, and a `Claim`, which owns the holder of its word; the worker reaches `Shared` through its own `Arc`. It touches no request, no frame and no map pointer, which is the same discipline compiled code keeps (§E.7). A trace compiled for a unit the cache has since evicted is published into the epoch and freed with it, which is correct and costs one compile. `Compiler::drop` closes the queue, drops the jobs still in it, whose claims settle themselves, and joins the running one; `JitEngine`'s field order puts `compiler` before `shared`, so the worker is gone before the table it reads. A panic in `publish` cannot poison the table: `publish` is transactional and the lock is taken with `PoisonError::into_inner`.

The dump. `hot` writes the start event and the worker writes the end event, both to `shared.log`, the engine's diagnostics sink, which is where php's `jit_debug` output goes. Under `Thread` the order of end events across roots is not stable, so the oracle diff of `TRACE` lines runs under `Inline`, which is one more reason `force` implies it. Under `Compile::Inline`, `submit` is `pipeline` on the calling thread, which is what every per-commit test runs; under `Thread`, `drain` is what a test calls after a request to make the queue empty before asserting a word's state.

Tested at the seam: `pipeline` over a fixture `Recorded` and a `Claim` on a fixture word, asserting the `Outcome` and the word's state for each failure of each stage, including a panicking fixture, with no backend but `dump`; and, natively, a `Thread` test that submits from four threads, drains, and finds every word `compiled` or `blacklisted` and no trace published twice.

### 4.9 `dispatch`: the one machine

```rust
pub(crate) enum Next { Continue, Enter(TraceId), Resume(FrameRef), Bail(Bailout) }

/// The one state machine over a word, for either home. A sealed word reads `Blacklisted` and
/// takes the last arm.
#[inline(always)]
pub(crate) fn visit(req: &mut Request, frame: FrameRef, word: &Word, holder: impl FnOnce() -> Holder, start: Start) -> Next {
    match policy::read(word) {
        State::Compiled(id) => Next::Enter(id),
        State::Cold => {
            let policy = &req.engine::<JitEngine>().shared.policy;
            if policy::bump(word) >= policy.threshold(start) {
                if req.trace_num() != 0 { return Next::Continue }                        // php bumps and does not trace while tracing (revision 9)
                match policy::claim(holder(), policy, start) { Some(claim) => hot(req, frame, claim), None => Next::Continue }
            } else { Next::Continue }
        }
        State::Recording | State::Blacklisted => Next::Continue,
    }
}

/// The cold path: record, submit, resume where recording stopped. Recording is exclusive per request.
#[inline(never)]
fn hot(req: &mut Request, frame: FrameRef, claim: Claim) -> Next {
    let _recording = RecordingGuard::set(req);                                       // sets the running context's trace number and clears it on drop, so a panic or bailout clears it
    match record::run(req, frame, claim.start()) {
        Ok(recorded) => { req.engine::<JitEngine>().compiler.submit(Job { recorded, claim }); Next::Resume(req.stack().current()) }
        Err(bailout) => Next::Bail(bailout),          // the claim drops here and settles itself
    }
}

/// From a `Func` word: what `Jit::safepoint` is. Here, and only here, `Continue` means "nothing happened".
#[inline(always)]
pub(crate) fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
    let engine = req.engine::<JitEngine>();
    if engine.shared.policy.mode() == Mode::Off { return Flow::Continue }             // no word is read or written under Off
    let at = site.idx();
    let func = req.func(frame.func());                                                // FrameRef::func() -> FuncRef; Request::func -> &Func
    let holder = || Holder::Root { unit: func.published().expect("a sealed word never claims").clone(), func: frame.func(), at };
    match visit(req, frame, func.word(at), holder, site.into()) {
        Next::Enter(id) => run(req, frame, id),
        Next::Continue => Flow::Continue,
        Next::Resume(f) => Flow::Resume(f),
        Next::Bail(b) => Flow::Bail(b),
    }
}

/// Trace to trace, until something is not compiled. A trace has run by the time anything is
/// returned, so `Continue` from an exit's visit is `Resume` here.
#[inline(never)]
pub(crate) fn run(req: &mut Request, mut frame: FrameRef, mut id: TraceId) -> Flow {
    let table = &req.engine::<JitEngine>().shared.table;
    let Some(first) = table.get(id) else { return Flow::Continue };                   // not compiled after all (§3.2, §4.5): nothing ran
    let mut trace = first;
    loop {
        let Exited { exit, frame: f } = match enter::enter(req, frame, &trace) { Ok(x) => x, Err(b) => return Flow::Bail(b) };
        frame = f;
        let ex = &trace.exits[exit.0 as usize];
        if let Some(link) = ex.link {
            match table.get(link) { Some(t) => { id = link; trace = t; continue }, None => return Flow::Resume(frame) }   // the window §3.2 names
        }
        let holder = || Holder::Exit { trace: table.arc(id), exit };
        match visit(req, frame, &ex.word, holder, Start::Side { parent: id, exit }) {
            Next::Enter(side) => match table.get(side) { Some(t) => { id = side; trace = t }, None => return Flow::Resume(frame) },
            Next::Continue => return Flow::Resume(frame),
            Next::Resume(f) => return Flow::Resume(f),
            Next::Bail(b) => return Flow::Bail(b),
        }
    }
}
```

**The exclusion rides the fiber switch** (revision 9, #627). It is the running execution context's trace number, php's `EG(jit_trace_num)`: one opaque word the engine stores per context and the tier alone interprets, read through `Request::trace_num` and written by the recording guard through `set_trace_num`. The fiber switch saves and restores it with the rest of the execution state and a fiber's body starts at zero (`Z/Zend/zend_fibers.c:105-155`, `:599`; design §5.11), and a generator resume does the same (`Z/Zend/zend_generators.c:793-865`). A recording suspended with its fiber therefore holds the exclusion for that context alone, and the fiber that resumes may record a root of its own, as php's may; a fiber suspended during a recording and never resumed cannot leave the request unable to record for the rest of its life, which a bit in `JitRequest` would.

`visit` is the machine for both homes; the only thing a home contributes is the `Holder` it would give a claim, and it is a closure so that the compiled and the cold-below-threshold paths, which are every visit but one, build no `Arc`. The `expect` in the root holder is never reached, because a sealed word never crosses a threshold: it is the one place the sealed-word invariant is written in `php_jit`, and it is written as an assertion. The recording bit is read only after a threshold crossing, so it costs nothing below the threshold and nothing when compiled; php's shape is the same (`php-src-jit-hot-root.txt:30-33`). `hot` never touches the word: the claim holds it. Recording ran the program forward, so `hot` resumes wherever recording stopped, and a `Threw` stop resumes in the frame `HANDLE_EXCEPTION` runs in (§4.1). The compiled trace is entered on the next visit to the root, as in php. `run` follows links without reading a word, because a link is a build-time fact on the exit, enters side traces by the id in the exit's word, and holds each trace through the table's guard for exactly the time it is inside it; `table.arc(id)` is the one place a strong reference is taken, and it is on the claim path only. The three meanings of "not compiled" are stated on the three places: at a root whose slot is empty nothing ran and `Continue` is right; after an exit a trace did run, so a missing link or side target is `Resume` of the landed frame; and a word that reads `Recording` or `Blacklisted` at an exit is `Resume` for the same reason.

Tested at the seam: `visit` as a table over `(State, Start)` on a `Word::default()` and a `Word::sealed()` with a fixture engine, asserting `Next`; `visit` with the context's trace number set at a threshold crossing, asserting `Continue` and a bumped word; a fiber suspended inside a recording and never resumed, asserting a root recorded on the resumer after it (revision 9); a witness whose destructor runs a hot loop inside a recording, asserting one `TRACE start` line, a second root recorded on the next request, and the bit clear after a panicking fixture recording; `run` with two hand-assembled traces, one linking to the other and one exiting to a cold word, asserting the sequence of entries through a `Probe`-style counter kernel and that the return is `Resume` of the landed frame; and, under `force`, that every witness enters through `safepoint` and returns through `run` with the oracle's output.

---

## 5. The kernel face: where the `extern "C"` wrappers live

### 5.1 Design it twice

| | (A) In `php_engine`, by macro beside each kernel (§E.8) | **(B) In `php_jit::enter`, by one `kernels!` list (chosen)** |
|---|---|---|
| Can it exist there | only with reference parameters, since dereferencing `*mut Request` is `unsafe` and `php_engine` forbids it; the soundness obligation then rests on the caller and is documented nowhere in the type | yes; the two raw-pointer dereferences are in the crate that owns the calling convention |
| `php_engine`'s public surface | grows by one `extern "C"` item per kernel, roughly three hundred, for exactly one caller | grows by zero items; the kernels themselves become visible to `php_jit` under `tier.lock` (§5.3) |
| Knowledge of the JIT's convention (`Status`, `opline` as index, out-pointer) | inside `php_engine` | inside `php_jit` |
| Drift between the list and the kernels | none; the wrapper is generated at the definition | a signature change in `ops` fails `php_jit`'s build, because the wrapper calls the kernel; no checker |
| Deletion test | delete the macro and the wrappers reappear in `php_jit` once, by macro: nothing spreads | delete the list and each wrapper reappears at its call site in `lower`, three hundred times: it earns its keep |

(B) is chosen. The wrappers are not `php_engine`'s interface; they are `php_jit`'s interface to its own compiled code, an internal seam, and the price of keeping them there is a hand-written list the compiler checks. The review's proposal to generate the list from `ops.toml` is not taken here and is left for a later revision.

### 5.2 One `step`, not two hundred opcode wrappers

php's `zend_jit_trace_handler` calls the opline's handler (§A.9); here the handler is an arm of the generated `match`, and `step` is that `match` run once. A trace that meets an opline `lower` does not know stores `frame.opline` and calls `php_jit_step`. One item where two hundred would have been generated.

### 5.3 The tier face, and `tier.lock`

`php_jit` sees more of `php_engine` than `api`; it is the only crate that does, `php_macros` depending on no workspace crate (ADR-0007). What it sees is listed and locked, by principle 8: `tier.lock` has two sections. The **face**: `Tier`, `Site`, `Flow`, `Nop`, `execute`, `step`, `Stepped`, `Word` with `halves` and `sealed`, `SafepointIdx`, `Func::word`, `Func::published`, `Published`, `Request::func`, `Request::interrupt`, `Request::trace_num` and `Request::set_trace_num`, `FuncRef`, `OplineIdx`, `Request::map_base`, `Request::stack`, the `api` items a tier's state needs (`Extension`, `ExtensionBuilder`, `IniEntry`, `StartupIni`, `Request::engine`, `Request::state`, `DiagnosticsSink`), and `UnitView` with its builders: a few dozen items, reviewed one by one. The **kernels**: the `ops::*` functions the `kernels!` list names, about three hundred, generated into the lock from the list itself by `api-check` and diffed as one section, so a kernel signature change in month one or two is a lock diff that names the kernel and nothing else. `xtask api-check` allows `php_jit` to import from `php_engine` only what either section names.

```rust
/// What a trace may read of the op arrays, literals and engine-immutable classes it touched. Built
/// by the recorder from the request; holds the request's epoch and the ids it accepted; `Send`.
pub struct UnitView { /* private */ }
impl UnitView {
    pub fn new() -> UnitView;
    pub fn hold(&mut self, req: &Request, f: FuncRef) -> bool;                // false, and holds nothing, for a request-scoped op array
    pub fn hold_class(&mut self, req: &Request, c: ClassId) -> bool;          // false, and holds nothing, unless the ClassId is published
    pub fn op_array(&self, f: FuncRef) -> &OpArray;                           // defined only for an op array hold accepted
    pub fn class(&self, c: ClassId) -> &LinkedClass;                          // defined only for a class hold_class accepted
    pub fn cache_slot(&self, f: FuncRef, opline: OplineIdx) -> MapPtrRaw;
}
```

`heap.lock` holds `VmStack::entry`, `push_entry`, `current`, `MapTable::base`, and `FrameRef::func() -> FuncRef` as a frame-header field.

---

## 6. Dependency direction and crate placement

```
php_heap ◄── php_unit ◄── php_compile ◄── php_engine ──► php_macros ◄── php_ext
   ▲            ▲                             ▲   ▲
   │            │                             │   └── php_ext ◄── ext/* ◄── php_bundle ──► (feature jit) php_jit
   └────────────┴──────── php_jit ────────────┘         ▲            ▲
                          (tier.lock face of php_engine; heap.lock; unit types;         │
                           cranelift-codegen, cranelift-frontend, cranelift-native,     │
                           regalloc2, rustix or memmap2, arc-swap)                      │
                                                        php ──► php_bundle ──► php_sapi ──► php_proto
                                                        xtask (in-process phpt, witness, bench) ──► php_bundle
```

Rules, all enforced by `xtask api-check`. `php_jit` depends on `php_engine`, `php_heap`, `php_unit`, the Cranelift crates, `rustix` (or `memmap2`) for `memfd_create`, the sealing `fcntl` and the two `mmap`s on Linux, and `arc-swap` for the table's slots; it imports from `php_engine` only `tier.lock` items. `php_engine` never depends on `php_jit`. `php_bundle` depends on `ext/*`, on `php_ext` for the `ExtensionBuilder` type its list returns, and, under the `jit` feature on the four targets, on `php_jit` for one alias; it names `php_engine::tier::Nop` through `php_ext`'s re-export otherwise:

```rust
// php_bundle
#[cfg(all(feature = "jit", any(target_os = "linux", target_os = "macos"), any(target_arch = "x86_64", target_arch = "aarch64")))]
pub type Tier = php_jit::Jit;
#[cfg(not(all(feature = "jit", any(target_os = "linux", target_os = "macos"), any(target_arch = "x86_64", target_arch = "aarch64"))))]
pub type Tier = php_ext::tier::Nop;
pub fn enabled() -> &'static [ExtensionBuilder];   // ext/* only; the tier is not in this list

// every host, the same line
let engine = Engine::builder().extensions(php_bundle::enabled()).tier::<php_bundle::Tier>().build()?;
```

Design-it-twice on the chooser: the `php` binary could make the choice, but then `xtask`'s in-process phpt host, the embed host and the dev server each repeat it, and design §4.6's deletion test for `php_bundle` applies to the tier verbatim. The bundle is the one place the feature flags live, so the tier is a feature flag there. The Cranelift crate set, `rustix` or `memmap2` and `arc-swap` are counted against the ≤ 120 lock-package target on day 31 (§F.4); `cranelift-jit` is not used, because its memory provider leaks by default and knows nothing of `MAP_JIT` (§B.8).

`php_unit` carries `OpArray.safepoints` and the compiler writes safepoint numbers into the field `ops.toml` names; both are unit-format items under ADR-0001 and `unit.lock` names them. `php_engine` names `Published`, `Word::sealed` and `Request::interrupt` on the tier face. `php_heap` carries the entry slot and `current` on `VmStack`.

---

## 7. Testability through the interfaces

| Module | Seam | Test adapter or fixture | Oracle | Mode |
|---|---|---|---|---|
| `Tier` in `php_engine` | `execute::<T>`, `EngineBuilder::tier` | `Probe` adapter asserting `(site, number)` sequences on witness scripts: `(Entry, 0)` exactly once per call and never for a call whose `RECV` throws; `(Loop, n)` with one `n` for two back-edges to one header, on `for`, `foreach` with `continue`, and `do { } while (!$x)`; `(Return, n)` never for the entry frame; and that its request state exists | none needed | `cargo test -p php_engine` |
| `step` in `php_engine` | `step` | a witness single-stepped to completion produces the output of `execute::<Nop>`; one witness per `Stepped` arm; `Pushed` lands at the first body opline with `Entry` raised; a fused compare-and-branch is one step; `Done` at the entry frame's return, with and without a pending exception | `Capture` output | `cargo test -p php_engine` |
| the word in `php_engine` | `Func::word`, `Func::published`, `OpArray.safepoints` | every backward jump and return in a witness carries the number of a word; two back-edges to one header carry one number; the entry is zero; an `eval` op array's words are sealed and its `published()` is `None` | `compile-identity` unchanged | `cargo test -p php_engine` |
| the invariant in `php_compile` | `compile` | every op array of the per-commit corpus has a header for every backward jump and a column for every backward jump opcode | none: an assertion | `cargo test -p php_compile`; `compile-identity` sweep |
| `VmStack` entry slot in `php_heap` | `entry`, `push_entry`, `current` | nested `execute` from a native, a bailout inside the nested one, a fiber switch between two nestings, under Miri | none needed | `cargo test -p php_heap` |
| `record` | `run`, `dump`/`parse` | `Capture` host, `jit.mode=force`; recorded traces as fixtures; one fixture per `Stepped` arm, including a `Return` start, a `RequestUnit` stop at a call into an `eval` function, and a fused branch each way | oracle's `TRACE_START|TRACE_STOP` lines through `oracle::compare` | per commit on the seven kernels |
| `types` | `build` | parsed fixtures in, `lower::dump::render` out, golden diff; one fixture per clobbering kernel class | goldens fixed once against `TRACE_TSSA` | per commit, no engine |
| `lower` | `Backend::new`, `Backend::compile` | CLIF text golden per fixture, the interrupt test in every loop golden; bytes run under `force`; `Unsupported` on a stubbed host | oracle output | per commit |
| `table` | `publish`, `get` | `HeapPages` under Miri: bounds, segments, `Full`, a full table that stops compiling, a new epoch's table empty, a panicking publish, chunk return; `mov; ret` through two views natively; the concurrent publish test natively on both targets; one `PERF` line | php's `perf` map line | per commit |
| `enter` | `enter`, `kernels!` | hand-assembled body calling `php_jit_step` through an `ExitCtx`; a panicking test kernel; the nested-entry panic; every wrapper under `force`; `jit.mode=exit-fuzz` for every landing (G5); `set_time_limit(1); for (;;) {}` and a `pcntl` signal into a compiled loop under `force`; a fiber suspended from inside a kernel a trace called | oracle output, byte for byte | per commit on witnesses, nightly on `Zend/tests` and `tests/` |
| `policy` | `from_ini`, `read`/`bump`/`claim`/`settle`, `dump` | php's config-number cases; the state table over a `Word::default()` and a `Word::sealed()` through a `Holder`; a `Claim` dropped on another thread; the heat reset on `Abort`; dump diff | oracle's `ini_get` and `jit_debug` output | per commit, no engine |
| `compiler` | `pipeline`, `submit`, `drain` | a fixture `Recorded` and `Claim`, asserting `Outcome` and word state for each failure of each stage, including a panicking fixture; four submitting threads and a drain | none | per commit |
| `dispatch` | `visit`, `safepoint`, `run` | `visit` as a table over `(State, Start)` and over the context's trace number; the nested-recording witness; `Off` touching no word under a `Probe`-style word spy; two hand-assembled traces, one linking and one exiting cold, asserting `Resume` of the landed frame | none | per commit |
| whole tier | `Jit` | G1 to G7 under the protocol (§E.11), with `jit.compile` in both settings for G6 and G7 | oracle with `opcache.jit=tracing` and `off` | per commit for G1, G4, G5; run of record for the flip |

Every row tests at an interface named in §4; none reaches past one. `force` and `exit-fuzz` are how the whole tier is tested through `Jit` alone: `force` compiles every eligible root at first execution so the witnesses and phpt corpora exercise every wrapper and guard predicate, `exit-fuzz` makes every guard fail once so the deopt protocol is exercised at every exit, the interrupt guard included. Zero divergence under both is G5, and G5 blocks the default.

---

## 8. Deepening moves, and the shallow modules they replace

| Risk | Why shallow | Move |
|---|---|---|
| A `Mode` trait beside `Tier`; five tier hooks; a `Recording` adapter and a third instantiation of the loop | two type parameters, four instantiations, one meaningless; "enter" is not a request the loop makes; a hook and a case two of three adapters implement as nothing | one trait, one hook; `Site` is a folded constant; enter is a branch; the recorder is a loop over `step` (§2) |
| `ExitInfo` as php's flag bitset | six flags, most combinations impossible, two meaningless here | a closed `Landing` of three cases; `CHECK_EXCEPTION` is the `Resume` contract; `INVALIDATE` and `PACKED_GUARD` vanish (§4.2) |
| One `extern "C"` wrapper per opcode; wrappers as a face of `php_engine` | two hundred pass-throughs to one `match`; three hundred public items for one caller | `step`; `kernels!` in `php_jit::enter`; `tier.lock` names what the engine exposes (§5) |
| `Func.code` beside the state word | two representations of "the entry is compiled" | the word alone (§3.1) |
| A backend builder trait for `lower`; a Cranelift type on the pipeline | Dax's `LowerHost` shape; a third-party type in three modules | `TraceIr` as a value with two consumers; `Backend` built from `Policy` (§4.3) |
| Patching exits and links into code; a `Tier::unpublish` hook or an epoch | RW after publication, an `INVALIDATE` path, an icache protocol per patch; a hook the interpreter would call for the tier's sake | bytes final at publish; the trampoline dispatches; `Arc` strong count under the compile lock (§4.3, §4.5) |
| Polymorphism bits written by `php_engine` kernels; static, thread-local or global JIT state | engine code writing a tier word; the inversion the design rejected for extensions | dropped; `engine_state` and `request_state` on `Tier::extension()` (§3.2, §4) |
| `Trace`, `TraceTable`, `TraceId` with no module; `policy::hot()` as the pipeline; `Exit { taken, attempts, target }`; `exits` as a module; `ExitSpec`, `Exit`, `Landing` in three modules; `UnitView` named and never placed; `Status` tested at every call site | one lifetime argument in four modules; a module named for thresholds calling every other; a second heat machine with no CAS; three types for one exit; a lock entry per accessor; a convention with two owners | `table`; `policy` pure and `compiler` the pipeline; an `Exit` embeds a `Word`; `ExitSpec` and `Landing` in `types`, `Exit` built by `table`; one `UnitView` type; one emitter in `lower` (revision 2) |
| `claim` as a `bool` with `settle` in two callers; no link target on an exit | one protocol in two places and a panic in neither; `Link` stops with nowhere to go | `Claim` settles on drop; `ExitSpec.link` (§4.7, §4.2) |
| The word machine in `Jit::safepoint` and in `enter`'s loop; `JitRequest.pending`; `compile` tested through a `cfg(test)` substitution; `Word` with `pub` halves and a two-level index; a word per opline; `code`'s RW window; `record` reading `prev` and classifying by identity; `Claim<'w>` and `UnitView<'e>` borrowing; `Trace.unit` and `OnceLock` slots; compile on the request thread | one machine, two drivers; a shared slot for a conversation that nests; a seam for one test; a layout two crates know; allocated where never used; a rule about a moment in time, false on Linux; frame-chain knowledge outside the chain's owner; cannot leave the thread; a slot that cannot be cleared; the engine's `Sync` unused | `dispatch::visit` and `run`; `ExitCtx`; `pipeline` takes a `Recorded`; one accessor and one 24-bit id; a word per safepoint; two views; `Stepped` and `VmStack::entry`; owned `Holder` and `UnitView`; `units`, `links`, swappable slots, a guard per entry; `compiler` with one worker (revision 3) |
| No interrupt test in compiled code (revision 3 §4.3) | a loop the interpreter's timer could not stop; the design of record's own time-limit witness hangs under `force` | one load and branch at the trace's loop header through `ExitCtx.interrupt`; the `Resume` contract performs the check (§4.3, §2.2) |
| Claims taken from a nested `execute` during recording (revision 3 §4.9) | a second recording on one request, with `hot` re-entered under `record::run` | the running context's trace number (`Request::trace_num`) held by a guard; `visit` bumps and does not claim while it is set (§4, §4.9, revision 9) |
| `Site::Entry` "inside the first body opline's handler" (revision 3 §2.2) | any opcode can be that opline: a compare per opline or a mutable handler table | the activation runs the prologue and raises `Entry`; `Pushed` lands at the body (§2.2) |
| A word read and a `Published` clone for every frame (revision 3 §4.9) | an op array from `eval` has no `Published` | sealed words; `Func::published()` an option read only inside the holder closure (§3.2) |
| Thresholds at `u32::MAX` for `Mode::Off` (revision 3 §4) | a bump on every safepoint on every thread with the tier off; a saturated counter claims | one mode load and `Continue` (§4.9) |

The named risk that runs the other way is unchanged: `TraceIr` could grow into a general IR. The review's region-shaped IR is the deliberate form of that growth and is not taken here; the first proposal to add a control-flow construct is still the moment to reread `research/jit-research.r1.md §D` and that card.

---

## 9. The eighth seam, the crate-map row, and the locks

### 9.1 Seam table row, in the shape of design §4.3

| # | Seam | Lives in | Interface | Adapters | Zend equivalent |
|---|---|---|---|---|---|
| 8 | `Tier` | `php_engine::tier`; `execute` is generic over it; bound per `Engine` as an `ExecuteFn` by `EngineBuilder::tier::<T>()`, which also registers the tier's state; named once in `php_bundle` | One method, `safepoint(req, frame, Site) -> Flow`, where `Site` is `Loop(n)`, `Entry` or `Return(n)` and carries the safepoint's number, and `Flow` is `Continue`, `Resume(frame)` or `Bail`, with the interrupt and exception checks after a `Resume` as the loop's; one provided method, `extension()`. Beside it, `step(req, frame) -> Result<Stepped, Bailout>`, which runs exactly one handler with `Nop` at the safepoints and reports `Same`, `Pushed`, `Popped`, `Moved`, `Threw` or `Done`; and one opaque `Word` per safepoint on `Func`, zero at publication, sealed for a request-scoped op array, whose bits only the tier interprets | `Nop` (month one, in `php_engine`); `php_jit::Jit`; `Probe` (engine tests) | `zend_execute_ex` as the bound entry; the counter handlers at loop headers and after `RECV` (`zend_jit.c:3219-3244`); `zend_jit_trace_execute` has no equivalent, because `step` is what it exists to be |

### 9.2 Crate-map row, in the shape of design §4.1

| Crate | Role | Interface (what callers learn) | Origin |
|---|---|---|---|
| `php_jit` | **The compiled tier.** Trace recorder as a loop over `step`, trace SSA with php's guard set and the clobber rule, lowering of straight-line traces with the interrupt test behind a backend value, one table owning every published trace in a dual-mapped arena, one dispatch over one state machine for roots and exits with recording exclusive per request, the deopt landing at the identical frame layout inside the crossing, php's thresholds as a pure state machine, and one pipeline from a record to a published trace on the engine's compile thread. `#![deny(unsafe_code)]` with the allowance on `code` and `enter`, about 300 lines. | `Jit: Tier`; nothing else is `pub`. Reads `php_engine` through `tier.lock`. | New. Shape after `zend_jit_trace.c`; nothing taken from Mahout's five JIT crates (§B.9). |

### 9.3 Locks and gates touched

`tier.lock` has a face section and a kernels section (§5.3); the face gains `Word::sealed`, `Func::published` as an option, `Request::interrupt`, `Request::trace_num` with `set_trace_num` (revision 9) and `UnitView::hold` returning a bool. `heap.lock` holds `VmStack::entry`, `push_entry`, `current`, `MapTable::base`. `unit.lock` holds `OpArray.safepoints`; `ops.lock` holds the `safepoint` column, now with `JMPZ`. `api.lock` is unchanged. `xtask unsafe-audit` names `php_jit::table::code` and `php_jit::enter`. `xtask budget` holds `php_jit` to the ceiling its `seams.toml` row carries. The gates: G6 and G7 run with `jit.compile` in both settings, and the run of record names which; G5's per-commit witnesses gain the time-limit, signal, nested-recording and fiber cases of §7.

---

## 10. What this note asks of the design of record and of the harness

Of the design of record, all taken in its ledger entry 8: `Site::Entry` raised by frame activation, with `Stepped::Pushed` landing at the first body opline; the interrupt flag as a named item on `Request`, tested at back-edges, at activation and after every `Resume`; sealed words for request-scoped op arrays and `Func::published()` as an option; `JMPZ` in the `safepoint` column and the compiler's assertion (corrected in design ledger 9: `JMPZ` is backward only after the optimizer, and `FAST_RET` is excluded); the fused compare-and-branch rule on `step`; `opcache.jit_buffer_size` as the one name; the `tier.lock` sections; the `php_bundle` edge to `php_ext`; the recording bit as a sentence beside the word; G1 defined as the tier bound and `Off`.

Of the harness (`harness-design.md`): rows in its §7 for the tier's lanes, `force` and `exit-fuzz` per commit on the witnesses and nightly on `Zend/tests` and `tests/`, the trace-dump diff under `inline` per commit on the seven kernels, and G1 per commit from the day `php_engine::tier` exists; the `trace` kind's two rows in `docs/divergence.toml`; and a reading of instrument `bench` for G8 if the LLVM spike is ever scheduled, which is after G3 has a reading and not before.

Day-31 prototypes, before any trace is recorded: the per-call cost of `catch_unwind` in the kernel wrappers on the non-unwinding path; the per-opline cost of `step` under recording on the seven kernels; the dual-mapped `mov eax, 7; ret` through two views on the aarch64 box, with cache maintenance against the execute view and the reused-chunk `isb` question answered; the concurrent publish test on both targets; and the interrupt test's cost on the seven kernels against the same traces without it, so that the sixth "if this is wrong" has its number on the first day a loop compiles.
