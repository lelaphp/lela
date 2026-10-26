//! `types` at its seam (`jit-design.md` §4.2 *Tested*, §7): a parsed
//! record in, [`build`] over it, `lower::dump::render` out, held to a
//! golden file, for the four programs the scalar core is measured on
//! (§11, days 39–45: `fib`, `mandelbrot`, `nbody`, `spectralnorm`); and
//! one record per class of call-effect kernel, held to the reload after
//! the call.
//!
//! A golden's record is the text of `goldens/<name>.trace`, parsed: the
//! recorder records the program again under the same plan and its record
//! is held to that text, so the fixture is what the recorder writes. The
//! view `build` reads through is the one that recording held, since a
//! `UnitView` is built by a request alone (§5.3). With
//! `PHP_JIT_BLESS=1` set, the test writes both files instead of reading
//! them.

use std::fmt::Write as _;

use php_heap::ZvalType;
use php_unit::{Opcode, OplineIdx};

use super::{
    Check, Def, End, ExitId, Form, Frame, Frees, Grows, GuardAt, Home, Imm, Interrupt, Landing,
    Line, Op, OpId, Operand, Pointee, Repeat, Slot, Slotted, Stored, TraceIr, Types, ValueId,
    build, on_declared_slot,
};
use crate::lower::dump;
use crate::record::{Landed, Stop, TraceRecord};
use crate::testing::goldens::{
    self, ARRAY_WRITE, BINARY_TREES_CHECK, BINARY_TREES_MAKE, DIM_AND, DIM_CONCAT, DIM_DIV,
    DIM_MOD, DIM_OR, DIM_SL, DIM_SR, DIM_XOR, FANNKUCH, FIB, Fixture, HASH_FETCH_W, HASH_IS,
    HASH_ISSET, HASH_READ, HASH_WRITE, INDIRECT_REF_WRITE, MANDELBROT, METHOD_CALL, METHOD_SCOPED,
    NATIVE_CALL, NBODY, NESTED_READ, NESTED_WRITE, NEW_CALL, OBJECT_SCOPED, OBJECT_THIS,
    OBJECT_VALUE, OWNING_LEAVE, REF_CALL, REF_WRITE, SIEVE, SPECTRALNORM, STATIC_CALL,
    STRING_BUILD, STRING_MATCH, held, traced as built,
};
use crate::testing::records::recording;

/// The fixture recorded under its plan: its record held to
/// `<name>.trace`, which is parsed and built through the recording's
/// view, the trace rendered and held to `<name>.ir`.
fn golden(fixture: Fixture) -> Result<TraceIr, String> {
    let recorded = goldens::recorded(fixture)?;
    held(
        &goldens::types().join(format!("{}.trace", fixture.name)),
        &recorded.record.dump(),
    )?;
    let ir = goldens::built(fixture, &recorded)?;
    held(
        &goldens::types().join(format!("{}.ir", fixture.name)),
        &dump::render(&ir),
    )?;
    Ok(ir)
}

/// `fib`, entered: a call into its own root, inlined to the recursive
/// call, the parameter's phi carried by the argument sent.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn fib_enters_and_recurses() -> Result<(), String> {
    let ir = golden(FIB)?;
    assert_eq!(ir.entry.stop, Stop::RecursiveCall);
    assert!(matches!(ir.end, End::Loop(header) if header.repeat == Repeat::Call));
    Ok(())
}

/// `binary_trees`' `check_tree`, entered: the recursive calls by name
/// send `$node[0]` and `$node[1]` to a parameter the trace knows is taken
/// by value, so each `CHECK_FUNC_ARG` is inline, each
/// `FETCH_DIM_FUNC_ARG` a `FETCH_DIM_R` and each `SEND_FUNC_ARG` a
/// `SEND_VAR`, which moves the element into the call, as php's code
/// generator compiles them (`Z/ext/opcache/jit/zend_jit_trace.c:5324-5390`,
/// `:5822-5829`); nothing writes `$node`, so the callee's parameter is
/// the array the send moved. The `=== null` over the element read is a
/// `TYPE_CHECK`, inline, as php's `zend_jit_type_check` compiles it
/// (`Z/ext/opcache/jit/zend_jit_trace.c:5536-5563`).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn binary_trees_sends_its_nodes_elements_by_value() -> Result<(), String> {
    let ir = golden(BINARY_TREES_CHECK)?;
    let opcodes: Vec<(Opcode, Form)> = (ir.ops.iter())
        .filter_map(|op| match op {
            Op::Line(line) => Some((line.opcode, line.form)),
            _ => None,
        })
        .collect();
    for opcode in [
        Opcode::CheckFuncArg,
        Opcode::FetchDimFuncArg,
        Opcode::SendFuncArg,
        Opcode::TypeCheck,
    ] {
        assert!(
            !opcodes.contains(&(opcode, Form::Step)),
            "{opcode:?} stepped: {}",
            dump::render(&ir)
        );
    }
    assert!(opcodes.contains(&(Opcode::CheckFuncArg, Form::Inline)));
    assert!(opcodes.contains(&(Opcode::SendVar, Form::Inline)));
    Ok(())
}

/// `binary_trees`' `make_tree`, from its first call's landing: a
/// `SEND_VAR_EX` of `$d` to the parameter the trace knows is taken by
/// value is a `SEND_VAR`, inline, and leaves `$d` the long it was, as
/// php's trace TSSA keeps the variable's type across it
/// (`Z/ext/opcache/jit/zend_jit_trace.c:2373-2382`).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn binary_trees_sends_a_long_by_value_and_keeps_its_type() -> Result<(), String> {
    let ir = golden(BINARY_TREES_MAKE)?;
    let sends: Vec<&Line> = (ir.ops.iter())
        .filter_map(|op| match op {
            Op::Line(line) if line.opline == OplineIdx(7) || line.opline == OplineIdx(11) => {
                Some(line)
            }
            _ => None,
        })
        .collect();
    assert!(!sends.is_empty(), "{}", dump::render(&ir));
    for send in sends {
        assert_eq!(
            (send.opcode, send.form, send.op1_def),
            (Opcode::SendVar, Form::Inline, None),
            "{}",
            dump::render(&ir)
        );
    }
    Ok(())
}

/// `fib`'s recursive trace leaves through one exit at its comparison's
/// direction in both levels it inlines, as php's `exit_1`, at opline
/// `0003`, serves both: php's stack map is relative to the frame pointer,
/// so the same guard at the same opline with the same types known is one
/// exit (`zend_jit_trace_get_exit_point`, #3203). It is numbered 1, behind
/// the `RECV`'s argument count guard, php's `exit_0`, which the trace
/// numbers though it entered past the prologue, and which its interrupt
/// test leaves through (`jit-design.md` §4.2, #3269).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn fib_leaves_both_levels_through_one_exit_per_place() -> Result<(), String> {
    let ir = golden(FIB)?;
    let directions: Vec<(Frame, ExitId)> = (ir.guards.iter())
        .filter(|guard| matches!(guard.check, Check::Direction { .. }))
        .filter_map(|guard| match guard.at {
            GuardAt::Within(at) => match ir.ops.get(at.index()) {
                Some(Op::Line(line)) => Some((line.frame, guard.exit)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        directions,
        [(Frame(0), ExitId(1)), (Frame(1), ExitId(1))],
        "{directions:?}"
    );
    let landing = ir.exits.get(1).map(|exit| (exit.frame, exit.opline));
    assert_eq!(landing, Some((Frame(0), Some(OplineIdx(3)))));
    assert!(
        matches!(ir.end, End::Loop(header) if header.interrupt == Interrupt::Exit(ExitId(0))),
        "{}",
        dump::render(&ir)
    );
    Ok(())
}

/// `fib`'s return trace loops by recursive return and takes no interrupt
/// test, and numbers no exit for one, as php's `zend_jit_trace` sets no
/// `timeout_exit_addr` for `ZEND_JIT_TRACE_STOP_RECURSIVE_RET`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:7214-7231`, #3314). The pinned
/// php's `TRACE 3 exit info` under the trace-dump regime lists three
/// exits, `exit_0: 0008/0000/3/VM`, `exit_1: 0004/0003/1` and
/// `exit_2: 0012/0004/4`, and no interrupt exit after them: the stack
/// check before the second call, the comparison's direction in the
/// callee and the addition's overflow, each an opline later here, past
/// the `VERIFY_RETURN_TYPE` php's optimiser drops from `return $n`. The
/// trace takes those three and #3176's two, which php's function-level
/// inference proves need no guard: the entry's guard on what the call
/// before returned, exit 0, and the subtraction's overflow before the
/// second call, exit 2. `$n` is held long by its declaration, as php's
/// inference holds it, `return $n;`'s `VERIFY_RETURN_TYPE` keeping it as
/// it is (`types::opcodes::written_after_entry`), so no guard reads it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_loop_by_recursive_return_takes_no_interrupt_test() -> Result<(), String> {
    let ir = built(
        &[("record.function", "fib"), ("record.start", "return")],
        "function fib(int $n): int
        {
            if ($n < 2) {
                return $n;
            }
            return fib($n - 1) + fib($n - 2);
        }
        echo fib(6), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let End::Loop(header) = ir.end else {
        return Err(format!("no loop: {rendered}"));
    };
    assert_eq!(
        (header.repeat, header.interrupt),
        (Repeat::Return, Interrupt::None),
        "{rendered}"
    );
    let inferred = [0, 2];
    let phps: Vec<(Frame, Option<u32>, Grows)> = (ir.exits.iter().enumerate())
        .filter(|(at, _)| !inferred.contains(at))
        .map(|(_, exit)| (exit.frame, exit.opline.map(|at| at.0), exit.grows))
        .collect();
    assert_eq!(
        (ir.exits.len(), phps.as_slice()),
        (
            3 + inferred.len(),
            [
                (Frame(0), Some(9), Grows::Never),
                (Frame(1), Some(5), Grows::Side),
                (Frame(0), Some(13), Grows::Side),
            ]
            .as_slice()
        ),
        "{rendered}"
    );
    Ok(())
}

/// A `foreach` over a packed array is fetched inline, as php's
/// `zend_jit_fe_fetch` compiles it, and writes its key as a long, as php's
/// inference types what `FE_FETCH_R` writes from the array's layout, so
/// no guard holds it; the exit its end takes knows nothing of it, as
/// php's stack map types what an op writes only once the op's exits are
/// taken; and the `+=` that reads it leaves through exit 4 at `0005`,
/// as php's trace takes `exit_0` to `exit_2` at `0003`, `exit_3` at
/// `0007` and `exit_4` at `0005` (#3203).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_packed_arrays_key_is_a_long_no_guard_holds() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f($a) { $s = 0; foreach ($a as $k => $v) { $s += $k; } return $s; }
        echo f([1, 2, 3, 4, 5, 6, 7, 8, 9]), \"\\n\";",
    )?;
    let fetch = (ir.ops.iter())
        .find_map(|op| match op {
            Op::Line(line) if line.opcode == Opcode::FeFetchR => Some(line),
            _ => None,
        })
        .ok_or("no FE_FETCH_R")?;
    assert_eq!(fetch.form, Form::Inline, "{}", dump::render(&ir));
    let key = fetch.out.ok_or("FE_FETCH_R writes no key")?;
    assert_eq!(ir.values[key.0 as usize].types, Types::LONG);
    let on_key = (ir.guards.iter()).any(|guard| {
        matches!(guard.check, Check::Type { value, .. } | Check::Deref { value, .. } if value == key)
    });
    assert!(!on_key, "{}", dump::render(&ir));
    let oplines: Vec<Option<u32>> = (ir.exits.iter().take(5))
        .map(|exit| exit.opline.map(|at| at.0))
        .collect();
    assert_eq!(
        oplines,
        [Some(3), Some(3), Some(3), Some(7), Some(5)],
        "{}",
        dump::render(&ir)
    );
    let slot = ir.values[key.0 as usize].slot;
    assert!(
        !ir.exits[3].known.iter().any(|known| known.slot == slot),
        "{:?}",
        ir.exits[3]
    );
    Ok(())
}

/// A trace that does not loop holds a guard on what its frame entered
/// with before the op that first reads it, as php's code generator
/// holds a loop-invariant type at entry only where the trace loops by
/// its own loop or recursive call (`Z/ext/opcache/jit/zend_jit_trace.c:4237-4272`),
/// and a side's guard at the read (#3203): the array's layout among
/// them, held where the array is read by a long key, as php's side holds
/// `MAY_BE_PACKED_GUARD` there; and `$n`, read once. `$i`, which php's
/// allocator loads into a register as the trace enters, read by the
/// comparison, the fetch and the increment, is held at entry, as php's
/// code holds a `ZREG_LOAD` value there (`:4283-4310`, #3270).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_trace_that_does_not_loop_holds_its_guards_at_the_read() -> Result<(), String> {
    let (mut recorded, _) = recording(
        &IN_F,
        "function f($a, $n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $a[$i]; } return $s; }
        echo f([1, 2, 3], 3), \"\\n\";",
    )?;
    let looping = build(&recorded.record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    let at_entry = |ir: &TraceIr| ir.guards.iter().filter(|g| g.at == GuardAt::Entry).count();
    assert!(at_entry(&looping) > 0, "{}", dump::render(&looping));
    recorded.record.stop = Stop::Link(crate::table::TraceId::new(1).ok_or("no trace 1")?);
    let linked = build(&recorded.record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    let entered: Vec<u32> = (linked.guards.iter())
        .filter(|guard| guard.at == GuardAt::Entry)
        .filter_map(|guard| match guard.check {
            Check::Type { value, .. } => Some(linked.values[value.0 as usize].slot.var),
            _ => None,
        })
        .collect();
    assert_eq!(entered, [3], "{}", dump::render(&linked));
    assert_eq!(at_entry(&linked), 1, "{}", dump::render(&linked));
    let layout = (linked.guards.iter())
        .find(|guard| matches!(guard.check, Check::Layout { .. }))
        .map(|guard| guard.at);
    assert!(
        matches!(layout, Some(GuardAt::Before(_))),
        "{layout:?}: {}",
        dump::render(&linked)
    );
    Ok(())
}

/// fannkuch's side trace at `0049` (`TRACE 10`, side trace 9/2 under the
/// pinned php at `opcache.optimization_level=0`, whose op arrays are this
/// engine's) does not loop, and php's allocator loads `$maxFlips` at its
/// entry, read by `0054`'s `IS_SMALLER` and written by `0056`'s `ASSIGN`
/// (its `TRACE 10 Live Ranges` list `#5.CV5($maxFlips) load`), so php
/// holds its type there: `exit_0` and `exit_1` land at `0049`, the guard
/// on `$maxFlips` and then the one on `$flips`, which `PRE_INC` reads
/// once (#3270). The record is the engine's own for that trace.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_value_phps_allocator_loads_is_held_at_entry() -> Result<(), String> {
    let recorded = goldens::recorded(FANNKUCH)?;
    let root = crate::record::func_name(recorded.record.root);
    let text = "start side 9/3\nroot {root}\nentered 1:array 11:long 12:long
op {root} 49 long - - -\nop {root} 50 array.packed - - -\nop {root} 51 long long - -
op {root} 52 long - - fell\nop {root} 54 long long - fell\nop {root} 56 long long - -
op {root} 57 long - - -\nop {root} 58 long - - taken\nop {root} 62 long - - -
op {root} 63 long - - -\nop {root} 64 long long - -\nop {root} 65 - - - -
op {root} 93 - - - taken\nop {root} 66 long long - taken\nop {root} 72 array.packed - - -
op {root} 73 long long - -\nop {root} 74 long - - -\nop {root} 75 - - - -
link-at {root} 82\nstop link 6\n"
        .replace("{root}", &root);
    let record = TraceRecord::parse(&text).map_err(|e| format!("{e:?}"))?;
    let ir = build(&record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    let first: Vec<Option<(u32, Option<u32>, bool)>> = (ir.guards.iter().take(2))
        .map(|guard| match guard.check {
            Check::Type { value, .. } => {
                let var = ir.values[value.0 as usize].slot.var;
                let lands = ir.exits[usize::from(guard.exit.0)].opline.map(|at| at.0);
                Some((var, lands, guard.at == GuardAt::Entry))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        first,
        [Some((5, Some(49), true)), Some((9, Some(49), false))],
        "{}",
        dump::render(&ir)
    );
    let exits: Vec<u16> = ir.guards.iter().take(2).map(|guard| guard.exit.0).collect();
    assert_eq!(exits, [0, 1], "{}", dump::render(&ir));
    Ok(())
}

/// An element write and its `OP_DATA` are two ops of php's trace SSA, so
/// `$a[$k] = $k` reads `$k` twice, as the key and as the value the
/// `OP_DATA` takes an owner of, and php's allocator loads it at the entry
/// of a trace that does not loop where nothing else reads it
/// (`zend_jit_trace_allocate_registers`, `Z/ext/opcache/jit/zend_jit_trace.c:3290-3300`;
/// #3270).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_key_written_as_its_own_value_is_read_twice() -> Result<(), String> {
    let (mut recorded, _) = recording(
        &IN_F,
        "function f($a, $k, $n) { for ($i = 0; $i < $n; $i++) { $a[$k] = $k; } return $a; }
        echo f([1, 2, 3], 1, 3)[1], \"\\n\";",
    )?;
    recorded.record.stop = Stop::Link(crate::table::TraceId::new(1).ok_or("no trace 1")?);
    let linked = build(&recorded.record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    let entered: Vec<u32> = (linked.guards.iter())
        .filter(|guard| guard.at == GuardAt::Entry)
        .filter_map(|guard| match guard.check {
            Check::Type { value, .. } => Some(linked.values[value.0 as usize].slot.var),
            _ => None,
        })
        .collect();
    assert!(entered.contains(&1), "{}", dump::render(&linked));
    Ok(())
}

/// A `match` takes an exit per opline its table may jump to but the one
/// the trace goes on at, the default's first and then each arm's in the
/// table's order, as php's `zend_jit_switch` takes them: its trace's
/// `exit_4` at `0005`, the `UnhandledMatchError`, and `exit_5` at `0008`,
/// the second arm (#3203). Each lands where the jump went.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_tables_jump_takes_an_exit_per_arm_as_phps_does() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f($keys) { $s = 0; foreach ($keys as $k) { $s += match ($k) { 'a' => 1, 'b' => 2 }; } return $s; }
        echo f(['a', 'a', 'a', 'b', 'b', 'b']), \"\\n\";",
    )?;
    let arms: Vec<(u32, u16, Option<u32>)> = (ir.guards.iter())
        .filter_map(|guard| match guard.check {
            Check::Arm { opline } => Some((
                opline.0,
                guard.exit.0,
                ir.exits[usize::from(guard.exit.0)].opline.map(|at| at.0),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        arms,
        [(5, 4, Some(5)), (8, 5, Some(8))],
        "{}",
        dump::render(&ir)
    );
    let stands = (ir.guards.iter()).any(|guard| matches!(guard.check, Check::Stands { .. }));
    assert!(!stands, "{}", dump::render(&ir));
    Ok(())
}

/// `mandelbrot`'s escape loop, in the file's own code: every variable a
/// symbol table reaches, guarded at each read and never narrowed.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn mandelbrot_loops_in_file_scope() -> Result<(), String> {
    let ir = golden(MANDELBROT)?;
    assert_eq!(ir.entry.stop, Stop::Loop);
    Ok(())
}

/// `nbody`'s inner loop of `advance`: packed arrays by reference read
/// and written by a long key, around a call to `sqrt`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn nbody_advances_in_a_loop() -> Result<(), String> {
    let ir = golden(NBODY)?;
    assert_eq!(ir.entry.stop, Stop::Loop);
    Ok(())
}

/// A native's call in a loop: its `INIT_FCALL` inline, the call pushed
/// in place as php's `zend_jit_push_call_frame` pushes a native's, its
/// sends inline, and its `DO_ICALL` served by the handler (#2963).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_natives_call_is_pushed_in_place() -> Result<(), String> {
    let ir = golden(NATIVE_CALL)?;
    let pushed = ir.ops.iter().any(
        |op| matches!(op, Op::InitCall { plain: Some(plain), .. } if plain.callee().is_none()),
    );
    assert!(pushed, "{}", dump::render(&ir));
    let init = (ir.ops.iter()).find_map(|op| match op {
        Op::Line(line) if line.opcode == Opcode::InitFcall => Some(line.form),
        _ => None,
    });
    assert_eq!(init, Some(Form::Inline), "{}", dump::render(&ir));
    Ok(())
}

/// A loop over strings: `$s === ''` fused to its jump, `$s !== "zz"`
/// written, `strlen($s)` and `(bool) $i` each inline, as php's code
/// generator compiles them (`zend_jit_identical`, `zend_jit_strlen`,
/// `zend_jit_bool_jmpznz`), where each was stepped (#3321).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn strings_are_compared_and_measured_inline() -> Result<(), String> {
    let ir = golden(STRING_MATCH)?;
    let rendered = dump::render(&ir);
    for opcode in [
        Opcode::IsIdentical,
        Opcode::IsNotIdentical,
        Opcode::Strlen,
        Opcode::Bool,
    ] {
        let form = (ir.ops.iter()).find_map(|op| match op {
            Op::Line(line) if line.opcode == opcode => Some(line.form),
            _ => None,
        });
        assert_eq!(form, Some(Form::Inline), "{opcode:?} in\n{rendered}");
    }
    Ok(())
}

/// A blob built in a loop: the path's two concatenations of strings, each
/// rope piece that is a string, `ROPE_INIT`, `ROPE_ADD` and `ROPE_END`
/// alike, and `.=` of the line onto the blob, inline, as php's
/// `zend_jit_concat`, `zend_jit_rope` and `zend_jit_assign_op` compile
/// them; the piece that is a long is the handler's, as php's is (#3321).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn ropes_and_appends_of_strings_are_inline() -> Result<(), String> {
    let ir = golden(STRING_BUILD)?;
    let rendered = dump::render(&ir);
    let forms: Vec<(Opcode, Form)> = (ir.ops.iter())
        .filter_map(|op| match op {
            Op::Line(line) => Some((line.opcode, line.form)),
            _ => None,
        })
        .collect();
    for opcode in [
        Opcode::Concat,
        Opcode::RopeInit,
        Opcode::RopeEnd,
        Opcode::AssignOp,
    ] {
        assert!(
            forms.contains(&(opcode, Form::Inline)),
            "{opcode:?} in\n{rendered}"
        );
    }
    // `$i`, the one long piece.
    let stepped = (forms.iter())
        .filter(|(opcode, form)| *opcode == Opcode::RopeAdd && *form == Form::Step)
        .count();
    assert_eq!(stepped, 1, "{rendered}");
    Ok(())
}

/// A callee's `RETURN` over variables that own something, an array its
/// caller keeps too and a string it built, is left in place and names
/// both to be freed, as php's trace frees each before
/// `zend_jit_leave_func` (#3313).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_leave_over_owning_variables_is_made_in_place() -> Result<(), String> {
    let ir = golden(OWNING_LEAVE)?;
    let rendered = dump::render(&ir);
    let leave = (ir.ops.iter()).find_map(|op| match op {
        Op::Line(line) if line.opcode == Opcode::Return => Some(line),
        _ => None,
    });
    let leave = leave.ok_or_else(|| format!("no RETURN in\n{rendered}"))?;
    // `$a` is CV0, `$i` CV1 and `$s` CV2.
    let owners = (super::Vars::default().with(0)).and_then(|vars| vars.with(2));
    assert_eq!(leave.form, Form::Inline, "{rendered}");
    assert!(
        matches!(leave.imm, super::Imm::Leave { owners: super::Owners::Each(vars), .. } if Some(vars) == owners),
        "{rendered}"
    );
    Ok(())
}

/// The element writes of `array_write` and `sieve`, a scalar by a long
/// key to a packed array recorded packed, are inline, as php's
/// `zend_jit_assign_dim` compiles them, with no layout guard, which php
/// adds only to an array recorded hash
/// (`Z/ext/opcache/jit/zend_jit_trace.c:1871-1893`): the array is of
/// either layout, which the write tests. They take no exit, since php's
/// code calls its helpers where it cannot write in place, and the handler
/// serves it here; the handler may throw, so they unwind (#3228).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_element_write_is_inline_and_takes_no_exit() -> Result<(), String> {
    for fixture in [ARRAY_WRITE, SIEVE] {
        let ir = golden(fixture)?;
        assert_eq!(ir.entry.stop, Stop::Loop);
        let writes: Vec<_> = (ir.ops.iter())
            .filter_map(|op| match op {
                Op::Line(line) if line.opcode == Opcode::AssignDim => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(writes.len(), 1, "{}", dump::render(&ir));
        for line in writes {
            assert_eq!(line.form, Form::Inline, "{}", dump::render(&ir));
            assert!(line.exit.is_none() && line.unwind.is_some());
            let Operand::Value(array) = line.op1 else {
                return Err(format!("{}: a write to no array", fixture.name));
            };
            let types = ir.values[array.0 as usize].types;
            assert!(types.meets(Types::PACKED) && types.meets(Types::HASH));
        }
        assert!(
            !ir.guards
                .iter()
                .any(|guard| matches!(guard.check, Check::Layout { .. })),
            "{}",
            dump::render(&ir)
        );
    }
    Ok(())
}

/// `spectralnorm`'s inner loop of `eval_A_times_u`, which inlines its
/// call to `eval_A`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn spectralnorm_inlines_its_call() -> Result<(), String> {
    let ir = golden(SPECTRALNORM)?;
    assert_eq!(ir.entry.stop, Stop::Loop);
    assert!(ir.ops.iter().any(|op| matches!(op, Op::Enter { .. })));
    Ok(())
}

/// An inline fetch takes its exits in the order php's
/// `zend_jit_fetch_dim_read` takes them: the exit its code opens with,
/// php's `ZEND_JIT_EXIT_TO_VM`, before the guards on the element it
/// copies, as `spectralnorm`'s `$u[$j]` at `0012` takes php's `exit_9`
/// before `exit_10` and `exit_11` at `0013`; and a layout guard leaves
/// through an exit of its own, php's `ZEND_JIT_EXIT_PACKED_GUARD`,
/// which no type guard's place shares (#3203).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fetch_takes_its_exits_in_phps_order() -> Result<(), String> {
    let ir = golden(SPECTRALNORM)?;
    let fetch = (ir.ops.iter()).find_map(|op| match op {
        Op::Line(line) if line.opcode == Opcode::FetchDimR && line.form == Form::Inline => {
            Some(line)
        }
        _ => None,
    });
    let fetch = fetch.ok_or("no inline FETCH_DIM_R")?;
    let own = fetch.exit.ok_or("the fetch has no exit of its own")?;
    let at = (ir.ops.iter()).position(|op| matches!(op, Op::Line(line) if line == fetch));
    let at = OpId(u32::try_from(at.ok_or("no fetch")?).map_err(|e| e.to_string())?);
    let elements: Vec<ExitId> = (ir.guards.iter())
        .filter(|guard| guard.at == GuardAt::Within(at))
        .map(|guard| guard.exit)
        .collect();
    assert_eq!(
        elements,
        [ExitId(own.0 + 1), ExitId(own.0 + 2)],
        "{}",
        dump::render(&ir)
    );
    let layouts: Vec<ExitId> = (ir.guards.iter())
        .filter(|guard| matches!(guard.check, Check::Layout { .. }))
        .map(|guard| guard.exit)
        .collect();
    let shared = (ir.guards.iter())
        .any(|guard| !matches!(guard.check, Check::Layout { .. }) && layouts.contains(&guard.exit));
    assert!(!layouts.is_empty() && !shared, "{}", dump::render(&ir));
    assert!(
        layouts
            .iter()
            .all(|exit| ir.exits[usize::from(exit.0)].layout)
    );
    Ok(())
}

/// php's stack check (`zend_jit_stack_check`, #3203): a loop whose calls
/// push frames holds the stack's room for them once, at its entry, its
/// first exit, back to the interpreter at the loop's first opline, as
/// php's trace for `$s += g($i)` takes `exit_0` `0009/----/0/VM`; a trace
/// that does not loop holds it at each `INIT_*` instead, in
/// `zend_jit_push_call_frame`, an `INIT_FCALL` of a user function taking no
/// exit of its own, as `zend_jit_init_fcall` holds no guard on such a
/// callee (`Z/ext/opcache/jit/zend_jit_ir.c:8790-8906`, #3230).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_trace_holds_the_stacks_room_where_php_does() -> Result<(), String> {
    let (mut recorded, _) = recording(
        &IN_F,
        "function g($x) { return $x + 1; }
        function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += g($i); } return $s; }
        echo f(10), \"\\n\";",
    )?;
    let rooms = |ir: &TraceIr| -> Vec<(GuardAt, u16)> {
        (ir.guards.iter())
            .filter(|guard| matches!(guard.check, Check::Room { .. }))
            .map(|guard| (guard.at, guard.exit.0))
            .collect()
    };
    let looping = build(&recorded.record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    assert_eq!(
        rooms(&looping),
        [(GuardAt::Entry, 0)],
        "{}",
        dump::render(&looping)
    );
    let first = looping.exits.first().ok_or("no exit")?;
    assert_eq!(
        (first.opline, first.grows, first.known.len()),
        (Some(OplineIdx(9)), Grows::Never, 0)
    );
    recorded.record.stop = Stop::Link(crate::table::TraceId::new(1).ok_or("no trace 1")?);
    let linked = build(&recorded.record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    let init = (linked.ops.iter())
        .position(|op| matches!(op, Op::Line(line) if line.opcode == Opcode::InitFcall))
        .and_then(|at| u32::try_from(at).ok())
        .map(OpId)
        .ok_or("no INIT_FCALL")?;
    let Some(Op::Line(line)) = linked.ops.get(init.index()) else {
        return Err("no INIT_FCALL".to_owned());
    };
    assert_eq!(line.exit, None, "{}", dump::render(&linked));
    assert!(
        matches!(rooms(&linked).as_slice(), [(GuardAt::Before(at), _)] if *at == init),
        "{}",
        dump::render(&linked)
    );
    Ok(())
}

/// A side trace enters with the types its parent's exit knew, an array's
/// without its layout, and holds the layout at its first read by a long
/// key, as php's holds `MAY_BE_PACKED_GUARD` (#3255's ruling, #3262).
/// #3196's program: php's `side trace 1/6` takes `exit_0` at `0007`, the
/// fetch's exit to the interpreter, then `exit_1` there, the layout
/// guard's, then the element's two at `0008`, and holds nothing at entry.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_side_holds_an_arrays_layout_at_its_first_read() -> Result<(), String> {
    use php_engine::tier::Sample;

    use crate::record::{Entered, Event, Start, TraceRecord};
    use crate::table::TraceId;

    let (recorded, _) = recording(
        &IN_F,
        "function f($a, $n) { $s = 0; for ($i = 0; $i < $n; $i++) { if ($i > 50) { $s += $a[$i]; } else { $s -= $a[$i]; } } return $s; }
        echo f([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 10), \"\\n\";",
    )?;
    let f = recorded.record.root;
    let of = |kind, packed| {
        Some(Sample {
            kind,
            indirect: false,
            reference: false,
            packed,
            class: None,
        })
    };
    let op = |at: u32, op1, op2| Event::Op {
        func: f,
        opline: OplineIdx(at),
        op1,
        op2,
        data: None,
        branch: None,
    };
    let parent = TraceId::new(1).ok_or("no trace 1")?;
    let side = TraceRecord {
        start: Start::Side {
            parent,
            exit: ExitId(6),
        },
        root: f,
        entered: [
            ZvalType::Array,
            ZvalType::Long,
            ZvalType::Long,
            ZvalType::Long,
        ]
        .into_iter()
        .zip(0..)
        .map(|(tag, var)| Entered { var, tag })
        .collect(),
        landed: Landed::Named,
        events: vec![
            op(7, of(ZvalType::Array, true), of(ZvalType::Long, false)),
            op(8, of(ZvalType::Long, false), of(ZvalType::Long, false)),
            op(9, None, None),
            op(12, of(ZvalType::Long, false), None),
        ],
        stop: Stop::Link(parent),
        link_at: Some((f, OplineIdx(13))),
    };
    let ir = build(&side, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    let layouts: Vec<(GuardAt, u16)> = (ir.guards.iter())
        .filter(|guard| matches!(guard.check, Check::Layout { .. }))
        .map(|guard| (guard.at, guard.exit.0))
        .collect();
    assert_eq!(
        layouts,
        [(GuardAt::Before(OpId(0)), 1)],
        "{}",
        dump::render(&ir)
    );
    let entry = (ir.guards.iter()).any(|guard| guard.at == GuardAt::Entry);
    assert!(!entry, "{}", dump::render(&ir));
    let first: Vec<(Option<u32>, Grows, bool)> = (ir.exits.iter().take(4))
        .map(|exit| (exit.opline.map(|at| at.0), exit.grows, exit.layout))
        .collect();
    assert_eq!(
        first,
        [
            (Some(7), Grows::Never, false),
            (Some(7), Grows::Side, true),
            (Some(8), Grows::Side, false),
            (Some(8), Grows::Side, false),
        ],
        "{}",
        dump::render(&ir)
    );
    Ok(())
}

/// A native's call returns what its declaration says, as php's trace
/// TSSA types what a `DO_ICALL` writes by the function its record names
/// (`Z/ext/opcache/jit/zend_jit_trace.c:2626-2650`): `strcmp()` returns
/// an `int`, so the `+=` that reads what it wrote holds no guard on it, as
/// php's holds none (#3264).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_natives_call_returns_what_it_declares() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += strcmp(\"a\", \"b\"); } return $s; }
        echo f(10), \"\\n\";",
    )?;
    let at = stepped(&ir, Opcode::DoIcall)?;
    let Some(Op::Line(call)) = ir.ops.get(at.index()) else {
        return Err("no DO_ICALL".to_owned());
    };
    let out = call.out.ok_or("the call keeps no result")?;
    assert_eq!(
        ir.values[out.0 as usize].types,
        Types::LONG,
        "{}",
        dump::render(&ir)
    );
    let on_out = (ir.guards.iter()).any(|guard| {
        matches!(guard.check, Check::Type { value, .. } | Check::Deref { value, .. } if value == out)
    });
    assert!(!on_out, "{}", dump::render(&ir));
    Ok(())
}

/// A frameless call returns what its native declares, as php's trace
/// TSSA types what one writes through the call map it builds for every op
/// array a trace runs through (`zend_get_func_info`,
/// `Z/Zend/Optimizer/zend_func_info.c:132-150`): `class_exists()`
/// returns a `bool`, so what it wrote is no reference the clobber rule
/// reads again, and an opline that reads it holds it as php's does. The
/// engine with no extension this test serves on has no `substr()`; the
/// pinned php's trace of `strlen(substr($s, 1))` in a loop holds no guard
/// on its `string` at the `STRLEN` (#3438).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_frameless_call_returns_what_its_native_declares() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f(int $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += (int) class_exists('stdClass'); } return $t; }
        echo f(10), \"\\n\";",
    )?;
    let at = stepped(&ir, Opcode::FramelessIcall1)?;
    let Some(Op::Line(call)) = ir.ops.get(at.index()) else {
        return Err("no FRAMELESS_ICALL_1".to_owned());
    };
    let out = call.out.ok_or("the call keeps no result")?;
    assert_eq!(
        ir.values[out.0 as usize].types,
        Types::BOOL,
        "{}",
        dump::render(&ir)
    );
    let again = (ir.values.iter())
        .any(|value| value.def == Def::Reload(at) && value.slot == ir.values[out.0 as usize].slot);
    assert!(!again, "{}", dump::render(&ir));
    Ok(())
}

/// php's code generator holds an `ASSIGN_DIM`'s variable before the
/// value its `OP_DATA` writes (`CHECK_OP1_TRACE_TYPE`, then
/// `CHECK_OP1_DATA_TRACE_TYPE`, `Z/ext/opcache/jit/zend_jit_trace.c:5064-5101`),
/// where its TSSA marks them the other way round. In a file's own code
/// the variable's guard leaves no type known, so the guard on the value
/// after it leaves through the same exit: the pinned php's trace of this
/// loop takes one exit, `exit_2: 0010/----/0`, at `$a[0] = class_exists($c)`
/// for both, where holding the value first took two (#3438).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_element_writes_guards_are_held_in_phps_order() -> Result<(), String> {
    let ir = built(
        &[("record.function", ""), ("record.start", "loop")],
        "$a = [1, 2];
        $c = $argc > 5 ? 'q' : 'stdClass';
        for ($i = 0; $i < 10; $i++) { $a[0] = class_exists($c); }
        echo $a[0], \"\\n\";",
    )?;
    let at = (ir.ops.iter())
        .position(|op| matches!(op, Op::Line(line) if line.opcode == Opcode::AssignDim))
        .and_then(|at| u32::try_from(at).ok())
        .map(OpId)
        .ok_or_else(|| format!("no ASSIGN_DIM:\n{}", dump::render(&ir)))?;
    let Some(Op::Line(write)) = ir.ops.get(at.index()) else {
        return Err("no ASSIGN_DIM".to_owned());
    };
    let held: Vec<(Operand, ExitId)> = (ir.guards.iter())
        .filter(|guard| guard.at == GuardAt::Before(at))
        .filter_map(|guard| match guard.check {
            Check::Type { value, .. } => Some((Operand::Value(value), guard.exit)),
            _ => None,
        })
        .collect();
    // Both guards stand, so the order below is held to something: with
    // neither, the comparison is of two empty lists.
    assert_eq!(held.len(), 2, "{}", dump::render(&ir));
    let exit = held.first().map(|(_, exit)| *exit);
    assert_eq!(
        held,
        exit.map(|exit| vec![(write.op1, exit), (write.data, exit)])
            .unwrap_or_default(),
        "{}",
        dump::render(&ir)
    );
    Ok(())
}

/// A fetched element's guards leave past the fetch with the element it
/// copied, a call under way or none, as php's
/// `zend_jit_guard_fetch_result_type` takes them for the opline after it
/// and `zend_jit_trace_get_exit_point` restores the call
/// (`Z/ext/opcache/jit/zend_jit_ir.c:8067-8085`,
/// `Z/ext/opcache/jit/zend_jit_trace.c:144-147`): the pinned php's trace
/// of `strtoupper($a[1])` in a loop takes `exit_5` and `exit_6`,
/// `0007/.../CALL` with `X4` copied, at the fetch, where this tier left
/// for the fetch again through the exit of its array's guard, and
/// `binary_trees`' `check_tree` takes php's `exit_6` to `exit_9`, two
/// per level it inlines (#3438).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fetched_elements_guard_leaves_past_the_fetch_under_a_call() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f(array $a, int $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += strcmp($a[1], 'b'); } return $t; }
        echo f(['a', 'bc'], 10), \"\\n\";",
    )?;
    let at = inline_at(&ir, Opcode::FetchDimR)?;
    let Some(Op::Line(fetch)) = ir.ops.get(at.index()) else {
        return Err("no FETCH_DIM_R".to_owned());
    };
    let elements: Vec<ExitId> = (ir.guards.iter())
        .filter(|guard| guard.at == GuardAt::Within(at))
        .map(|guard| guard.exit)
        .collect();
    assert_eq!(elements.len(), 2, "{}", dump::render(&ir));
    let past = OplineIdx(fetch.opline.0 + 1);
    for exit in &elements {
        let spec = &ir.exits[usize::from(exit.0)];
        assert_eq!(
            (spec.opline, spec.landing),
            (Some(past), Landing::RESTORE_CALL),
            "{}",
            dump::render(&ir)
        );
        assert!(
            (spec.stores.iter()).any(|store| matches!(store.from, Stored::Copied { .. })),
            "{}",
            dump::render(&ir)
        );
    }
    assert_ne!(elements[0], elements[1], "{}", dump::render(&ir));
    Ok(())
}

/// A fetched element's guard under a call under way, over a temporary
/// array, leaves past the fetch with the call to restore and the
/// temporary to free, both: the two halves of the landing
/// `a_fetched_elements_guard_leaves_past_the_fetch_under_a_call` and
/// `a_fetched_elements_guard_frees_the_temporary_it_read` take apart, so
/// a landing that dropped the free under a call would leak the array, or
/// call its destructor late, at every such exit (#3438).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fetched_elements_guard_under_a_call_frees_the_temporary_it_read() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function g() { $a = []; $a[] = 'b'; return $a; }
        function f(int $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += strcmp(g()[0], 'b'); } return $t; }
        echo f(10), \"\\n\";",
    )?;
    let freeing_under_a_call = Landing {
        frees: Frees {
            op1: true,
            op2: false,
        },
        restores_call: true,
    };
    assert!(
        ir.exits
            .iter()
            .any(|exit| exit.landing == freeing_under_a_call),
        "{}",
        dump::render(&ir)
    );
    Ok(())
}

/// `FE_FETCH_R` takes the exit the way out of the loop leaves through
/// before its array's packed guard, as php's code generator takes it
/// before `zend_jit_fe_fetch`, whose packed guard follows
/// (`Z/ext/opcache/jit/zend_jit_trace.c:6320-6350`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:13839-13843`): the pinned php's trace
/// through this `foreach` takes `exit_8: 0005` before `exit_9: 0002`
/// (#3438).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_foreach_leaves_its_loop_before_it_holds_the_arrays_layout() -> Result<(), String> {
    let ir = built(
        &[("record.function", ""), ("record.start", "loop")],
        "function f($a) { foreach ($a as $v) { return $v; } return 0; }
        $x = [[1, 2], [3]];
        for ($i = 0; $i < 4; $i++) { echo f($x[$i % 2]), \"\\n\"; }",
    )?;
    let at = inline_at(&ir, Opcode::FeFetchR)?;
    let exit_of = |held: fn(&Check) -> bool| {
        (ir.guards.iter())
            .find(|guard| {
                matches!(guard.at, GuardAt::Before(op) | GuardAt::Within(op) if op == at)
                    && held(&guard.check)
            })
            .map(|guard| guard.exit)
    };
    let direction = exit_of(|check| matches!(check, Check::Direction { .. }));
    let layout = exit_of(|check| matches!(check, Check::Layout { .. }));
    let (Some(direction), Some(layout)) = (direction, layout) else {
        return Err(format!(
            "no direction and layout at the fetch:\n{}",
            dump::render(&ir)
        ));
    };
    assert!(direction.0 < layout.0, "{}", dump::render(&ir));
    Ok(())
}

/// The place of the first stepped opline of `opcode`.
fn stepped(ir: &TraceIr, opcode: Opcode) -> Result<OpId, String> {
    ir.ops
        .iter()
        .position(
            |op| matches!(op, Op::Line(line) if line.opcode == opcode && line.form == Form::Step),
        )
        .and_then(|at| u32::try_from(at).ok())
        .map(OpId)
        .ok_or_else(|| format!("no stepped {opcode:?} in\n{}", dump::render(ir)))
}

/// The variables of the root's frame read again after the op at `at`.
fn reloaded(ir: &TraceIr, at: OpId) -> Vec<u32> {
    ir.values
        .iter()
        .filter(|value| value.def == Def::Reload(at) && value.slot.frame == Frame::ROOT)
        .map(|value| value.slot.var)
        .collect()
}

/// What the first opline past `at` that reads variable `var` reads it as.
fn next_read(ir: &TraceIr, at: OpId, var: u32) -> Option<ValueId> {
    ir.ops.iter().skip(at.index() + 1).find_map(|op| {
        let Op::Line(line) = op else { return None };
        [line.op1, line.op2, line.data]
            .into_iter()
            .find_map(|operand| match operand {
                Operand::Value(value)
                    if ir.values[value.0 as usize].slot
                        == (Slot {
                            frame: Frame::ROOT,
                            var,
                        }) =>
                {
                    Some(value)
                }
                _ => None,
            })
    })
}

/// The clobber rule held at the call-effect kernel `opcode`: what the
/// variables `memory` hold is read again after it, and read so by the
/// next opline that reads each; what `owned` hold is not.
fn clobbers(ir: &TraceIr, opcode: Opcode, memory: &[u32], owned: &[u32]) -> Result<(), String> {
    let at = stepped(ir, opcode)?;
    let again = reloaded(ir, at);
    for var in memory {
        if !again.contains(var) {
            return Err(format!(
                "CV{var} is not read again after {opcode:?}:\n{}",
                dump::render(ir)
            ));
        }
        if let Some(read) = next_read(ir, at, *var) {
            assert_eq!(
                ir.values[read.0 as usize].def,
                Def::Reload(at),
                "CV{var}'s next read"
            );
        }
    }
    for var in owned {
        if again.contains(var) {
            return Err(format!(
                "CV{var}, the trace's own, is read again after {opcode:?}"
            ));
        }
    }
    Ok(())
}

/// A loop in `f`, where the plan records.
const IN_F: [(&str, &str); 2] = [("record.function", "f"), ("record.start", "loop")];

/// A native's call, `DO_ICALL`: the array a reference passed in is read
/// again after it and guarded again where it is read; the sum the trace
/// keeps in a register is not.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_natives_call_reloads_what_a_reference_holds() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; strcmp('a', 'b'); $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    // `$a` is CV0, `$n` CV1, `$s` CV2, `$i` CV3.
    clobbers(&ir, Opcode::DoIcall, &[0], &[1, 2, 3])?;
    let at = stepped(&ir, Opcode::DoIcall)?;
    let read = next_read(&ir, at, 0).ok_or("no read of $a after the call")?;
    assert!(
        ir.guards.iter().any(|guard| matches!(
            guard.check,
            Check::Deref { value, .. } if value == read
        )),
        "the reloaded reference is guarded again"
    );
    Ok(())
}

/// A call php resolves by name, `DO_FCALL_BY_NAME`, to a native.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_by_name_to_a_native_reloads_what_a_reference_holds() -> Result<(), String> {
    let ir = built(
        &[("record.function", "N\\f"), ("record.start", "loop")],
        "namespace N;
        function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; strcmp('a', 'b'); $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    clobbers(&ir, Opcode::DoFcallByName, &[0], &[1, 2, 3])
}

/// A frameless call, `FRAMELESS_ICALL_2`, in a file's own code, whose
/// every variable a symbol table reaches: each is read again.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_frameless_call_reloads_every_variable_of_a_files_code() -> Result<(), String> {
    let ir = built(
        &[("record.function", ""), ("record.start", "loop")],
        "$o = new \\stdClass; $s = 0;
        for ($i = 0; $i < 3; $i++) { $s += $i; property_exists($o, 'p'); $s += $i; }
        echo $s, \"\\n\";",
    )?;
    // `$o` is CV0, `$s` CV1, `$i` CV2.
    clobbers(&ir, Opcode::FramelessIcall2, &[0, 1, 2], &[])
}

/// A dynamic call to a native, `DO_FCALL`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_dynamic_call_to_a_native_reloads_what_a_reference_holds() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f(array &$a, int $n) {
            $s = 0; $cmp = 'strcmp';
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; $cmp('a', 'b'); $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    // `$a` is CV0, `$n` CV1, `$s` CV2, `$cmp` CV3, `$i` CV4.
    clobbers(&ir, Opcode::DoFcall, &[0], &[1, 2, 4])
}

/// A `TICKS` opline, which calls what `register_tick_function` registered:
/// user code.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn ticks_reload_what_a_reference_holds() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "declare(ticks=1);
        function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    clobbers(&ir, Opcode::Ticks, &[0], &[1, 2, 3])
}

/// The leave of an inlined callee whose variables own something, a
/// `RETURN` left in place that frees them and may serve a destructor, has
/// no `call` effect: the caller's variables, a reference among them, are
/// read again after it by nothing, as php's code keeps every type and
/// register across `zend_jit_free_cv` (`Builder::clobber`'s witnesses
/// hold a destructor there that writes them).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_leave_that_frees_its_variables_reloads_nothing() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "class D {}
        function h($i) { $d = new D; return $i; }
        function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; $s += h($i); $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let (at, leave) = ir
        .ops
        .iter()
        .enumerate()
        .find_map(|(at, op)| match op {
            Op::Line(line) if line.opcode == Opcode::Return => Some((at, line)),
            _ => None,
        })
        .ok_or_else(|| format!("no RETURN in\n{rendered}"))?;
    // `$d` is the callee's CV1.
    let d = super::Vars::default().with(1).ok_or("a set of one")?;
    assert_eq!(leave.form, Form::Inline, "{rendered}");
    assert!(
        matches!(leave.imm, super::Imm::Leave { owners: super::Owners::Each(vars), .. } if vars == d),
        "{rendered}"
    );
    assert!(leave.unwind.is_some(), "a destructor may throw: {rendered}");
    let at = OpId(u32::try_from(at).map_err(|_| "a long trace")?);
    assert_eq!(reloaded(&ir, at), Vec::<u32>::new(), "{rendered}");
    assert!(rendered.contains("frees CV1"), "{rendered}");
    Ok(())
}

/// A return trace that lands past a recursive call and never reads the
/// typed parameter knows it holds what its declaration says all the same:
/// the `VERIFY_RETURN_TYPE` of `return $n;` under an `int` return leaves
/// an `int` as it is, so it is no write of another type, as php's
/// inference types it, and the `RETURN` left in place frees nothing.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_typed_parameter_its_return_type_keeps_is_no_owner_at_the_leave() -> Result<(), String> {
    let ir = built(
        &[("record.function", "f"), ("record.start", "return")],
        "function f(int $n): int { if ($n < 2) { return $n; } $a = f($n - 1); return $a + 1; }
        echo f(20), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let leaves: Vec<_> = (ir.ops.iter())
        .filter_map(|op| match op {
            Op::Line(line) if line.opcode == Opcode::Return => Some(line.imm),
            _ => None,
        })
        .collect();
    assert!(!leaves.is_empty(), "{rendered}");
    for imm in leaves {
        assert!(
            matches!(
                imm,
                super::Imm::Leave {
                    owners: super::Owners::Uncounted,
                    ..
                }
            ),
            "{rendered}"
        );
    }
    Ok(())
}

/// A kernel that may throw and calls nothing, a cast to string, reads
/// nothing again: it may only unwind, which takes no exit, as php's
/// compiled code jumps to its stub for a throwable and takes no exit point.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_throwing_kernel_reloads_nothing_but_may_leave() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; $t = (string) $s; $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    let at = stepped(&ir, Opcode::Cast)?;
    assert_eq!(reloaded(&ir, at), Vec::<u32>::new());
    let Some(Op::Line(line)) = ir.ops.get(at.index()) else {
        return Err("no cast".to_owned());
    };
    let unwind = line.unwind.ok_or("a throwing kernel unwinds")?;
    assert!(usize::from(unwind.0) < ir.unwinds.len());
    assert_eq!(line.exit, None, "a step finishes or unwinds");
    Ok(())
}

/// A recursive return trace (`jit-design.md` §2.2, #3036): rooted at
/// `r`'s landing past its call to itself, it returns past its root into
/// a caller it escapes from unless it stands at that landing, goes on
/// there, and
/// goes back to its header through the next return, which escapes unless
/// the caller stands at the header's landing, as php's
/// `zend_jit_leave_func` holds the instruction pointer with no exit; a
/// phi stands for the slot the call before the landing writes.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_trace_guards_the_caller_it_lands_in() -> Result<(), String> {
    let ir = built(
        &[("record.function", "r"), ("record.start", "return")],
        "function r($n) { if ($n == 0) { return 0; } $x = r($n - 1); return $x + 1; }
        echo r(5), \"\\n\";",
    )?;
    assert_eq!(ir.entry.stop, Stop::RecursiveRet);
    let stands: Vec<_> = (ir.ops.iter())
        .filter_map(|op| match op {
            Op::Back {
                escapes: Some((_, opline)),
                ..
            } => Some(*opline),
            _ => None,
        })
        .collect();
    let Some(Op::Line(first)) = ir.ops.first() else {
        return Err("no first opline".to_owned());
    };
    assert_eq!(
        stands,
        [first.opline, first.opline],
        "each return held to the landing"
    );
    assert!(matches!(ir.end, End::Loop(_)), "{:?}", ir.end);
    Ok(())
}

/// A trace that reaches a compiled entry links to it: it leaves through
/// its last exit, in the frame the call pushed, standing where the frame
/// stands, naming the trace.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_link_leaves_through_the_exit_that_names_its_trace() -> Result<(), String> {
    let ir = built(
        &[
            ("record.function", "f"),
            ("record.start", "enter"),
            ("record.compiled", "g"),
        ],
        "function g($x) { return $x + 1; }
        function f() { $a = g(1); return $a * 2; }
        g(0); echo f(), \"\\n\";",
    )?;
    let End::Leave(exit) = ir.end else {
        return Err(format!("a link loops:\n{}", dump::render(&ir)));
    };
    let exit = &ir.exits[usize::from(exit.0)];
    assert!(
        matches!(ir.entry.stop, Stop::Link(_)),
        "{:?}",
        ir.entry.stop
    );
    assert_eq!((exit.opline, exit.link.is_some()), (None, true), "{exit:?}");
    assert_ne!(exit.frame, Frame::ROOT, "the callee's frame");
    Ok(())
}

/// A fetched element's guard leaves past the fetch with the element
/// stored, and frees the temporary array the fetch read, which compiled
/// code has not freed: its landing's `frees`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fetched_elements_guard_frees_the_temporary_it_read() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function g() { $a = []; $a[] = 1.5; return $a; }
        function f(int $n) {
            $s = 0.0;
            for ($i = 0; $i < $n; $i++) { $s += g()[0]; }
            return $s;
        }
        echo f(3), \"\\n\";",
    )?;
    assert!(
        ir.exits.iter().any(|exit| exit.landing.frees
            == Frees {
                op1: true,
                op2: false
            }),
        "{}",
        dump::render(&ir)
    );
    // Freeing the array may run a destructor once the fetch is done: an
    // unwind, which takes no exit, as php's exception stub takes none.
    let fetch = ir
        .ops
        .iter()
        .find_map(|op| match op {
            Op::Line(line) if line.opcode == Opcode::FetchDimR && line.form == Form::Inline => {
                Some(line)
            }
            _ => None,
        })
        .ok_or_else(|| format!("no inline fetch:\n{}", dump::render(&ir)))?;
    let freed = fetch
        .unwind
        .ok_or("the free of the fetched array leaves nowhere")?;
    assert!(usize::from(freed.0) < ir.unwinds.len());
    assert!(
        fetch.exit.is_some(),
        "a hole leaves through the fetch's exit"
    );
    Ok(())
}

/// A loop of 250 `$s += $i * k;` statements, which php compiles: its
/// places that leave with one spec share one exit, as php's
/// `zend_jit_trace_get_exit_point` reuses them, so it builds under php's
/// 512. Each statement leaves at two oplines, the `MUL`'s overflow and
/// the `ASSIGN_OP`'s, the last's too: the loop's phi of `$s` is guarded
/// long at entry, and the sum its back edge brings is held long within
/// the last `+=`, which reruns where it overflows, as php's
/// `is_checked_guard` moves the phi's guard (#3135), so no guard on `$s`
/// stands at the header. With the entry guards on `$s` and `$i`, the
/// `$i++`'s overflow, the loop's test and its interrupt test, which grows
/// no side trace and leaves through an exit of its own, php's
/// `ZEND_JIT_EXIT_TO_VM`, that is 505, where an exit per place was 754.
/// php's is 502: its inference over the whole function knows `$i` long
/// and its range, so it holds no entry guard on it and no overflow on
/// `$i++` or on `$i * 1`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_long_loop_shares_its_exits_as_php_reuses_them() -> Result<(), String> {
    let mut body = String::new();
    for k in 1..=250 {
        writeln!(body, "$s += $i * {k};").map_err(|e| e.to_string())?;
    }
    let ir = built(
        &IN_F,
        &format!(
            "function f(int $n) {{ $s = 0; for ($i = 0; $i < $n; $i++) {{\n{body}}} return $s; }}
            echo f(3), \"\\n\";"
        ),
    )?;
    assert_eq!(ir.exits.len(), 505, "{}", dump::render(&ir));
    let s = |value: ValueId| ir.values[value.0 as usize].slot.var == 1;
    let on_s: Vec<_> = (ir.guards.iter())
        .filter_map(|guard| match guard.check {
            Check::Type { value, tag } if s(value) => Some((guard.at, tag)),
            _ => None,
        })
        .collect();
    let last = ir
        .ops
        .iter()
        .rposition(|op| matches!(op, Op::Line(line) if line.opcode == Opcode::AssignOp));
    let last = OpId(u32::try_from(last.ok_or("no +=")?).map_err(|e| e.to_string())?);
    assert!(
        on_s.contains(&(GuardAt::Entry, ZvalType::Long))
            && on_s.contains(&(GuardAt::Within(last), ZvalType::Long))
            && !on_s.iter().any(|(at, _)| matches!(at, GuardAt::Before(_))),
        "$s held at entry and within the last +=, and not at the header: {on_s:?}"
    );
    for (at, exit) in ir.exits.iter().enumerate() {
        assert!(
            !ir.exits[..at].contains(exit),
            "exit {at} is an earlier one's"
        );
    }
    Ok(())
}

/// A boolean the trace keeps in a register holds what its op computed:
/// a guard on the type the recording saw, `true` here, is tested on it
/// and narrows only what the opline after it sees. The boolean a
/// comparison writes, read by a `JMPZ_EX` in the first loop and
/// assigned to `$b` in the second, stays a boolean in its register
/// beside its guard:
/// narrowed to the one seen, the register would hold a constant, its
/// guard would test nothing and the trace would go the recorded way past
/// the iteration where the comparison turns (#3076).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_guarded_boolean_keeps_what_its_op_computed() -> Result<(), String> {
    for program in [
        "function f(int $n) { $h = 0; for ($i = 0; $i < $n; $i++) { if ($i < 100 && $i >= 0) { $h++; } } return $h; }
        echo f(3), \"\\n\";",
        "function f(int $n) { $h = 0; $b = true; for ($i = 0; $i < $n; $i++) { if ($b) { $h++; } $b = $i < 99; } return $h; }
        echo f(3), \"\\n\";",
    ] {
        let ir = built(&IN_F, program)?;
        let compared = ir
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Line(line) if line.opcode == Opcode::IsSmaller && line.fused.is_none() => {
                    line.out
                }
                _ => None,
            })
            .ok_or_else(|| format!("no comparison writes a boolean:\n{}", dump::render(&ir)))?;
        let held = ir.values[compared.0 as usize];
        assert_eq!(
            (held.home, held.types),
            (Home::Register, Types::BOOL),
            "{}",
            dump::render(&ir)
        );
        assert!(
            ir.guards.iter().any(|guard| guard.check
                == Check::Type {
                    value: compared,
                    tag: ZvalType::True
                }),
            "{}",
            dump::render(&ir)
        );
    }
    Ok(())
}

/// The trace of `program`'s loop in `f`, rendered, where any of its
/// oplines is stepped; none where every one is inline.
fn stepped_trace(program: &str) -> Result<Option<String>, String> {
    let ir = built(&IN_F, program)?;
    let stepped = ir
        .ops
        .iter()
        .any(|op| matches!(op, Op::Line(line) if line.form == Form::Step));
    Ok(stepped.then(|| dump::render(&ir)))
}

/// A product of one variable by itself: the guard on its second operand
/// narrows the value both read, so the opline sees both as the guard
/// holds them and is inline, where it saw its first as the loop's phi
/// stood before the guard and was stepped on every turn (#3073).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_operand_read_twice_is_seen_as_its_one_guard_holds_it() -> Result<(), String> {
    for body in ["$s = $i * $i;", "$s = ($s + $i * $i) % 1000003;"] {
        let stepped = stepped_trace(&format!(
            "function f($n) {{ $s = 0; for ($i = 1; $i < $n; $i++) {{ {body} }} return $s; }}
            echo f(100), \"\\n\";"
        ))?;
        assert_eq!(stepped, None, "{body}");
    }
    Ok(())
}

/// A division at a loop's header, whose operands php's guard switch
/// leaves out: guarded as a product's here, it sees the loop's variable `$i`
/// as the guard holds it and is inline, where it saw the loop's phi as
/// anything and was stepped on every turn (#3073, #3111).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_division_at_a_loops_header_is_inline() -> Result<(), String> {
    for (start, body) in [
        ("0.0", "$s = 3.0 / $i;"),
        ("0.0", "$s = $i / 3.0;"),
        ("1.0", "$s = $s / 1.0000001;"),
        ("0", "$s = $i / 1;"),
        ("1000.0", "$s /= $i;"),
    ] {
        let stepped = stepped_trace(&format!(
            "function f($n) {{ $s = {start}; for ($i = 1; $i < $n; $i++) {{ {body} }} return $s; }}
            echo f(100), \"\\n\";"
        ))?;
        assert_eq!(stepped, None, "{body}");
    }
    Ok(())
}

/// A loop in the file's own code, where the plan records.
const IN_MAIN: [(&str, &str); 3] = [
    ("record.function", ""),
    ("record.start", "loop"),
    ("record.headers", "0"),
];

/// `statement` written `n` times in the body of a loop of the file's own
/// code, every variable a symbol table reaches.
fn repeated(statement: &str, n: usize) -> String {
    format!(
        "function f($a) {{ return $a; }}
        $a = [1, 2, 3]; $b = true; $y = 1; $s = 0; $x = 0;
        for ($i = 0; $i < 100; $i++) {{ {} }}
        echo \"ok\\n\";",
        statement.repeat(n)
    )
}

/// The exits one more `statement` takes in a loop of the file's own
/// code, as php's `zend_jit_trace_get_exit_point` takes them: the pinned
/// php's `TRACE 1 exit info` (`opcache.jit_debug` bit 20) for the loop at
/// two and three statements under the recipe's hot counters (#3113). A
/// variable a symbol table reaches is guarded at every opline php's code
/// generator holds its recorded type at, a cast to string of an int and
/// a rope part or an element of an array written out take none of their own, a
/// throw none at all, and an exit at an opline is new where the types the
/// trace knows of the frame's slots differ from an earlier one's. A
/// jump on a condition its guard holds as `true` or `false` goes one way
/// and takes no exit of its own, php's unconditional branch: `if (!$b)`
/// takes its `BOOL_NOT`'s guard on `$b` and its `JMPZ`'s on what the
/// `BOOL_NOT` wrote alone, and `$b || $i` its `JMPNZ_EX`'s guard on `$b`
/// and none for its direction, the pinned php's two and three under
/// `opcache.optimization_level=0`, so that its op arrays are this
/// engine's (#3203).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn each_statement_takes_the_exits_php_takes() -> Result<(), String> {
    for (statement, php) in [
        ("$s = (string)$i;", 1),
        ("$x = \"a{$i}b\";", 2),
        ("$x = [$i, $i, $i, $i];", 1),
        ("$x = $i . \"K\" . $i . \"z\";", 3),
        ("$x = $a[$i % 3];", 7),
        ("$x = $i + 0.5;", 3),
        ("$x = $i == 5;", 3),
        ("$x = $i ^ 5;", 3),
        ("$x = 5;", 1),
        ("$s += $i * 5;", 2),
        ("$x = $b && $i;", 4),
        ("if (!$b) { $x = 1; }", 2),
        ("$x = $b || $i;", 3),
        ("$x = $y;", 1),
    ] {
        let two = built(&IN_MAIN, &repeated(statement, 2))?;
        let three = built(&IN_MAIN, &repeated(statement, 3))?;
        assert_eq!(
            three.exits.len() - two.exits.len(),
            php,
            "{statement}\n{}",
            dump::render(&three)
        );
    }
    Ok(())
}

/// A jump whose condition its guard holds as one kind goes one way, so
/// `if (!$b)` three times in a loop of the file's own code leaves at the
/// oplines php's `TRACE 1 exit info` lists, exit by exit, under
/// `opcache.optimization_level=0`: the loop's test and its direction at
/// `0017` and `0019`, then each statement's `BOOL_NOT` and `JMPZ` guards
/// (`0007`, `0008`, `0010`, `0011`, `0013`, `0014`), the increment's at
/// `0016`, with no exit for a `JMPZ`'s direction
/// (`Z/ext/opcache/jit/zend_jit_trace.c:5687-5691`, #3203), and tenth
/// its interrupt test's own at `0017`, php's `ZEND_JIT_EXIT_TO_VM`: no
/// code of the trace's reads the instruction pointer it entered with,
/// since php compiles every one of its oplines inline, the `BOOL_NOT`s by
/// `zend_jit_bool_jmpznz` (`:5644-5654`) where this tier steps them, so
/// `zend_jit_trace_uses_initial_ip` says no and the test takes an exit
/// (`:7210-7228`, #3277).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_jump_its_guard_holds_one_way_takes_phps_exits() -> Result<(), String> {
    let ir = built(&IN_MAIN, &repeated("if (!$b) { $x = 1; }", 3))?;
    let oplines: Vec<Option<u32>> = (ir.exits.iter())
        .map(|exit| exit.opline.map(|at| at.0))
        .collect();
    let php = [17, 19, 7, 8, 10, 11, 13, 14, 16, 17].map(Some);
    assert_eq!(oplines, php, "{}", dump::render(&ir));
    Ok(())
}

/// php's 512 exits bound `$x = 5;` at the count php's exits reach: one
/// exit a statement, the loop's entry guard, its test's direction, the
/// increment's guard and the interrupt test, which grows no side trace
/// and leaves through an exit of its own, php's `ZEND_JIT_EXIT_TO_VM`, so
/// 508 statements build and 509 are refused with `too many side exits`,
/// as the pinned php refuses them (#3113).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn the_exit_bound_falls_where_phps_does() -> Result<(), String> {
    let ir = built(&IN_MAIN, &repeated("$x = 5;", 508))?;
    assert_eq!(ir.exits.len(), 512);
    let refused = built(&IN_MAIN, &repeated("$x = 5;", 509));
    assert!(
        refused
            .as_ref()
            .is_err_and(|why| why.contains("TooManyExits")),
        "{:?}",
        refused.map(|ir| ir.exits.len())
    );
    Ok(())
}

/// A jump on a double is stepped, so a NAN is coerced to its truth by the
/// handler, which warns of it each time, as php's compiled code does
/// (`ext/opcache/tests/jit/nan_001.phpt`); inline, the compiled test read
/// the NAN as true and warned of nothing.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_jump_on_a_double_warns_of_a_nan_each_time() {
    let served = crate::testing::serve::<crate::testing::Lane>(
        &[("jit.mode", "force"), ("html_errors", "0")],
        "$b = 1e308 * 10; $b = $b - $b;
        for ($i = 0; $i < 3; $i++) {
            if ($b) { echo \"nan is true\\n\"; } else { echo \"nan is false\\n\"; }
        }",
    );
    let stdout = crate::testing::stdout(&served);
    assert_eq!(
        stdout
            .matches("Warning: unexpected NAN value was coerced to bool")
            .count(),
        3,
        "{stdout}"
    );
    assert_eq!(stdout.matches("nan is true").count(), 3, "{stdout}");
}

/// The guards a loop holds at entry stand in their slots' order, as
/// php's code holds the types the trace entered with slot by slot, so
/// each exit knows the types of the slots before its own: `$n`, `$s`
/// and `$i`, as the pinned php's `TRACE 1 exit info` lists `exit_0`,
/// `exit_1` knowing `$n` and `exit_2` knowing `$n` and `$s`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn the_entry_guards_stand_in_their_slots_order() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        echo f(5), \"\\n\";",
    )?;
    let entered: Vec<(u32, usize)> = (ir.guards.iter())
        .filter(|guard| guard.at == GuardAt::Entry)
        .filter_map(|guard| match guard.check {
            Check::Type { value, .. } => {
                let exit = &ir.exits[usize::from(guard.exit.0)];
                Some((ir.values[value.0 as usize].slot.var, exit.known.len()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(entered, [(0, 0), (1, 1), (2, 2)], "{}", dump::render(&ir));
    Ok(())
}

/// A `for` loop's trace, rooted at its condition, keeps a second way in
/// at the opline its back edge lands on, the body's first, `+=` at 4
/// past the condition's `IS_SMALLER` at 6: the op after the condition's,
/// with every variable it holds there, `$n`, `$s` and `$i`, so the
/// interpreter's back edge enters the trace there and steps nothing on
/// to the condition. A loop whose condition makes a call keeps none: the
/// call's frame stands between.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_loops_trace_keeps_a_way_in_at_the_body_its_back_edge_lands_on() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        echo f(5), \"\\n\";",
    )?;
    let body = ir.body.as_ref().ok_or("no way in at the body")?;
    let at = ir.ops.get(body.at.index());
    assert!(
        matches!(at, Some(Op::Line(line)) if line.opline.0 == 4 && line.opcode == Opcode::AssignOp),
        "{at:?}\n{}",
        dump::render(&ir)
    );
    assert_eq!(body.opline.0, 4);
    let mut held: Vec<u32> = body.holds.iter().map(|(slot, _)| slot.var).collect();
    held.sort_unstable();
    held.dedup();
    assert_eq!(held, [0, 1, 2], "{:?}", body.holds);
    let called = built(
        &IN_F,
        "function g($i) { return $i; }
        function f($n) { $s = 0; for ($i = 0; g($i) < $n; $i++) { $s += $i; } return $s; }
        echo f(5), \"\\n\";",
    )?;
    assert_eq!(called.body, None, "{}", dump::render(&called));
    Ok(())
}

/// A `while (true)`'s trace is rooted at its body, where its back edge
/// lands, as php's CFG heads the loop once its optimizer folds the
/// constant condition into a `JMP` (`record::loop_root`, #3317): it keeps
/// no second way in, its back edge landing on its root, and the `JMPNZ`
/// on `true` that ends its body goes one way and takes no exit, as php's
/// `JMP` takes none (#3203): no exit goes on past it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_loop_on_a_constant_condition_keeps_no_way_in_at_its_body() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f($n) { $s = 0; while (true) { $s += $n; if ($s > 20) { break; } } return $s; }
        echo f(5), \"\\n\";",
    )?;
    assert_eq!(ir.body, None, "{}", dump::render(&ir));
    let jump = (ir.ops.iter())
        .find_map(|op| match op {
            Op::Line(line) if line.opcode == Opcode::Jmpnz => Some(line.opline),
            _ => None,
        })
        .ok_or_else(|| format!("no JMPNZ in\n{}", dump::render(&ir)))?;
    let past = OplineIdx(jump.0 + 1);
    assert!(
        (ir.exits.iter()).all(|exit| exit.opline != Some(past)),
        "{}",
        dump::render(&ir)
    );
    Ok(())
}

/// A loop's interrupt test takes php's exit point only where php's code
/// does not keep the trace's initial instruction pointer or keeps values
/// in registers at the loop's end, and php's interrupt stub otherwise,
/// which takes none (`Z/ext/opcache/jit/zend_jit_trace.c:7210-7228`): in
/// a loop of the file's own code, whose variables a symbol table reaches
/// and no register keeps, one whose oplines php serves through their
/// handlers lists no `/VM` exit, and one served inline does. Each builds
/// php's exits in total, as the pinned php's `TRACE 1 exit info` for the
/// same program lists them, once and twice over (#3137).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn each_loop_builds_phps_exits_and_its_interrupt_test_takes_one_where_phps_does()
-> Result<(), String> {
    for (statement, php, to_vm) in [
        ("$s = (string)$i;", [4, 5], false),
        ("$x = \"a{$i}b\";", [5, 7], false),
        ("$x = [$i, $i, $i, $i];", [4, 5], false),
        ("$x = $i . \"K\" . $i . \"z\";", [6, 9], false),
        ("$x = $i + 0.5;", [6, 9], false),
        ("$s += $i * 5;", [5, 7], false),
        ("$x = 5;", [5, 6], true),
        ("$x = $y;", [5, 6], true),
        ("$x = $i == 5;", [7, 10], true),
    ] {
        for (n, php) in [1, 2].into_iter().zip(php) {
            let ir = built(&IN_MAIN, &repeated(statement, n))?;
            let vm = ir
                .exits
                .iter()
                .any(|exit| exit.grows == super::Grows::Never);
            assert_eq!(
                (ir.exits.len(), vm),
                (php, to_vm),
                "{statement} {n} times\n{}",
                dump::render(&ir)
            );
        }
    }
    Ok(())
}

/// A side trace whose parent's exit named no opline, `Landed::Stood`,
/// holds before any other guard that the frame stands at the opline it
/// was recorded from, php's IP guard (`zend_jit_trace_opline_guard`,
/// `Z/ext/opcache/jit/zend_jit_trace.c:4178-4180`): exit 0, which names
/// no opline, knows nothing of the frame, as php takes it before the
/// side's stack holds a type, and grows a side of its own. One whose
/// exit named its opline holds none.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_side_of_an_exit_that_names_no_opline_holds_where_the_frame_stands() -> Result<(), String> {
    let (mut recorded, _) = recording(
        &IN_F,
        "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        echo f(5), \"\\n\";",
    )?;
    let stands = |ir: &TraceIr| -> Vec<(Check, u16)> {
        (ir.guards.iter())
            .filter(|guard| matches!(guard.check, Check::Stands { .. }))
            .map(|guard| (guard.check, guard.exit.0))
            .collect()
    };
    let named = build(&recorded.record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    assert_eq!(stands(&named), [], "{}", dump::render(&named));
    recorded.record.landed = Landed::Stood;
    let ir = build(&recorded.record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    let Some(Op::Line(first)) = ir.ops.first() else {
        return Err("no first opline".to_owned());
    };
    let func = ir.frames[usize::from(first.frame.0)].func;
    let check = Check::Stands {
        func,
        opline: first.opline,
    };
    assert_eq!(stands(&ir), [(check, 0)], "{}", dump::render(&ir));
    let held = ir.guards.iter().find(|guard| guard.exit.0 == 0);
    assert_eq!(held.map(|guard| guard.at), Some(GuardAt::Entry));
    let exit = &ir.exits[0];
    assert_eq!(
        (exit.opline, exit.known.len(), exit.stores.len(), exit.grows),
        (None, 0, 0, Grows::Side),
        "{exit:?}"
    );
    assert_eq!(ir.exits.len(), named.exits.len() + 1, "one exit more");
    Ok(())
}

/// A trace that stops at a function's `RETURN` compiles the `RETURN`,
/// leaving the frame in place and going on in the caller, as php's code
/// goes on through `zend_jit_trace_return`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:6532-6556`): its last op is the
/// `RETURN` left in place, its end the exit the interpreter serves the
/// `RETURN` through where the leave cannot be made in place
/// ([`End::Return`]). A variable the trace does not hold uncounted, here
/// a string, is named to be freed as the frame leaves, as php's
/// `zend_jit_free_cv` frees it, and no other.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_trace_that_stops_at_a_return_leaves_the_frame_in_place() -> Result<(), String> {
    let ir = built(
        &[("record.function", "f"), ("record.start", "enter")],
        "function f($i) { $d = \"s$i\"; return $i; }
        $s = 0; for ($i = 0; $i < 100; $i++) { $s += f($i); } echo $s, \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    assert_eq!(ir.entry.stop, Stop::Return, "{rendered}");
    let End::Return(exit) = ir.end else {
        return Err(format!("no return in place: {rendered}"));
    };
    let Some(Op::Line(last)) = ir.ops.last() else {
        return Err(format!("no last line: {rendered}"));
    };
    assert_eq!(last.opcode, Opcode::Return, "{rendered}");
    // `$d`, the second variable, may own a string; `$i` owns nothing.
    let d = super::Vars::default().with(1).ok_or("a set of one")?;
    assert_eq!(
        last.imm,
        super::Imm::Leave {
            owners: super::Owners::Each(d),
            value: super::Returned::Scalar,
            receives: false,
        },
        "{rendered}"
    );
    assert_eq!(
        ir.exits
            .get(usize::from(exit.0))
            .and_then(|exit| exit.opline),
        Some(last.opline),
        "{rendered}"
    );
    Ok(())
}

/// A loop whose first opline php compiles inline but whose code may call
/// into php's runtime keeps php's initial instruction pointer, the code
/// standing the frame at that opline first (`jit_SET_EX_OPLINE`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:1100-1109`): `!$done` over a variable
/// recorded through a reference calls `zend_is_true` (`:7790-7792`), so
/// the pinned php's `TRACE 1 exit info` lists five exits and no `/VM` one,
/// its interrupt test taking the stub; with no reference, seven, the last
/// the interrupt test's `0006/VM` (under `opcache.optimization_level=0`,
/// #3277).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_loop_keeps_the_initial_pointer_where_its_first_opline_may_call_the_runtime()
-> Result<(), String> {
    for (program, php, stub) in [
        (
            "$done = false; $r = &$done; $n = 0;
            while (!$done) { $n++; $done = $n > 50; } echo $n, \"\\n\";",
            5,
            true,
        ),
        (
            "$done = false; $n = 0;
            while (!$done) { $n++; $done = $n > 50; } echo $n, \"\\n\";",
            7,
            false,
        ),
    ] {
        let ir = built(&IN_MAIN, program)?;
        let End::Loop(header) = ir.end else {
            return Err(format!("no loop: {}", dump::render(&ir)));
        };
        assert_eq!(
            (
                ir.exits.len(),
                matches!(header.interrupt, Interrupt::Stub(_))
            ),
            (php, stub),
            "{program}\n{}",
            dump::render(&ir)
        );
    }
    Ok(())
}

/// A `DO_*CALL` the trace follows into a callee its `INIT_*` pushed
/// through the `push_call` kernel takes no exit, as php's code takes none
/// for the call (#3203's measurement): what the call's kernel throws
/// unwinds. A loop calling `g`, declared after it, by name takes the
/// pinned php's first six exits (`TRACE 1 exit info` under
/// `opcache.optimization_level=0`): the stack check, three guards at
/// entry, the direction at `0011`, and the find-function guard of the
/// `INIT_FCALL_BY_NAME` at `0004`; the call at `0006` none.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_the_trace_follows_takes_no_exit() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += g($i); } return $s; }
        function g($a) { return $a + 1; }
        echo f(50), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let oplines: Vec<Option<u32>> = (ir.exits.iter().take(6))
        .map(|exit| exit.opline.map(|at| at.0))
        .collect();
    assert_eq!(oplines, [9, 9, 9, 9, 11, 4].map(Some), "{rendered}");
    let call = ir.ops.iter().find_map(|op| match op {
        Op::Line(line) if line.opcode == Opcode::DoFcallByName => Some(line),
        _ => None,
    });
    assert!(
        call.is_some_and(|line| line.exit.is_none() && line.unwind.is_some()),
        "{rendered}"
    );
    Ok(())
}

/// An entered trace of a function with typed parameters numbers php's
/// `RECV` exits ahead of its own (`jit-design.md` §4.2, #3269): one per
/// `RECV` that has a type or that no `RECV` follows, as php's
/// `zend_jit_recv` takes them for a root frame whose argument count the
/// trace does not know (`Z/ext/opcache/jit/zend_jit_ir.c:10825-10872`),
/// none for a `RECV_INIT`, and none at all where no parameter has a
/// type. Each lands at the entry's opline and grows nothing, and the
/// recursion's interrupt test leaves through the first where it is the
/// `RECV` at opline zero, php's reuse there; where the first `RECV` takes
/// none, the test numbers its own, last, as php's does. The direction
/// exit after them carries php's number: the pinned php's `TRACE 1 exit
/// info` under the trace-dump regime lists `exit_0` at `0000` and
/// `exit_1` at `0003` then the direction's `exit_2` at `0007` for the
/// first; `exit_0` at `0001`, the direction's `exit_1` and the test's own
/// `exit_4` at `0000` for the second; `exit_0` and `exit_1` at `0000` and
/// `0001`, then `exit_2` for the third; and for the fourth no `RECV`
/// exit, its guard's `exit_0` at `0002` then the direction's `exit_1`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_entered_trace_numbers_phps_recv_exits_ahead_of_its_own() -> Result<(), String> {
    for (program, recvs, direction, reuses) in [
        (
            "function f(int $a, $b, $c, int $d, $e = 1) {
                if ($a < 1) { return 0; } return f($a - 1, 0, 0, $d);
            }
            echo f(20, 1, 2, 3), \"\\n\";",
            2,
            2,
            true,
        ),
        (
            "function f($b, int $a) { if ($a < 1) { return 0; } return f(0, $a - 1); }
            echo f(1, 20), \"\\n\";",
            1,
            1,
            false,
        ),
        (
            "function f(int $a, $b) { if ($a < 1) { return 0; } return f($a - 1, 0); }
            echo f(20, 1), \"\\n\";",
            2,
            2,
            true,
        ),
        (
            "function f($a, $b) { if ($a < 1) { return 0; } return f($a - 1, 0); }
            echo f(20, 1), \"\\n\";",
            0,
            1,
            false,
        ),
    ] {
        let ir = built(
            &[("record.function", "f"), ("record.start", "enter")],
            program,
        )?;
        let rendered = dump::render(&ir);
        let Some(Op::Line(first)) = ir.ops.first() else {
            return Err(format!("no first opline: {rendered}"));
        };
        let reserved = (ir.exits.iter())
            .take_while(|exit| {
                (exit.frame, exit.opline, exit.grows, exit.stores.len())
                    == (Frame::ROOT, Some(first.opline), Grows::Never, 0)
            })
            .count();
        let directions: Vec<ExitId> = (ir.guards.iter())
            .filter(|guard| matches!(guard.check, Check::Direction { .. }))
            .map(|guard| guard.exit)
            .collect();
        let End::Loop(header) = ir.end else {
            return Err(format!("no loop: {rendered}"));
        };
        let interrupt = match header.interrupt {
            Interrupt::Exit(exit) => exit,
            Interrupt::Stub(_) => return Err(format!("a stub: {rendered}")),
            Interrupt::None => return Err(format!("no test: {rendered}")),
        };
        // The test's own exit, where it takes one, is numbered last.
        let last = u16::try_from(ir.exits.len().saturating_sub(1)).map_err(|e| e.to_string())?;
        let interrupt_php = if reuses { ExitId(0) } else { ExitId(last) };
        assert_eq!(
            (reserved, directions.first().map(|exit| exit.0), interrupt),
            (recvs, Some(direction), interrupt_php),
            "{program}\n{rendered}"
        );
        assert!(
            (ir.guards.iter()).all(|guard| usize::from(guard.exit.0) >= recvs),
            "no guard tests a RECV exit: {rendered}"
        );
    }
    Ok(())
}

/// `binary_trees`' `make_tree`, entered, leaves its second inlined level
/// through the first level's exits where the levels leave alike: the
/// caller's `$d`, held in a register, is stored to its slot as the call
/// enters (`Op::Enter`), so an exit from the callee stores none of it, as
/// php's stack map, relative to the frame an exit lands in, holds none of
/// a caller's slots. So the trace takes the pinned php's five exits,
/// `TRACE 1 exit info` under the trace-dump regime: the `RECV`'s
/// `exit_0`, the direction's `exit_1` for both levels, the subtraction's
/// overflow, and a stack check per level, `exit_3` and `exit_4`, which
/// each store their own level's `$d` (#3203, #3290).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_callees_exit_stores_none_of_its_callers_registers() -> Result<(), String> {
    let ir = built(
        &[("record.function", "make_tree"), ("record.start", "enter")],
        "function make_tree(int $depth)
        {
            if ($depth === 0) {
                return [null, null];
            }
            $d = $depth - 1;
            return [make_tree($d), make_tree($d)];
        }
        $t = make_tree(8); echo \"ok\", \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let directions: Vec<(Frame, ExitId)> = (ir.guards.iter())
        .filter(|guard| matches!(guard.check, Check::Direction { .. }))
        .filter_map(|guard| match guard.at {
            GuardAt::Within(at) => match ir.ops.get(at.index()) {
                Some(Op::Line(line)) => Some((line.frame, guard.exit)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        (ir.exits.len(), directions.as_slice()),
        (5, [(Frame(0), ExitId(1)), (Frame(1), ExitId(1))].as_slice()),
        "{rendered}"
    );
    assert!(
        (ir.exits.iter()).all(|exit| exit
            .stores
            .iter()
            .all(|store| store.slot.frame == exit.frame)),
        "{rendered}"
    );
    Ok(())
}

/// The forms the property oplines of `ir` are compiled in, in order.
fn property_forms(ir: &TraceIr) -> Vec<(Opcode, Form)> {
    (ir.ops.iter())
        .filter_map(|op| match op {
            Op::Line(line) if crate::types::on_declared_slot(line.opcode) => {
                Some((line.opcode, line.form))
            }
            _ => None,
        })
        .collect()
}

/// How many class guards `ir` holds.
fn class_guards(ir: &TraceIr) -> usize {
    (ir.guards.iter())
        .filter(|guard| matches!(guard.check, Check::Class { .. }))
        .count()
}

/// A loop over `$this` and a loop over an object a function holds: each
/// property opline of a published class's declared slot with no type is
/// compiled in place, a read, a write, a compound write, an increment or
/// a decrement and a fetch for writing, under one class guard per object
/// before the first,
/// as php's code generator compiles one whose `prop_info` it knows on an
/// immutable class (`zend_jit_class_guard`, `TRACE_FRAME_SET_THIS_CLASS_CHECKED`);
/// a typed property's write of a long its type holds is in place, as the
/// handler's own inline path writes it, and its write of a string its
/// type must coerce is the handler's (#3337).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn properties_of_a_published_class_are_inline_under_one_class_guard() -> Result<(), String> {
    let this = golden(OBJECT_THIS)?;
    assert_eq!(
        property_forms(&this),
        [
            (Opcode::FetchObjR, Form::Inline),
            (Opcode::AssignObj, Form::Inline),
            (Opcode::AssignObjOp, Form::Inline),
            (Opcode::PreIncObj, Form::Inline),
            (Opcode::FetchObjW, Form::Inline),
        ],
        "{}",
        dump::render(&this)
    );
    assert_eq!(class_guards(&this), 1, "{}", dump::render(&this));
    let value = golden(OBJECT_VALUE)?;
    assert_eq!(
        property_forms(&value),
        [
            (Opcode::FetchObjR, Form::Inline),
            (Opcode::AssignObj, Form::Inline),
            (Opcode::AssignObjOp, Form::Inline),
            (Opcode::PreDecObj, Form::Inline),
            (Opcode::AssignObj, Form::Inline),
            (Opcode::AssignObj, Form::Step),
            (Opcode::PostDecObj, Form::Step),
        ],
        "{}",
        dump::render(&value)
    );
    assert_eq!(class_guards(&value), 1, "{}", dump::render(&value));
    Ok(())
}

/// A class of the cached file linked over a class the request declared,
/// `zend_lazy_class_load`'s copy, is the request's own, with an id of
/// its own (#3322): no class guard assumes it is a published one, and
/// each of its property oplines is compiled in place on the place the
/// request's own cache slot holds, which it reads where it runs, as php's
/// code reads `CACHED_PTR(cache_slot)` for a class that is not immutable
/// (`Z/ext/opcache/jit/zend_jit_ir.c:14341-14370`, #3346).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_request_scoped_copy_of_a_class_is_guarded_through_its_cache_slot() -> Result<(), String> {
    let ir = golden(OBJECT_SCOPED)?;
    let rendered = dump::render(&ir);
    let lines: Vec<&Line> = (ir.ops.iter())
        .filter_map(|op| match op {
            Op::Line(line) if on_declared_slot(line.opcode) => Some(line),
            _ => None,
        })
        .collect();
    assert_eq!(
        lines
            .iter()
            .map(|line| (line.opcode, line.form))
            .collect::<Vec<_>>(),
        [
            (Opcode::FetchObjR, Form::Inline),
            (Opcode::AssignObj, Form::Inline),
            (Opcode::AssignObjOp, Form::Inline),
            (Opcode::PreIncObj, Form::Inline),
            (Opcode::AssignObj, Form::Inline),
        ],
        "{rendered}"
    );
    for line in &lines {
        assert!(
            matches!(
                line.imm,
                Imm::Property {
                    slot: Slotted::Cached,
                    ..
                }
            ),
            "{rendered}"
        );
        let map = line.map_ptr.ok_or_else(|| rendered.clone())?;
        assert!(map < ir.map_ptrs.len(), "{rendered}");
    }
    assert_eq!(class_guards(&ir), 0, "{rendered}");
    Ok(())
}

/// A variable written over between two property reads holds an object
/// no guard has held, whatever class the recording saw it of: the class a
/// guard held is carried only across a property opline that writes
/// through the object, as php's `COPY_SSA_OBJ_TYPE` carries it, so the
/// second read holds its class again (#3337).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_object_written_over_is_guarded_again() -> Result<(), String> {
    let ir = built(
        &[
            ("record.function", "f"),
            ("record.start", "loop"),
            ("record.headers", "0"),
        ],
        "class A { public $x = 1; }
        class B { public $q = 0; public $x = 2; }
        function f($os, $n) { $t = 0; $o = $os[0]; for ($i = 0; $i < $n; $i++) { $t += $o->x; $o = $os[$i]; $t += $o->x * 10; } return $t; }
        $a = new A; $os = []; for ($j = 0; $j < 30; $j++) { $os[] = $a; } $os[20] = new B;
        echo f($os, 30), \"\\n\";",
    )?;
    assert_eq!(class_guards(&ir), 2, "{}", dump::render(&ir));
    Ok(())
}

/// The fixtures of the hash slice and the dimension oplines each holds
/// inline (#3339).
const HASH_DIMS: [(Fixture, &[Opcode]); 13] = [
    (HASH_READ, &[Opcode::FetchDimR]),
    (HASH_IS, &[Opcode::FetchDimIs]),
    (HASH_ISSET, &[Opcode::IssetIsemptyDimObj]),
    (HASH_WRITE, &[Opcode::AssignDim]),
    (HASH_FETCH_W, &[Opcode::FetchDimW, Opcode::FetchDimRw]),
    (DIM_DIV, &[Opcode::AssignDimOp]),
    (DIM_MOD, &[Opcode::AssignDimOp]),
    (DIM_SL, &[Opcode::AssignDimOp]),
    (DIM_SR, &[Opcode::AssignDimOp]),
    (DIM_AND, &[Opcode::AssignDimOp]),
    (DIM_OR, &[Opcode::AssignDimOp]),
    (DIM_XOR, &[Opcode::AssignDimOp]),
    (DIM_CONCAT, &[Opcode::AssignDimOp]),
];

/// A hash's elements read, tested, written and fetched for writing by a
/// constant string, a variable's string and a long, and compounded with
/// `/`, `%`, the shifts, the bitwise operators and `.`, are inline, as
/// php's code generator compiles each over an array its trace holds a
/// hash (`zend_jit_fetch_dim_read`, `zend_jit_isset_isempty_dim`,
/// `zend_jit_assign_dim`, `zend_jit_fetch_dim`, `zend_jit_assign_dim_op`).
/// A read and a test are held to the layout php's packed guard holds; a
/// write, which tests the layout, takes no exit, its handler serving
/// what it does not write in place.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_hashs_dimension_oplines_are_inline() -> Result<(), String> {
    for (fixture, opcodes) in HASH_DIMS {
        let ir = golden(fixture)?;
        let rendered = dump::render(&ir);
        assert_eq!(ir.entry.stop, Stop::Loop, "{}: {rendered}", fixture.name);
        for opcode in opcodes {
            let lines: Vec<&Line> = (ir.ops.iter())
                .filter_map(|op| match op {
                    Op::Line(line) if line.opcode == *opcode => Some(line),
                    _ => None,
                })
                .collect();
            assert!(
                !lines.is_empty(),
                "{}: no {opcode:?} in {rendered}",
                fixture.name
            );
            for line in lines {
                assert_eq!(line.form, Form::Inline, "{}: {rendered}", fixture.name);
                if *opcode != Opcode::FetchDimR {
                    assert!(line.exit.is_none(), "{}: {rendered}", fixture.name);
                }
            }
        }
    }
    Ok(())
}

/// The first of `opcode`'s inline lines in `ir`, by its op.
fn inline_at(ir: &TraceIr, opcode: Opcode) -> Result<OpId, String> {
    ir.ops
        .iter()
        .position(
            |op| matches!(op, Op::Line(line) if line.opcode == opcode && line.form == Form::Inline),
        )
        .and_then(|at| u32::try_from(at).ok())
        .map(OpId)
        .ok_or_else(|| format!("no inline {opcode:?} in\n{}", dump::render(ir)))
}

/// A compound write to a key the array has not got is its handler's,
/// whose warning reaches an error handler that frees the array, writing
/// over the variable the reference holds: `ASSIGN_DIM_OP` has no `call`
/// effect, so the trace reads nothing again after it, and the reference's
/// inner type is held at its next read, as revision 83's analysis has it
/// (`jit-design.md` §4.2); `Emit::written_in_place`'s witnesses serve it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_undefined_keys_warning_reloads_nothing() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "set_error_handler(function () { $GLOBALS['x'] = ['k' => 2]; return true; });
        function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a['k']; $a[$i + 100] += 1; $s += $a['k']; }
            return $s;
        }
        $x = ['k' => 1]; echo f($x, 3), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let at = inline_at(&ir, Opcode::AssignDimOp)?;
    assert_eq!(reloaded(&ir, at), Vec::<u32>::new(), "{rendered}");
    let read = next_read(&ir, at, 0).ok_or("no read of $a after the write")?;
    assert!(
        ir.guards.iter().any(|guard| matches!(
            guard.check,
            Check::Deref { value, .. } | Check::Layout { value, .. } if value == read
        )),
        "the reference's inner value is held again at its read: {rendered}"
    );
    Ok(())
}

/// A read under `??` over a temporary array is inline and frees the
/// temporary after, whose element's destructor writes the variable a
/// reference reaches and a global array: the free is the release kernel
/// over a temporary, which revision 83's analysis covers (`jit-design.md`
/// §4.2), so the trace reads nothing again after it, and the reference's
/// inner type is held at its next read. `Emit::fetch_is`'s witnesses
/// serve it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_temporarys_free_reloads_nothing() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "class D { function __construct(public $n) {} function __destruct() { global $g; $g = $this->n; $GLOBALS['l'][0] = $this->n; } }
        function f(&$r, $n) {
            global $l; $s = 0;
            for ($i = 0; $i < $n; $i++) { $r = 0; $l = [1]; $s += [new D($i), 5][1] ?? 0; $s += $r + $l[0]; }
            return $s;
        }
        $g = 0; $l = []; echo f($g, 3), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let at = inline_at(&ir, Opcode::FetchDimIs)?;
    assert!(
        matches!(&ir.ops[at.index()], Op::Line(line) if line.unwind.is_some()),
        "the free unwinds where a destructor throws: {rendered}"
    );
    assert_eq!(reloaded(&ir, at), Vec::<u32>::new(), "{rendered}");
    let read = next_read(&ir, at, 0).ok_or("no read of $r after the free")?;
    assert!(
        ir.guards
            .iter()
            .any(|guard| matches!(guard.check, Check::Deref { value, .. } if value == read)),
        "the reference's inner value is held again at its read: {rendered}"
    );
    Ok(())
}

/// A write to a shared hash is its handler's, which copies the array,
/// and one over an object gives the object up, whose destructor writes
/// the variables a reference reaches: `ASSIGN_DIM` has no `call` effect,
/// so the trace reads nothing again after it, as revision 83's analysis
/// has it (`jit-design.md` §4.2); `Emit::written_in_place`'s witnesses serve it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_hashs_separation_and_a_destructor_reload_nothing() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "class D { function __destruct() { $GLOBALS['r'] = [7]; } }
        function f(array &$t, int $n) {
            $h = ['a' => new D, 'b' => 0]; $s = 0;
            for ($i = 0; $i < $n; $i++) { $c = $h; $h['a'] = $i; $s += $t[0] + $c['b'] * 0; $h['b'] = new D; $h['b'] = $i; }
            return $s;
        }
        $r = [1]; echo f($r, 3), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let at = inline_at(&ir, Opcode::AssignDim)?;
    assert_eq!(reloaded(&ir, at), Vec::<u32>::new(), "{rendered}");
    Ok(())
}

/// A write, a compound write and a fetch for writing through the `VAR` a
/// fetch for writing left pointing at a reference, inline under one guard
/// that follows the `VAR` and then the reference to its inner type, as
/// php follows `zend_jit_fetch_indirect_var` with `zend_jit_fetch_reference`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:5078-5096`, `:4749-4770`,
/// `:5906-5930`); the fetch for writing through the reference leaves a
/// `VAR` that points at the element, which the write after it follows to
/// the zval alone (#3410). `Emit::written_array`'s witnesses serve it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_write_through_a_var_that_points_at_a_reference_is_inline() -> Result<(), String> {
    let ir = golden(INDIRECT_REF_WRITE)?;
    let rendered = dump::render(&ir);
    let mut through = Vec::new();
    for (at, op) in ir.ops.iter().enumerate() {
        let Op::Line(line) = op else { continue };
        if !matches!(
            line.opcode,
            Opcode::AssignDim | Opcode::AssignDimOp | Opcode::FetchDimW
        ) {
            continue;
        }
        assert_eq!(line.form, Form::Inline, "{:?}: {rendered}", line.opcode);
        let at = OpId(u32::try_from(at).map_err(|e| e.to_string())?);
        let pointee = (ir.guards.iter())
            .filter(|guard| guard.at == GuardAt::Before(at))
            .find_map(|guard| match (guard.check, line.op1) {
                (
                    Check::Indirect {
                        value,
                        tag: ZvalType::Array,
                        pointee,
                    },
                    Operand::Value(op1),
                ) if value == op1 => Some(pointee),
                _ => None,
            });
        if let Some(pointee) = pointee {
            through.push((line.opcode, pointee));
        }
    }
    assert_eq!(
        through,
        [
            (Opcode::AssignDim, Pointee::Reference),
            (Opcode::AssignDimOp, Pointee::Reference),
            (Opcode::FetchDimW, Pointee::Reference),
            (Opcode::AssignDim, Pointee::Value),
        ],
        "{rendered}"
    );
    Ok(())
}

/// The fixtures of the shapes the hash slice left to its handler, and
/// the oplines each holds inline (#3370).
const NESTED_DIMS: [(Fixture, &[Opcode]); 3] = [
    (
        NESTED_READ,
        &[
            Opcode::IssetIsemptyDimObj,
            Opcode::FetchDimIs,
            Opcode::FetchDimR,
        ],
    ),
    (
        NESTED_WRITE,
        &[Opcode::AssignDim, Opcode::AssignDimOp, Opcode::FetchDimW],
    ),
    (REF_WRITE, &[Opcode::AssignDim]),
];

/// What the hash slice left to the handler, inline as php's code
/// generator compiles it (#3339's successor, #3370): `isset()`, `??` and a
/// read over a temporary a fetch made, which each frees after
/// (`zend_jit_isset_isempty_dim`'s `jit_FREE_OP`); a write, a compound
/// write and a fetch for writing through the `VAR` a fetch for writing
/// left indirect, under a guard on what it points at
/// (`zend_jit_fetch_indirect_var`), which the inline fetch for writing
/// left so the guard tests nothing of the `VAR` itself; and a write to the
/// array a reference holds, through its inner type's guard
/// (`zend_jit_prepare_array_update`). `fetch_dim_w`'s, `fetch_is`'s and
/// `assign_dim`'s witnesses serve them.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_dimension_over_a_temporary_an_indirect_var_or_a_reference_is_inline() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "function f(array &$r, int $n) {
            $h = ['n' => [5], 'a' => []]; $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += isset($h['n'][0]); $h['a'][$i & 3] = $i; $r['k'] = $i; }
            return $s;
        }
        $x = ['k' => 0]; echo f($x, 3), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let lines = |opcode: Opcode| -> Vec<(OpId, &Line)> {
        (ir.ops.iter().enumerate())
            .filter_map(|(at, op)| match op {
                Op::Line(line) if line.opcode == opcode => {
                    Some((OpId(u32::try_from(at).ok()?), line))
                }
                _ => None,
            })
            .collect()
    };
    let held = |at: OpId, of: Operand| {
        (ir.guards.iter())
            .filter(|guard| guard.at == GuardAt::Before(at))
            .map(|guard| guard.check)
            .find(|check| match (check, of) {
                (
                    Check::Deref { value, .. } | Check::Indirect { value, .. },
                    Operand::Value(op1),
                ) => *value == op1,
                _ => false,
            })
    };
    for opcode in [
        Opcode::IssetIsemptyDimObj,
        Opcode::AssignDim,
        Opcode::FetchDimW,
    ] {
        for (_, line) in lines(opcode) {
            assert_eq!(line.form, Form::Inline, "{opcode:?}: {rendered}");
        }
    }
    let writes = lines(Opcode::AssignDim);
    assert_eq!(writes.len(), 2, "{rendered}");
    assert!(
        matches!(
            held(writes[0].0, writes[0].1.op1),
            Some(Check::Indirect {
                tag: ZvalType::Array,
                ..
            })
        ),
        "the write through the `VAR` is held to what it points at: {rendered}"
    );
    assert!(
        matches!(
            held(writes[1].0, writes[1].1.op1),
            Some(Check::Deref {
                tag: ZvalType::Array,
                ..
            })
        ),
        "the write through the reference is held to its inner type: {rendered}"
    );
    for (fixture, opcodes) in NESTED_DIMS {
        let ir = golden(fixture)?;
        let rendered = dump::render(&ir);
        for opcode in opcodes {
            let forms: Vec<Form> = (ir.ops.iter())
                .filter_map(|op| match op {
                    Op::Line(line) if line.opcode == *opcode => Some(line.form),
                    _ => None,
                })
                .collect();
            assert!(
                !forms.is_empty(),
                "{}: no {opcode:?} in {rendered}",
                fixture.name
            );
            assert!(
                forms.iter().all(|form| *form == Form::Inline),
                "{}: {opcode:?} in {rendered}",
                fixture.name
            );
        }
    }
    Ok(())
}

/// A call that sends arrays by reference to typed parameters is made in
/// place, the two references' `RECV` checks inline, the array and the
/// float sent by value known to pass theirs, and its `SEND_REF`s inline,
/// as php's trace compiles `zend_jit_send_ref` and `zend_jit_recv` (#3340).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_by_reference_is_made_in_place_with_its_checks() -> Result<(), String> {
    let ir = golden(REF_CALL)?;
    let rendered = dump::render(&ir);
    let line = |opcode| {
        (ir.ops.iter()).find_map(|op| match op {
            Op::Line(line) if line.opcode == opcode => Some(line),
            _ => None,
        })
    };
    let made = line(Opcode::DoUcall).ok_or_else(|| format!("no DO_UCALL in\n{rendered}"))?;
    let checked: Vec<u32> = match made.imm {
        super::Imm::Make { checks, .. } => checks.each().map(|(at, _)| at).collect(),
        _ => return Err(format!("the call is not made in place\n{rendered}")),
    };
    assert_eq!(checked, [0, 1], "{rendered}");
    let sent = line(Opcode::SendRef).ok_or_else(|| format!("no SEND_REF in\n{rendered}"))?;
    assert_eq!(sent.form, Form::Inline, "{rendered}");
    Ok(())
}

/// A method's frame is left in place, its receiver given up after its
/// variables, as php's `zend_jit_leave_func` gives it up (#3343).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_methods_frame_is_left_in_place_with_its_receiver() -> Result<(), String> {
    let ir = golden(METHOD_CALL)?;
    let rendered = dump::render(&ir);
    let leave = (ir.ops.iter())
        .find_map(|op| match op {
            Op::Line(line) if line.opcode == Opcode::Return && line.frame != Frame::ROOT => {
                Some(line)
            }
            _ => None,
        })
        .ok_or_else(|| format!("no RETURN of the method in\n{rendered}"))?;
    assert!(
        matches!(leave.imm, super::Imm::Leave { receives: true, .. }),
        "{rendered}"
    );
    assert!(leave.unwind.is_some(), "{rendered}");
    Ok(())
}

/// The line of `opcode` in `ir`, the first.
fn line_of(ir: &TraceIr, opcode: Opcode) -> Option<&super::Line> {
    ir.ops.iter().find_map(|op| match op {
        Op::Line(line) if line.opcode == opcode => Some(line),
        _ => None,
    })
}

/// A method called on a variable's object of a published class is pushed
/// in place under the class guard, its send compiled by value and the
/// call made in place, as php's `zend_jit_init_method_call` pushes it
/// once its class guard holds (#3343).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_methods_call_is_pushed_in_place_under_its_class_guard() -> Result<(), String> {
    let ir = golden(METHOD_CALL)?;
    let rendered = dump::render(&ir);
    let init = line_of(&ir, Opcode::InitMethodCall)
        .ok_or_else(|| format!("no INIT_METHOD_CALL in\n{rendered}"))?;
    assert_eq!(init.form, Form::Inline, "{rendered}");
    assert!(
        (ir.guards.iter()).any(|guard| matches!(
            guard.check,
            Check::Class {
                object: super::Object::Value(_),
                ..
            }
        )),
        "{rendered}"
    );
    // `SEND_VAR_EX` compiled as the `SEND_VAR` its callee takes by value.
    let send =
        line_of(&ir, Opcode::SendVar).ok_or_else(|| format!("no SEND_VAR in\n{rendered}"))?;
    assert_eq!(send.form, Form::Inline, "{rendered}");
    let made =
        line_of(&ir, Opcode::DoFcall).ok_or_else(|| format!("no DO_FCALL in\n{rendered}"))?;
    assert!(matches!(made.imm, super::Imm::Make { .. }), "{rendered}");
    Ok(())
}

/// A static method's call and `new`'s constructor are pushed by their
/// handlers, and a guard after each holds the callee the trace recorded,
/// as php's `zend_jit_func_guard` does; the sends after it are compiled by
/// value and the call made in place (#3343).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_its_handler_pushes_is_held_to_its_callee() -> Result<(), String> {
    for (fixture, opcode) in [
        (STATIC_CALL, Opcode::InitStaticMethodCall),
        (NEW_CALL, Opcode::New),
    ] {
        let ir = golden(fixture)?;
        let rendered = dump::render(&ir);
        let init = line_of(&ir, opcode).ok_or_else(|| format!("no {opcode:?} in\n{rendered}"))?;
        assert_eq!(init.form, Form::Step, "{rendered}");
        assert!(
            (ir.guards.iter()).any(|guard| matches!(guard.check, Check::Callee { .. })),
            "{rendered}"
        );
        let made = (ir.ops.iter())
            .find_map(|op| match op {
                Op::Line(line) if matches!(line.opcode, Opcode::DoFcall | Opcode::DoUcall) => {
                    Some(line)
                }
                _ => None,
            })
            .ok_or_else(|| format!("no DO_*CALL in\n{rendered}"))?;
        assert!(matches!(made.imm, super::Imm::Make { .. }), "{rendered}");
        let sends = [
            Opcode::SendVar,
            Opcode::SendVal,
            Opcode::SendVarEx,
            Opcode::SendValEx,
        ];
        assert!(
            (ir.ops.iter()).all(|op| !matches!(op, Op::Line(line)
                if sends.contains(&line.opcode) && line.form == Form::Step)),
            "{rendered}"
        );
    }
    Ok(())
}

/// A method of a request-scoped class, whose id and op array are the
/// request's, is pushed by its handler with no class guard or callee
/// held, and its sends stay the handlers' (#3343).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_request_scoped_classs_method_is_pushed_by_its_handler() -> Result<(), String> {
    let ir = golden(METHOD_SCOPED)?;
    let rendered = dump::render(&ir);
    let init = line_of(&ir, Opcode::InitMethodCall)
        .ok_or_else(|| format!("no INIT_METHOD_CALL in\n{rendered}"))?;
    assert_eq!(init.form, Form::Step, "{rendered}");
    assert!(
        (ir.guards.iter())
            .all(|guard| !matches!(guard.check, Check::Class { .. } | Check::Callee { .. })),
        "{rendered}"
    );
    Ok(())
}

/// The clobber rule at a method's leave in place whose receiver's
/// destructor runs (`free_this`): no `call` effect, so the caller's
/// variables, a reference among them, are read again by nothing, as
/// php's code keeps every type and register across `OBJ_RELEASE`
/// (`Emit::free_this`'s witnesses hold such a destructor) (#3343).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_leave_whose_receiver_is_destroyed_reloads_nothing() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "class D { function m($i) { return $i; } function __destruct() { } }
        function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; $d = new D; $s += $d->m($i); unset($d); $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let (at, leave) = (ir.ops.iter().enumerate())
        .find_map(|(at, op)| match op {
            Op::Line(line) if line.opcode == Opcode::Return => Some((at, line)),
            _ => None,
        })
        .ok_or_else(|| format!("no RETURN in\n{rendered}"))?;
    assert!(
        matches!(leave.imm, super::Imm::Leave { receives: true, .. }),
        "{rendered}"
    );
    assert!(leave.unwind.is_some(), "a destructor may throw: {rendered}");
    let at = OpId(u32::try_from(at).map_err(|_| "a long trace")?);
    assert_eq!(reloaded(&ir, at), Vec::<u32>::new(), "{rendered}");
    Ok(())
}

/// The clobber rule at `new`, which has no `call` effect, and whose
/// constructor, made in place and followed, may throw: nothing is read
/// again after the `NEW`, a reference's inner type guarded where it is
/// read as always, and the constructor's call unwinds where it throws
/// (#3343).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_new_whose_constructor_may_throw_reloads_nothing() -> Result<(), String> {
    let ir = built(
        &IN_F,
        "class C { public $v; function __construct($v) { if ($v > 99) { throw new Exception; } $this->v = $v; } }
        function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; $c = new C($i); $s += $c->v; $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    let rendered = dump::render(&ir);
    let at = stepped(&ir, Opcode::New)?;
    assert_eq!(reloaded(&ir, at), Vec::<u32>::new(), "{rendered}");
    let made =
        line_of(&ir, Opcode::DoFcall).ok_or_else(|| format!("no DO_FCALL in\n{rendered}"))?;
    assert!(matches!(made.imm, super::Imm::Make { .. }), "{rendered}");
    assert!(made.unwind.is_some(), "{rendered}");
    Ok(())
}
