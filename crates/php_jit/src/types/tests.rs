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

use php_unit::Opcode;

use super::{
    Check, Def, End, Form, Frame, Landing, Op, OpId, Operand, Repeat, Slot, TraceIr, ValueId,
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
/// nothing again: it may only leave.
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
    let exit = line.exit.ok_or("a throwing kernel leaves")?;
    assert_eq!(ir.exits[usize::from(exit.0)].landing, Landing::Plain);
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
            .any(|guard| matches!(guard.check, Check::Caller { .. }))
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
    // exit of its own, at the fetch, past it.
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
        .freed
        .ok_or("the free of the fetched array leaves nowhere")?;
    let exit = &ir.exits[usize::from(freed.0)];
    assert_eq!(exit.opline, Some(fetch.opline));
    assert_ne!(Some(freed), fetch.exit);
    Ok(())
}
