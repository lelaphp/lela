# ops.toml extraction report, php-src tag php-8.5.9

Source tree: `~/Projects/php-src-8.5.9` (all `file:line` below are in it).
Output: `ops.toml` from `scripts/extract_ops.py`; the verification log is `data/extract.log`.

## Counts

- Opcodes: 209, numbers 0..210 with 45 and 79 unused (`Zend/zend_vm_opcodes.h`, `ZEND_VM_LAST_OPCODE` at :335).
- Opcode-defining handler lines in `Zend/zend_vm_def.h`: 208 = 207 `ZEND_VM_*HANDLER` + 1 `ZEND_VM_INLINE_HANDLER` (`ZEND_RETURN`, :4508). `ZEND_OP_DATA` has no handler (`ZEND_VM_DEFINE_OP`, :10524). The design's "207" is the count without the inline one.
- Type-specialised handlers (`ZEND_VM_*TYPE_SPEC_HANDLER`, variants of existing opcodes): 42.
- Helpers: 34 = 24 `ZEND_VM_HELPER` + 8 `ZEND_VM_COLD_HELPER` + 1 `ZEND_VM_HOT_HELPER` (`zend_leave_helper`, :2963) + 1 `ZEND_VM_INLINE_HELPER` (:1859). The design's "25" matches no macro subset exactly; the real number is 34.

## a. `result` on the six safepoint opcodes

| opcode | handler (`Zend/zend_vm_def.h`) | body references `opline->result`? | zend_compile.c emission, first arg of `zend_emit_op` | `extended_value` |
|---|---|---|---|---|
| JMP | :3105-3110 | no | `zend_emit_jump` :2354 `zend_emit_op(NULL, ZEND_JMP, ...)`; try/finally :6882 `NULL`; BRK/CONT :5883 `NULL`, rewritten to JMP in `Zend/zend_opcode.c`:1154-1165; GOTO rewritten at `zend_compile.c`:5934-5937 with `SET_UNUSED(opline->result)` | unused (goto clears it, :5937) |
| JMPZ | :3112-3144 | no | only via `zend_emit_cond_jump` :2406 `zend_emit_op(NULL, opcode, cond, NULL)` (callers :6211, :6358, :10546) | unused |
| JMPNZ | :3146-3178 | no | same helper :2406 (callers :6004, :6026, :6088, :6361, :6371, :6619) | unused |
| RETURN | :4508-4584, then `zend_leave_helper` :2963 | no (writes `EX(return_value)`) | :5749 `zend_emit_op(NULL, by_ref ? ZEND_RETURN_BY_REF : ZEND_RETURN, ...)`; :2713 implicit final return, `NULL` | implicit return sets `-1` (:2714) |
| RETURN_BY_REF | :4586-4728 | no | same two sites, `NULL` | yes: `ZEND_RETURNS_VALUE` :4599, `ZEND_RETURNS_FUNCTION` :4624; set at :5751-5757, `-1` at :2714 |
| GENERATOR_RETURN | :4730-4780 | no | never emitted by zend_compile.c; `pass_two` rewrites RETURN/RETURN_BY_REF to it in generator functions, `Zend/zend_opcode.c`:1213-1217, keeping the emitted (unused) result | inherits RETURN's |

The script checks the same two facts mechanically (`data/extract.log`). Verdict: `result` is free to carry a u32 on all six. `extended_value` is not free on RETURN either (the `-1` implicit-return marker), so `result` is the right field for all six, not only RETURN_BY_REF.

## b. Backward jumps emitted by zend_compile.c

Emitted backward (target index below the jump's index):

- `JMPNZ`: `while` (`zend_compile_while` :5986, cond jump :6004 to `opnum_start`), `do-while` (:6010, :6026), `for` (:6060, :6088). All three loops put the condition after the body and jump back with `JMPNZ`.
- `JMP`: `foreach` loop end :6172 `zend_emit_jump(opnum_fetch)`; `continue` inside `foreach` (`ZEND_CONT` :5883, resolved in `zend_opcode.c`:1154-1165 to the loop's `cont` address, which for foreach is `opnum_fetch`, :6180). For `while`/`do-while`/`for` the `cont` address is the condition or loop expression, both after the body, so those `continue`s are forward; every `break` targets the loop end, forward. `goto` to an earlier label (`zend_compile_goto` :5950-5964 emits `ZEND_GOTO`; `zend_resolve_goto_label` :5889-5945 turns it into `ZEND_JMP` at :5934).
- `JMPZ`: never backward from zend_compile.c. `zend_emit_cond_jump` :2387-2409 does not look at `BOOL_NOT`; its only rewrite is smart-branch fusion of a preceding comparison (:2392-2404). `do { } while (!$x)` compiles to `BOOL_NOT` into a TMP then `JMPNZ TMP` backward. The `BOOL_NOT + JMPNZ -> JMPZ` inversion lives in the optimizer: `Zend/Optimizer/block_pass.c`:699-707 (`INV_COND`). A backward `JMPZ` therefore exists only in optimized op arrays.

Other candidates, all forward or not jumps:

- `JMPZ_EX`/`JMPNZ_EX` :10386 (`&&`/`||`), `JMP_SET` :10494, `COALESCE` :10577/:10624, `JMP_NULL` :2577, `BIND_INIT_STATIC_OR_JMP` :5537, `JMP_FRAMELESS` :4759: each is patched to the next opline with `zend_update_jump_target(_to_next)` (:2411-2439). Forward.
- `FE_RESET_R/RW` :6135, op2 patched to the loop end :6175; `FE_FETCH_R/RW` :6140, `extended_value` patched to the loop end :6178 (`pass_two` makes it relative, `zend_opcode.c`:1203-1206). Forward; the backward edge of foreach is the `JMP`.
- `SWITCH_LONG/STRING` :6325 and `MATCH` :6590: jumptable targets are the case bodies compiled after the opline; default `JMP` :6375/:6628 forward.
- `CATCH` :6815: op2 is the next catch clause, forward; `FAST_CALL` :6877 to `finally_op`, forward.
- `FAST_RET` :6890: the handler (`zend_vm_def.h`:8659-8669) jumps to `fast_call + 1`, the opline after the `FAST_CALL` that entered the finally block, which is an earlier index. It is a return from the finally subroutine, not a loop edge: `Zend/Optimizer/zend_cfg.c`:508-514 gives it no successor, like RETURN. Not a site, but the compiler's "every backward jump has a column" assertion must exclude it explicitly.
- `HANDLE_EXCEPTION`: dispatch by try/catch table, not a jump opcode.

So the safepoint set stands at `JMP`, `JMPNZ` (pre-optimizer) plus `JMPZ` (post-optimizer only).

## c. RECV skipping and the JIT entry counter

`Zend/zend_execute.c` `i_init_func_execute_data` :4412-4445:

```
:4426  first_extra_arg = op_array->num_args;
:4427  num_args = EX_NUM_ARGS();
:4428  if (UNEXPECTED(num_args > first_extra_arg)) {            /* extra args: copy them, do NOT skip */
:4432  } else if (EXPECTED((op_array->fn_flags & ZEND_ACC_HAS_TYPE_HINTS) == 0)) {
:4433      /* Skip useless ZEND_RECV and ZEND_RECV_INIT opcodes */
:4435      opline += num_args;      (or EX(opline) += num_args, :4437)
```

So the skip is exactly `num_args` oplines (the arguments actually passed), only when no argument has a type and no extra arguments were passed. `RECV_INIT` oplines for omitted arguments still run, and a call with extra arguments starts at opline 0.

JIT (`ext/opcache/jit/zend_jit.c` `zend_jit_setup_hot_counters_ex` :3219-3231): when `!(fn_flags & ZEND_ACC_HAS_TYPE_HINTS)` it walks past every `ZEND_RECV`/`ZEND_RECV_INIT` (:3224-3228) and installs `zend_jit_func_hot_counter_handler` on that opline (:3230); with type hints it is opline 0. Loop counters go on `ZEND_BB_LOOP_HEADER` block starts (:3233-3243). The tracing JIT does the same skip in `ext/opcache/jit/zend_jit_trace.c`:8985-8999 and marks the opline `ZEND_JIT_TRACE_START_ENTER` (:8993-8999). Note the JIT skips all RECVs statically while the interpreter skips `num_args` of them dynamically; the JIT's entry opline is what the design's "opline after the RECV* prologue" should mean, and `RECV_VARIADIC` is never skipped by either.

## d. Where the source contradicts the design

1. `JMPZ` is not a backward jump as zend_compile.c emits it; `zend_emit_cond_jump` does not invert on `BOOL_NOT` (:2387-2409). Backward `JMPZ` comes only from the optimizer (`block_pass.c`:699-707). Keeping `JMPZ` as a site is still right for optimizer-produced op arrays; the compiler-side assertion should expect only `JMP` and `JMPNZ` pre-optimizer.
2. Counts: 208 opcode handlers + OP_DATA = 209 opcodes; 34 helpers, not 25; plus 42 type-spec handlers.
3. `GENERATOR_RETURN` is not emitted by zend_compile.c; it is a `pass_two` rewrite (`zend_opcode.c`:1213-1217). "As zend_compile.c emits it" must include `pass_two` (which also resolves BRK/CONT/GOTO to `JMP`).
4. `RETURN` also uses `extended_value` (`-1` on the implicit final return, :2714); the reason `result` is the field applies to all six, not to RETURN_BY_REF alone.
5. `FETCH_UNSET` with the global fetch type does not write the symbol table: the helper's `BP_VAR_UNSET` path returns `&EG(uninitialized_zval)` (`zend_vm_def.h`:1767-1768); only `BP_VAR_W` (:1766) and `BP_VAR_RW` (:1777) insert. `ops.toml` marks `FETCH_W`, `FETCH_RW`, `FETCH_FUNC_ARG`, `UNSET_VAR`, `BIND_GLOBAL`, `BIND_STATIC`, `BIND_INIT_STATIC_OR_JMP` as `write_globals`.
6. `FAST_RET` is a backward control transfer the design's list of non-sites does not name (see b).
7. The entry safepoint "after the RECV* prologue" is conditional: with typed parameters or extra arguments php starts at opline 0 (c above).
8. `INIT_FCALL`, `JMP_FRAMELESS`, `NOP`, `JMP`, `BEGIN_SILENCE`, `END_SILENCE`, `DEFINED`, `MAKE_REF`, `ISSET_ISEMPTY_CV`, `SEPARATE`, `FUNC_NUM_ARGS`, `FUNC_GET_ARGS`, `ISSET_ISEMPTY_THIS`, `OP_DATA` are the only opcodes `zend_may_throw_ex` clears unconditionally given their operand kinds; every other opcode carries `throw`, with the condition in `throw_cond`.
