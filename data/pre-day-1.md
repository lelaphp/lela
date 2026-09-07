# Pre-day-1 checks

Run on 2026-09-07 on the dev machine (Darwin arm64, rustc 1.95.0, Claude Code 2.1.263), before any crate exists. Each item names the open question it answers and where the answer now lives.

## 1. The oracle is 8.5.9, everywhere

| Thing | Before | Now |
|---|---|---|
| `php` binary | Homebrew keg 8.5.9, md5 `315162bd…` | pinned in `oracle.toml` |
| php-src tree, stubs, phpt corpus | 8.5.7 checkout | `~/Projects/php-src-8.5.9`, a git worktree at tag `php-8.5.9` (`dd6e76cc`) |
| phpt files at the tag | 21,548 (8.5.7) | 21,655; the corpus lock is regenerated at 8.5.9 on day 1 |
| `run-tests.php` `$ini_overwrites` | design §8 says 39 | 38 at both 8.5.7 and 8.5.9 (`run-tests.php:269-308`); the design's count is off by one |

`Z/path:line` citations in the design records are at 8.5.7 and are not re-pointed; citations from design ledger 9 on are at 8.5.9.

**opcache is compiled into the Homebrew build.** `-d zend_extension=opcache` fails to load and is not needed; the tier regimes in `oracle.toml` set `opcache.enable_cli=1` and `opcache.jit=…` only. **A script is only JIT-compiled if opcache cached it**, and opcache's default `file_update_protection=2` refuses a file written in the last two seconds. That is why the entry is in the baseline ini; a probe that omitted it saw a JIT buffer that never grew. With the baseline applied, `opcache.jit_debug=258048` (bits 12–17 of `zend_jit.h:67-72`) emits `---- TRACE N start/stop/compiled/abort` lines on stderr, with the absolute script path in each, which the recipe's scratch token normalises.

## 2. The manifest's grammar (harness §12 item 13)

`seams.toml` is written against every rule of design §4.1, §4.2, §4.5, §10 and harness §4.1. Findings:

- **The harness's example is not TOML.** `name = "x"; budget = 1` on one line is a syntax error; TOML has no statement separator. Rows are one key per line. `xtask`'s parser is `toml`, so the example in harness §4.1 should be rewritten in the next harness revision.
- **Four predicates the example lacked**, each marked `# grammar:` in the file: `deny_lints` (design §10's denied clippy lints, two rules), `check = "<name>"` (a named `xtask` lint: the re-entry signature rule, witness coverage, divergence reasons), `avoid_from` (the glossary as a sensor, harness §5), `generated = { file = rule }` (the committed `ext/clippy.toml`).
- **Budget arithmetic.** The `[[crate]]` rows sum to 253.2k, plus 15k bindings is 268.2k against the 272.5k total, so the total is read as excluding `docs`; 4.3k slack.
- **Not carried, by decision.** Binary size (design §10 names it as a budget with no number). The `hash` extension appears in the day 15–21 schedule but not in §4.1's budget table nor in Mahout's 19 crates; it has no row.
- The `php_bundle → php_sapi` arrow in design §4.2's diagram is read as `php → php_bundle` and `php → php_sapi`; the manifest encodes that reading.

## 3. Rustdoc and the `expect` fence (harness D.3)

Tested with a scratch crate, edition 2024, rustc 1.95.0.

- `cargo test --doc` runs real doctests and ignores the `php`, `expect` and `stderr` fences, as intended.
- `rustdoc::invalid_codeblock_attributes` fires on the `=` character: "unexpected character `=`" for `expect exit=255 wall=1s`. Under `-D warnings` `cargo doc` fails. `expect` alone, `expect exit:255 wall:1s` and `expect exit-255 wall-1s` are clean.
- **Resolution to choose on day 1:** `#![allow(rustdoc::invalid_codeblock_attributes)]` at the root of every crate that carries witnesses (one line, verified to pass `-D warnings`), or the fence grammar changes to `exit:N wall:D` and the manifest's `witness.exit-authored` regex with it. The allow keeps the design's grammar and costs nothing the harness does not already check itself.

## 4. Hook JSON shapes (harness D.16)

Captured from a headless `claude -p` run with a command hook on `PreToolUse` (matcher `Bash`) and on `Stop`, on Claude Code 2.1.263.

PreToolUse stdin:

```json
{"session_id":"…","transcript_path":"…/<session>.jsonl","cwd":"…","prompt_id":"…","permission_mode":"auto","effort":{"level":"medium"},"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo hook-probe","description":"Echo hook-probe"},"tool_use_id":"toolu_…"}
```

Stop stdin:

```json
{"session_id":"…","transcript_path":"…","cwd":"…","prompt_id":"…","permission_mode":"auto","effort":{"level":"medium"},"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"…","background_tasks":[],"session_crons":[]}
```

Outputs, both verified end to end:

- Deny at the tool call: the hook prints `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"<text>"}}` and exits 0. The tool never ran; the model saw `<error><text></error>` and reported the refusal with the rule's id from the text.
- Hold the turn at Stop: the hook prints `{"decision":"block","reason":"<text>"}` and exits 0. The model continued with the reason as its instruction; the hook fired again with `stop_hook_active: true`, on which `done --hook` must pass or the session loops.

The `permit` latency bar is not measurable until `xtask permit` exists.

## 5. The Mahout pin (design §13)

Mahout `main` is 20 commits past `1b3104a8` (HEAD `59fd0f16`). The diff over every path on the inheritance manifest:

| Path | Change |
|---|---|
| lexer, parser, source, syntax generator, `php.ungram` | none |
| `mahout_native_macros`, `mahout_native_abi` | none |
| 19 `mahout_ext_*` crates | none |
| `messages.rs`, `ids.rs`, `compare.rs` (zend_sort), `convert.rs` (gcvt), `ty2_pins` | none |
| `mahout_phpt`, `mahout_fuzz_gen`, `fuzz/`, `conformance/*.lock.json`, `bench.rs` | none |
| `semantics/coerce.rs` | +388/−73, all in `bind_callback_args*`, `deref_cell_arg_quiet`, `settle_transient_cells_named` and the cell-ABI settle of stage 379b; none of the six ladder functions §13 takes |

The other 20 commits are JIT tier work (`jit2`, `tier2_host`, `mir_exec`, `interp`), plans and research. **Recommendation: keep the pin at `1b3104a8`.** Nothing inherited moved, and the one file that changed changed in a part the design does not take.

## 6. `ops.toml` (design §5.7, jit-design §3.1)

Extracted by `scripts/extract_ops.py` from the 8.5.9 tree; derivations are stated in the file's header and the verification log is `data/extract.log`. Full report: `data/ops-extraction.md`.

| Count | Design says | At 8.5.9 |
|---|---|---|
| opcodes | — | 209 (numbers 0–210; 45 and 79 unused) |
| handlers in `zend_vm_def.h` | 207 | 207 plus one `ZEND_VM_INLINE_HANDLER` (`RETURN`); 42 type-specialised handlers beside them |
| helpers | 25 | 34 |
| hot opcodes with `specialise = true` | 30 | 30 |

The six safepoint opcodes leave `result` unused in every handler body, transitively through helpers, and every `zend_compile.c` emission passes a null result, so `result` carries the safepoint number for all six. `RETURN_BY_REF` reads `extended_value`, and `RETURN` gets `extended_value = -1` on the implicit final return, which confirms `result` and not `extended_value` as the field.

**Three claims the source contradicts, to take into design ledger 9 and JIT ledger 5:**

1. **`JMPZ` never jumps backward before the optimizer.** `zend_emit_cond_jump` (`zend_compile.c:2387-2409`) does not invert on `BOOL_NOT`; `do {} while (!$x)` compiles to `BOOL_NOT` then a backward `JMPNZ`. A backward `JMPZ` exists only after `block_pass.c:699-707`. The pre-optimizer backward jumps are `JMPNZ` (while, do-while, for) and `JMP` (foreach's loop end, `continue` in a foreach after `pass_two` rewrites `BRK`/`CONT`, and a backward `goto`). `JMPZ` keeps its `safepoint` column for a month-two optimizer's output and the design's `do { } while (!$x)` sentence is wrong.
2. **`GENERATOR_RETURN` is not emitted by `zend_compile.c`.** `pass_two` rewrites `RETURN*` inside generators (`zend_opcode.c:1213-1217`); "the instruction set as `zend_compile.c` emits it" has to read "as `pass_two` leaves it", which is also where `BRK`/`CONT` become `JMP`.
3. **`FAST_RET` jumps to an earlier opline index** (`fast_call + 1`) without being a loop back-edge: it is the finally subroutine's return and has no CFG successor (`zend_cfg.c:508`). The compiler's assertion "every backward jump has a `safepoint` column and a numbered header" must exclude it by opcode, or it fires on the first `finally`.

**The `RECV` prologue, precisely** (JIT design "seventh", design §5.6). `i_init_func_execute_data` (`zend_execute.c:4426-4437`) advances `EX(opline)` past the `RECV*` oplines only when the function has no type hints and was passed no more arguments than it declares; otherwise the frame starts at opline 0 and the `RECV*` handlers run. The JIT (`zend_jit.c:3219-3231`, `zend_jit_trace.c:8985-8999`) installs its entry counter on the first opline after the `RECV*` run when there are no type hints, and on opline 0 otherwise. So "the entry is the opline after the `RECV*` prologue" holds only for untyped parameters; for a typed function php's counter sits on opline 0 and fires before the `RECV` that may throw. The design's rule that `Site::Entry` is never raised when a `RECV` throws therefore diverges from php's trace roots for typed functions, which is exactly the low-likelihood risk row in §12 and now has its citation.

Smaller: `FETCH_UNSET` with a global fetch does not write the symbol table (the helper's `BP_VAR_UNSET` path never inserts), so `write_globals` has seven opcodes, not eight; `effects.throw` is absent on 14 opcodes only, and every other opcode carries `throw_cond` with the text of `zend_may_throw_ex`'s condition.

## 7. `api.lock` v0 (design §4.4)

Drafted by `scripts/census.sh` over Mahout at the pin, joined to §4.4's groups and rules. Full report: `data/api-lock-v0.md`; the join is `data/port-map.v0.toml`, the seed of `xtask port-map`.

| | Design §4.4 | Census at the pin |
|---|---|---|
| distinct Mahout items the extensions cross | 345 | 389 over the four sources (value 134 incl. constructors, `RequestCtx` 174 incl. public fields, `semantics::` 171, ABI 18 of 38) |
| `api.lock` v0 items | ≤ 400 | 301: request 98, values 86, call 29, abi 28, coerce 22, decl 17, raising 17, attr 4 |
| port-map rows | — | 651: 165 same, 336 renamed, 150 gone under 92 rules |

The census's earlier numbers undercounted because the extensions reach the request through `Reentrant::ctx` at 1,209 sites and through public fields such as `ctx.classes`, and two items the design lists as value methods (`instance_of`, `lookup`) are class-table methods. The largest deletions are the `Reentrant` receiver, the `sig` attribute, `note_alloc`, and the descriptor-construction family; the real linker leak is `mahout_native::build_linked_descriptor` at 49 sites, not the `use mahout_linker` lines the census counted.

**`engine_limitation`, decided: gone.** php has no engine-limitation error, so no witness can pin one and no api item exists for it. Of Mahout's 15 sites, ten (curl, iconv, mbstring, dom) go with the C bindings the design chooses over pure Rust; two (an `SplPriorityQueue::compare` override, a `PhpToken` `__get` override) exist only because Mahout could not re-enter user code from those positions, which `call::call` and the object handlers provide; three (RegexIterator `GET_MATCH`/`ALL_MATCHES`/`SPLIT`, `unpack` non-integer codes, `htmlspecialchars` non-UTF-8 charsets) are unported behaviour. For those, the native's own witness fails against the oracle and proposes an open divergence row, and `done` fails while the row is open. There is no interim raise: a native either does what php does or is visibly not done. `sig_tentative` does not exist at the pin; the attribute is `#[php_method(tentative)]` and maps to `attr::tentative`.
