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
use php_unit::Opcode;

use super::{
    Check, Def, End, Form, Frame, GuardAt, Home, Landing, Op, OpId, Operand, Repeat, Slot, TraceIr,
    Types, ValueId,
};
use crate::lower::dump;
use crate::record::Stop;
use crate::testing::goldens::{
    self, FIB, Fixture, MANDELBROT, NBODY, SPECTRALNORM, held, traced as built,
};

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

/// A return trace: its root's return lands in a caller the guard holds,
/// and the trace goes on there.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_trace_guards_the_caller_it_lands_in() -> Result<(), String> {
    let ir = built(
        &[("record.function", "g"), ("record.start", "return")],
        "function g(int $x) { return $x + 1; }
        function f() { $a = g(1); return $a * 2; }
        echo f(), \"\\n\";",
    )?;
    assert!(
        ir.guards
            .iter()
            .any(|guard| matches!(guard.check, Check::Stands { .. }))
    );
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
/// code has not freed: `Landing::FreeTemps`.
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
        ir.exits.iter().any(|exit| exit.landing
            == Landing::FreeTemps {
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
/// trace knows of the frame's slots differ from an earlier one's.
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
