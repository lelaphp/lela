//! `lower` at its seam (`jit-design.md` §4.3 *Tested*, §7): the CLIF of
//! each `types` fixture's trace, held to a golden file, every loop's
//! carrying the interrupt test; the same function compiled to bytes for
//! the golden's ISA and the host's, a mark per recorded opline; and a
//! host with no ISA refused as `Unsupported`.
//!
//! The goldens are lowered for x86-64 on the System V convention whatever
//! the host, with the kernels at made-up addresses, so the text is one on
//! every machine; with `PHP_JIT_BLESS=1` set, the tests write them.

use std::path::PathBuf;

use cranelift_codegen::isa;

use super::{Backend, INTERRUPT_AT, INTERRUPTED, Kernel, Symbols, UNWOUND, Unsupported};
use crate::table::TraceName;
use crate::testing::goldens::{
    self, ARRAY_WRITE, BINARY_TREES_CHECK, BINARY_TREES_MAKE, FIB, Fixture, MANDELBROT,
    NATIVE_CALL, NBODY, OWNING_LEAVE, SIEVE, SPECTRALNORM, held,
};
use crate::testing::{self, Lane, policy};
use crate::types::{Check, End, Guard, GuardAt, Imm, Interrupt, Op, Returned, TraceIr};

/// Where `lower`'s goldens are.
fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lower/goldens")
}

/// Each kernel at an address of its own that no code is at.
fn symbols() -> Symbols {
    Symbols::new(|kernel| 0x7000_0000 + 0x100 * kernel.index())
}

/// The `Backend` the goldens are lowered with: x86-64 on System V.
fn golden_backend() -> Result<Backend, Unsupported> {
    Backend::on(
        || isa::lookup_by_name("x86_64-unknown-linux-gnu").map_err(|_| "x86-64 is not built"),
        &policy(&[]),
        symbols(),
    )
}

/// What a profiler would name a fixture's trace.
fn named(fixture: Fixture) -> TraceName {
    TraceName {
        at: fixture.name.as_bytes().into(),
        line: 1,
    }
}

/// `clif` with the address of each constant the trace copies, which the
/// process's allocation decides, named by its place among them, so a golden
/// holds what the code does with it.
fn constants_named(ir: &TraceIr, clif: &str) -> String {
    let mut named = clif.to_owned();
    let constants = ir.ops.iter().filter_map(|op| match op {
        Op::Line(line) => match line.imm {
            Imm::Leave {
                value: Returned::Constant(address),
                ..
            } => Some(address),
            _ => None,
        },
        _ => None,
    });
    for (at, address) in constants.enumerate() {
        // CLIF prints a constant in groups of four hex digits.
        let digits = format!("{address:x}");
        let mut grouped = String::new();
        for (i, digit) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(4) {
                grouped.push('_');
            }
            grouped.push(digit);
        }
        named = named.replace(&format!("0x{grouped}"), &format!("constant#{at}"));
    }
    named
}

/// The fixture's trace lowered: its CLIF held to `<name>.clif`, the
/// interrupt test found in it where the trace loops, and the function
/// compiled for the golden's ISA and the host's.
fn lowered(fixture: Fixture) -> Result<TraceIr, String> {
    let recorded = goldens::recorded(fixture)?;
    let ir = goldens::built(fixture, &recorded)?;
    let backend = golden_backend().map_err(|e| format!("{e:?}"))?;
    let clif = backend
        .clif(&ir, None)
        .map_err(|abort| format!("{}: {abort:?}", fixture.name))?
        .display()
        .to_string();
    held(
        &goldens_dir().join(format!("{}.clif", fixture.name)),
        &constants_named(&ir, &clif),
    )?;
    if let End::Loop(header) = ir.end {
        let answers = match header.interrupt {
            Interrupt::Exit(exit) => i64::from(exit.0),
            // CLIF writes an `i32` constant signed.
            Interrupt::Stub(_) => i64::from(i32::from_ne_bytes(INTERRUPTED.to_ne_bytes())),
        };
        interrupt_test(&clif, answers)?;
    }
    let host = Backend::new(&policy(&[]), symbols()).map_err(|e| format!("{e:?}"))?;
    for backend in [&backend, &host] {
        let compiled = backend
            .compile(&ir, named(fixture), None)
            .map_err(|abort| format!("{}: {abort:?}", fixture.name))?;
        assert!(!compiled.bytes.is_empty());
        assert!(compiled.align.is_power_of_two());
        assert_eq!(compiled.exits.len(), ir.exits.len());
        assert_eq!(compiled.map_ptrs.len(), ir.map_ptrs.len());
        let lines = ir.ops.iter().filter(|op| matches!(op, Op::Line(_))).count();
        assert_eq!(compiled.marks.len(), lines, "a mark per recorded opline");
        let end = u32::try_from(compiled.bytes.len()).unwrap_or(u32::MAX);
        assert!(compiled.marks.iter().all(|mark| mark.offset <= end));
    }
    Ok(ir)
}

/// The interrupt test in a loop's CLIF (`jit-design.md` §4.3): the
/// interrupt pointer loaded from the `ExitCtx` once, and the byte it
/// points to loaded and branched on to a block that answers the header's
/// interrupt exit, or [`INTERRUPTED`] where the test takes php's stub.
fn interrupt_test(clif: &str, exit: i64) -> Result<(), String> {
    let lines: Vec<&str> = clif.lines().map(instruction).collect();
    let pointer = lines
        .iter()
        .find_map(|line| {
            let (value, load) = line.split_once(" = ")?;
            (load == format!("load.i64 notrap aligned v0+{INTERRUPT_AT}")).then_some(value)
        })
        .ok_or_else(|| format!("no interrupt pointer loaded:\n{clif}"))?;
    let flag = lines
        .iter()
        .find_map(|line| {
            let (value, load) = line.split_once(" = ")?;
            (load == format!("load.i8 notrap aligned {pointer}")).then_some(value)
        })
        .ok_or_else(|| format!("the interrupt flag is never loaded:\n{clif}"))?;
    let branch = lines
        .iter()
        .find(|line| line.starts_with(&format!("brif {flag}, ")))
        .ok_or_else(|| format!("the interrupt flag is never branched on:\n{clif}"))?;
    let target = branch
        .trim_start_matches(&format!("brif {flag}, "))
        .split(',')
        .next()
        .unwrap_or_default()
        .trim();
    if answer(clif, target) == Some(exit) {
        Ok(())
    } else {
        Err(format!(
            "the interrupt test's block does not answer exit {exit}:\n{clif}"
        ))
    }
}

/// What the cold block `block` of `clif` answers: the number it returns,
/// the last constant it makes, past the type words it stores.
fn answer(clif: &str, block: &str) -> Option<i64> {
    clif.lines()
        .map(instruction)
        .skip_while(|line| *line != format!("{block} cold:"))
        .skip(1)
        .take_while(|line| !line.ends_with(':'))
        .filter_map(|line| line.split_once("iconst.i32 ")?.1.parse().ok())
        .last()
}

/// A line of CLIF text as its instruction: the indent and the source
/// location before it dropped.
fn instruction(line: &str) -> &str {
    let line = line.trim();
    match line.strip_prefix('@') {
        Some(located) => located
            .split_once(char::is_whitespace)
            .map_or(line, |(_, rest)| rest.trim_start()),
        None => line,
    }
}

#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn fib_lowers_its_recursion_with_the_interrupt_test() -> Result<(), String> {
    lowered(FIB).map(drop)
}

#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn mandelbrot_lowers_its_escape_loop_with_the_interrupt_test() -> Result<(), String> {
    lowered(MANDELBROT).map(drop)
}

#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn nbody_lowers_its_inner_loop_with_the_interrupt_test() -> Result<(), String> {
    lowered(NBODY).map(drop)
}

#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn spectralnorm_lowers_its_inlined_call_with_the_interrupt_test() -> Result<(), String> {
    lowered(SPECTRALNORM).map(drop)
}

/// `binary_trees`' two recursions, `check_tree` entered and `make_tree`
/// from its call's landing, lowered with what php's code generator
/// compiles inline of them (#2963).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn binary_trees_lowers_its_two_recursions() -> Result<(), String> {
    for fixture in [BINARY_TREES_CHECK, BINARY_TREES_MAKE] {
        lowered(fixture)?;
    }
    Ok(())
}

/// `array_write`'s and `sieve`'s element writes, inline: written in
/// place, or through `step` where the array is not the variable's own,
/// not packed, or the element is past `nNumUsed`, a hole or counted,
/// taking no exit (#3228).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_element_write_lowers_inline_and_steps_where_it_cannot() -> Result<(), String> {
    for fixture in [ARRAY_WRITE, SIEVE] {
        lowered(fixture)?;
    }
    Ok(())
}

/// A native's call pushed in place and its `DO_ICALL` served by the
/// `handle` kernel, the `Stood` written past it (#2963).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_natives_call_lowers_in_place() -> Result<(), String> {
    lowered(NATIVE_CALL).map(drop)
}

/// A leave over owning variables, each freed in place: its type tested,
/// a `GC_DELREF` inline where another owner stands and the collector holds
/// or never takes the value, and the `free_cv` kernel otherwise (#3313).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_leave_over_owning_variables_lowers_its_frees() -> Result<(), String> {
    lowered(OWNING_LEAVE)?;
    let clif = std::fs::read_to_string(goldens_dir().join("owning_leave.clif"))
        .map_err(|e| e.to_string())?;
    let address = format!("{:#x}", symbols().address(Kernel::FreeCv)).replace("0x7000", "0x7000_");
    let frees = (clif.lines())
        .filter(|line| line.contains("call_indirect") && line.contains(&address))
        .count();
    // One kernel call per variable freed, `$a` and `$s`.
    assert_eq!(frees, 2, "{clif}");
    Ok(())
}

/// A host Cranelift has no ISA for is `Unsupported`, which runs the
/// tier `Off` (`jit-design.md` §4.3): the ISA lookup stubbed to find none.
#[test]
fn a_host_with_no_isa_is_unsupported() {
    let backend = Backend::on(|| Err("no backend"), &policy(&[]), symbols());
    assert_eq!(backend.err(), Some(Unsupported("no backend")));
}

/// `program`'s trace under `plan`, compiled for the goldens' ISA and the
/// host's, and its CLIF.
fn compiled(plan: &[(&str, &str)], program: &str) -> Result<(TraceIr, String), String> {
    let ir = goldens::traced(plan, program)?;
    let golden = golden_backend().map_err(|e| format!("{e:?}"))?;
    let host = Backend::new(&policy(&[]), symbols()).map_err(|e| format!("{e:?}"))?;
    for backend in [&golden, &host] {
        backend
            .compile(
                &ir,
                TraceName {
                    at: b"f".as_slice().into(),
                    line: 1,
                },
                None,
            )
            .map_err(|abort| format!("{abort:?}"))?;
    }
    let clif = golden
        .clif(&ir, None)
        .map_err(|abort| format!("{abort:?}"))?
        .display()
        .to_string();
    Ok((ir, clif))
}

/// The calls of `kernel` in `clif`, each with the block its status
/// branches to where it is not zero.
fn calls_of(clif: &str, kernel: Kernel) -> Vec<String> {
    let address = format!("{:#x}", symbols().address(kernel)).replace("0x7000", "0x7000_");
    let lines: Vec<&str> = clif.lines().map(instruction).collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("call_indirect") && line.contains(&address))
        .filter_map(|(at, line)| {
            let status = line.split_once(" = ")?.0;
            let branch = lines.get(at + 1)?;
            let to = branch.strip_prefix(&format!("brif {status}, "))?;
            Some(to.split(',').next()?.trim().to_owned())
        })
        .collect()
}

/// A loop in `f`, where the plan records.
const IN_F: [(&str, &str); 2] = [("record.function", "f"), ("record.start", "loop")];

/// A fetch from a temporary array frees it through [`Kernel::Release`],
/// whose refusal leaves through the unwind `types` gives the free, never
/// abandoned and taking no exit; the fetched element's guard copies it,
/// its owner added inline, for the exit that goes on past the fetch.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_temporary_arrays_free_leaves_through_an_unwind() -> Result<(), String> {
    let (ir, clif) = compiled(
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
        ir.ops
            .iter()
            .any(|op| matches!(op, Op::Line(line) if line.unwind.is_some())),
        "no fetch frees a temporary"
    );
    let releases = calls_of(&clif, Kernel::Release);
    assert!(!releases.is_empty(), "{clif}");
    for to in releases {
        assert_eq!(
            answer(&clif, &to),
            Some(i64::from(UNWOUND.cast_signed())),
            "{clif}"
        );
    }
    // The element's copy adds its owner inline, as php's `GC_ADDREF`,
    // calling no kernel.
    Ok(())
}

/// A recursive return trace lands in a caller php's IP guard holds: the
/// caller's op array and opline read from where the return's kernel
/// stood.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_trace_holds_its_caller_by_where_the_return_stood() -> Result<(), String> {
    let (ir, clif) = compiled(
        &[("record.function", "r"), ("record.start", "return")],
        "function r($n) { if ($n == 0) { return 0; } $x = r($n - 1); return $x + 1; }
        echo r(5), \"\\n\";",
    )?;
    assert!(matches!(ir.end, End::Loop(_)));
    assert!(
        clif.contains("stack_addr.i64 ss0+16") && clif.contains("stack_addr.i64 ss0+20"),
        "the landed op array and opline are read:\n{clif}"
    );
    Ok(())
}

/// A trace that links leaves through its last exit, which names the
/// trace dispatch follows.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_link_answers_its_last_exit() -> Result<(), String> {
    let (ir, clif) = compiled(
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
        return Err("a link loops".to_owned());
    };
    let last = clif
        .lines()
        .map(instruction)
        .filter_map(|line| line.strip_prefix("jump "))
        .find(|to| answer(&clif, to.trim()) == Some(i64::from(exit.0)));
    assert!(
        last.is_some(),
        "the trace's end jumps to its last exit:\n{clif}"
    );
    Ok(())
}

/// A native's call is pushed in place, its sends of constants stepped,
/// the operands they read stored first, and its `DO_ICALL` served by its
/// handler through [`Kernel::Handle`] (#2963).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_natives_call_is_served_by_its_handler() -> Result<(), String> {
    let (_, clif) = compiled(
        &IN_F,
        "function f(array &$a, int $n) {
            $s = 0;
            for ($i = 0; $i < $n; $i++) { $s += $a[0]; strcmp('a', 'b'); $s += $a[0]; }
            return $s;
        }
        $x = [1]; echo f($x, 3), \"\\n\";",
    )?;
    assert!(!calls_of(&clif, Kernel::Step).is_empty(), "{clif}");
    assert!(!calls_of(&clif, Kernel::Handle).is_empty(), "{clif}");
    Ok(())
}

/// A guard on a boolean the trace keeps in a register is tested there:
/// each loop, compiled at its first iteration under `jit.mode=force`,
/// where `$i < 100` was true, leaves where it turns false, so each counts
/// the hundred iterations php counts rather than going the recorded way
/// to the end, its loop compiled rather than aborted on a guard the
/// lowering does not serve (#3076).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_guarded_boolean_is_tested_where_the_trace_runs() {
    let ini = [
        ("jit.mode", "force"),
        ("jit.compile", "inline"),
        ("opcache.jit_debug", "258048"),
    ];
    for program in [
        "function f() { $h = 0; for ($i = 0; $i < 200; $i++) { if ($i < 100 && $i >= 0) { $h++; } } return $h; }
        echo f(), \"\\n\";",
        "function f() { $h = 0; $b = true; for ($i = 0; $i < 200; $i++) { if ($b) { $h++; } $b = $i < 99; } return $h; }
        echo f(), \"\\n\";",
    ] {
        let served = testing::serve::<Lane>(&ini, program);
        let lines = String::from_utf8_lossy(&served.stderr).into_owned();
        assert_eq!(testing::stdout(&served), "100\n", "{program}\n{lines}");
        assert!(
            lines.contains("---- TRACE 1 stop (loop)\n---- TRACE 1 compiled"),
            "{lines}"
        );
        assert!(!testing::MAPS_CODE || lines.contains(" exit "), "{lines}");
    }
}

/// A table's jump, `SWITCH_STRING`, `SWITCH_LONG` or `MATCH`, served
/// through `step`, is held to the opline the trace serves next, as php's
/// `zend_jit_switch` leaves for any target the trace did not follow: each
/// loop, compiled at its first iteration under `jit.mode=force` where the
/// table went to its default, leaves where it goes to a case, so each
/// prints what php prints rather than the recorded way to the end; under
/// `exit-fuzz` the guard is failed once too, its exit landing where the
/// step left the frame (Zend/tests/match/001.phpt, match/008.phpt,
/// switch/bug26696.phpt).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_tables_jump_leaves_where_it_goes_another_way() {
    for (program, printed) in [
        (
            "function f($a) { for ($i = 0; $i < 4; $i++) { switch ($a[$i]) { case 'b': echo 'T'; break; case 'c': echo 'C'; break; default: echo 'd'; } } }
            f(['a', 'a', 'b', 'c']); echo \"\\n\";",
            "ddTC\n",
        ),
        (
            "function f() { for ($i = 0; $i < 7; $i++) { switch ($i) { case 2: echo 'b'; break; case 3: echo 'c'; break; case 4: echo 'd'; break; case 5: echo 'e'; break; case 6: echo 'f'; break; default: echo '-'; } } }
            f(); echo \"\\n\";",
            "--bcdef\n",
        ),
        (
            "function f() { for ($i = 0; $i < 4; $i++) { echo match ($i) { 1 => 'one', 2 => 'two', 3 => 'three', default => 'none' }, ' '; } }
            f(); echo \"\\n\";",
            "none one two three \n",
        ),
    ] {
        for mode in ["force", "exit-fuzz"] {
            let ini = [
                ("jit.mode", mode),
                ("jit.compile", "inline"),
                ("opcache.jit_debug", "258048"),
            ];
            let served = testing::serve::<Lane>(&ini, program);
            let lines = String::from_utf8_lossy(&served.stderr).into_owned();
            assert_eq!(testing::stdout(&served), printed, "{mode} {program}\n{lines}");
            assert!(lines.contains("---- TRACE 1 compiled"), "{lines}");
        }
    }
}

/// A packed fetch from a temporary array takes the array out of its slot
/// as the interpreter's `FREE_OP1` does, so the slot owns nothing once the
/// array is released and the return that follows finds every temporary
/// empty, where the slot had kept the freed array's counted type word
/// (`Zend/tests/numeric_strings/invalid_numeric_string_must_generate_warning_assign.phpt`).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fetch_from_a_temporary_array_leaves_its_slot_empty() {
    // The throw aborts `$main`'s recording, so `f` is a root of its own,
    // entered on its second call.
    let program = "function f($v) { return [$v][0]; }
        try { throw new Exception; } catch (Exception $e) { echo 'e '; }
        $x = 'a'; $x .= 'a';
        echo f($x), f($x . 'b'), f(1), \"\\n\";";
    for mode in ["force", "exit-fuzz"] {
        let ini = [
            ("jit.mode", mode),
            ("jit.compile", "inline"),
            ("opcache.jit_debug", "258048"),
        ];
        let served = testing::serve::<Lane>(&ini, program);
        let lines = String::from_utf8_lossy(&served.stderr).into_owned();
        assert_eq!(served.fatal, None, "{mode}\n{lines}");
        assert_eq!(testing::stdout(&served), "e aaaab1\n", "{mode}\n{lines}");
        assert!(lines.contains("---- TRACE 1 start (enter) f()"), "{lines}");
    }
}

/// A `DO_FCALL`, whose callee a closure's or a method's call may vary, is
/// held to the opline the trace serves next, php's IP guard after it: each
/// loop, recorded where `$g` was the `int` closure, leaves where it is
/// the `float` one, whose parameter php coerced, rather than step the
/// `int` closure's code over it
/// (`Zend/tests/type_declarations/scalar_basic.phpt`,
/// `scalar_return_basic_64bit.phpt`, `scalar_strict_64bit.phpt`).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_another_function_answers_leaves_where_it_enters() {
    let program = "function f($gs) {
            foreach ($gs as $g) { foreach ([1, 9223372036854775807] as $v) { $r = $g($v); echo $r + 0, ' '; } }
        }
        f([function (int $i) { return $i; }, function (float $f) { return $f; }]);
        echo \"\\n\";";
    for mode in ["force", "exit-fuzz"] {
        let ini = [
            ("jit.mode", mode),
            ("jit.compile", "inline"),
            ("opcache.jit_debug", "258048"),
        ];
        let served = testing::serve::<Lane>(&ini, program);
        let lines = String::from_utf8_lossy(&served.stderr).into_owned();
        assert_eq!(
            testing::stdout(&served),
            "1 9223372036854775807 1 9.2233720368548E+18 \n",
            "{mode}\n{lines}"
        );
    }
}

/// A `NEW` whose class has no constructor skips the `DO_FCALL` after it,
/// so it is held to the opline the trace serves next, as php holds a
/// `NEW` that may skip its constructor: the loop, recorded where the class
/// had one, leaves where it has none rather than call a frame nothing
/// pushed (`Zend/tests/instantiate_all_classes.phpt`).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_new_that_skips_its_constructor_leaves_where_it_does() {
    let program = "class A { function __construct() { echo 'a'; } } class B {}
        function f() { foreach (['A', 'A', 'B', 'A'] as $c) { new $c; } }
        f(); echo \"\\n\";";
    for mode in ["force", "exit-fuzz"] {
        let ini = [
            ("jit.mode", mode),
            ("jit.compile", "inline"),
            ("opcache.jit_debug", "258048"),
        ];
        let served = testing::serve::<Lane>(&ini, program);
        let lines = String::from_utf8_lossy(&served.stderr).into_owned();
        assert_eq!(served.fatal, None, "{mode}\n{lines}");
        assert_eq!(testing::stdout(&served), "aaa\n", "{mode}\n{lines}");
    }
}

/// A `VERIFY_RETURN_TYPE` over a variable that holds a reference, a
/// function's or a property hook's `&get` returning by reference, is
/// stepped: inline, its value was the scalar the reference held, and an
/// exit stored that scalar over the reference, so the return lost it and
/// the reference cell was never freed
/// (`Zend/tests/lazy_objects/init_trigger_foreach_hooks.phpt`).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_type_checked_through_a_reference_keeps_the_reference() {
    // The throw aborts `$main`'s recording, so `g` is a root of its own.
    let program = "function &g($o): int { $ref = &$o->x; return $ref; }
        try { throw new Exception; } catch (Exception $e) {}
        $o = new stdClass; $o->x = 1; echo g($o); $r = &g($o); $r = 5; echo $o->x, \"\\n\";";
    for mode in ["force", "exit-fuzz"] {
        let ini = [
            ("jit.mode", mode),
            ("jit.compile", "inline"),
            ("opcache.jit_debug", "258048"),
        ];
        let served = testing::serve::<Lane>(&ini, program);
        let lines = String::from_utf8_lossy(&served.stderr).into_owned();
        assert_eq!(served.fatal, None, "{mode}\n{lines}");
        assert_eq!(testing::stdout(&served), "15\n", "{mode}\n{lines}");
        assert!(lines.contains("---- TRACE 1 start (enter) g()"), "{lines}");
    }
}

/// A guarded step the trace ends at is held to the opline the trace goes
/// on at, as php holds the trace's next opline at every end but a return
/// and the interpreter's: a table whose recorded case is a `RETURN` the
/// trace stops before, and the last call of a recursion through a
/// variable, after which the trace goes back to its header, each leave
/// where the step went another way rather than go on where the record
/// went.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_step_the_trace_ends_at_is_held_to_where_it_goes_on() {
    for (program, printed) in [
        (
            "function f($x) { switch ($x) { case 'a': return 1; case 'b': return 2; } return 3; }
            try { throw new Exception; } catch (Exception $e) {}
            echo f('a'), f('b'), f('c'), \"\\n\";",
            "123\n",
        ),
        (
            "function s($a, $n) { return 100; }
            function r($a, $n) { if ($n == 0) { return 0; } $g = $a[$n]; return $g($a, $n - 1) + 1; }
            try { throw new Exception; } catch (Exception $e) {}
            echo r(['-', 'r', 'r', 'r'], 3), ' ', r(['-', 'r', 's', 'r'], 3), \"\\n\";",
            "3 102\n",
        ),
    ] {
        for mode in ["force", "exit-fuzz"] {
            let ini = [
                ("jit.mode", mode),
                ("jit.compile", "inline"),
                ("opcache.jit_debug", "258048"),
            ];
            let served = testing::serve::<Lane>(&ini, program);
            let lines = String::from_utf8_lossy(&served.stderr).into_owned();
            assert_eq!(served.fatal, None, "{mode} {program}\n{lines}");
            assert_eq!(
                testing::stdout(&served),
                printed,
                "{mode} {program}\n{lines}"
            );
            assert!(lines.contains("compiled"), "{lines}");
        }
    }
}

/// A side trace that returns an array its parent's exit knew the type of
/// serves `VERIFY_RETURN_TYPE` inline and lowers it to nothing: the array
/// stands in its slot, of the declared type, so there is nothing to check
/// or move, as php's code emits nothing; the lowering had taken every
/// inline `VERIFY_RETURN_TYPE` for a scalar's and refused the trace, so
/// spectralnorm's side trace past its inner loop aborted with `JIT
/// compilation error` on every attempt.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_side_trace_returning_an_array_its_parent_knew_compiles() -> Result<(), String> {
    let recorded = goldens::recorded(SPECTRALNORM)?;
    let record = crate::record::TraceRecord::parse(
        "start side 4/3\nroot e2\nentered 2:array 3:long 4:double\n\
         op e2 23 array.packed - - -\nop e2 24 array.packed - - -\nstop return\n",
    )
    .map_err(|e| format!("{e:?}"))?;
    let ir = crate::types::build(&record, &recorded.view).map_err(|abort| format!("{abort:?}"))?;
    let backend = golden_backend().map_err(|e| format!("{e:?}"))?;
    backend
        .compile(&ir, named(SPECTRALNORM), None)
        .map(|_| ())
        .map_err(|abort| format!("{abort:?}\n{}", super::dump::render(&ir)))
}

/// A trace entered at its loop's body reads where the frame stands as
/// `enter` laid it down, as one entered at its first op does: a guard at
/// the body's entry on the stack's room, php's stack check before the
/// calls the trace pushes, read it from a `Stood` no one had written, and
/// took its exit at random (`binary_trees`' `TRACE 9 exit 2` at `0037`,
/// which the pinned php does not take). The words are copied from the
/// `ExitCtx` before the entry block branches to the body; here a loop
/// with a way in at its body, held to the room a call would take.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_body_entry_reads_where_the_frame_stands() -> Result<(), String> {
    let mut ir = goldens::traced(
        &[("record.function", "f"), ("record.start", "loop")],
        "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        echo f(5), \"\\n\";",
    )?;
    let exit = (ir.guards.iter())
        .find(|guard| guard.at == GuardAt::Entry)
        .map(|guard| guard.exit)
        .ok_or("no guard at entry")?;
    ir.guards.push(Guard {
        at: GuardAt::Entry,
        check: Check::Room { bytes: 112 },
        exit,
    });
    let rendered = crate::lower::dump::render(&ir);
    assert!(ir.body.is_some(), "no way in at the body:\n{rendered}");
    assert!(
        super::stood_at_entry(&ir),
        "no stood read at entry:\n{rendered}"
    );
    let backend = golden_backend().map_err(|e| format!("{e:?}"))?;
    let clif = backend
        .clif(&ir, None)
        .map_err(|abort| format!("{abort:?}"))?
        .display()
        .to_string();
    let lines: Vec<&str> = clif.lines().map(instruction).collect();
    let stood = format!("load.i64 notrap aligned v0+{}", super::STOOD_AT);
    let read = lines.iter().position(|line| line.ends_with(&stood));
    let branch = lines.iter().position(|line| line.starts_with("brif "));
    assert!(
        matches!((read, branch), (Some(read), Some(branch)) if read < branch),
        "the stood is read after the entry branches:\n{clif}"
    );
    Ok(())
}

/// A trace's landing slots (`jit-design.md` §4.9): each written once, to
/// the first trace entered at its landing, every key tagged so none is a
/// free slot's zero, and a landing past the slots left to the crossing.
#[test]
fn a_landing_slot_names_the_first_trace_entered_at_its_landing() {
    use super::{Entrance, LANDING_TAG, LANDINGS, LandingSlots, Stands};
    let entrance = |trace| Entrance {
        body: 0x1000,
        trace,
    };
    let (first, second, third) = (entrance(1), entrance(2), entrance(3));
    let slots = LandingSlots::new();
    let key = LandingSlots::key(Stands { func: 0, opline: 0 });
    assert_eq!(
        key, LANDING_TAG,
        "the first opline of op array 0 is no free slot"
    );
    slots.remember(key, &first);
    slots.remember(key, &second);
    let at = |entrance: &Entrance| std::ptr::from_ref(entrance).addr();
    assert_eq!(slots.landed(key), Some(at(&first)));
    let other = LandingSlots::key(Stands { func: 7, opline: 9 });
    assert_eq!(other, 7 | 9 << 32 | LANDING_TAG);
    slots.remember(other, &second);
    assert_eq!(slots.landed(other), Some(at(&second)));
    assert_eq!(LANDINGS, 2);
    let past = LandingSlots::key(Stands {
        func: 7,
        opline: 10,
    });
    slots.remember(past, &third);
    assert_eq!(slots.landed(past), None);
}

/// A link the trace was compiled against is a jump in its own bytes
/// (#3229's ruling): its stores, the request's row of the linked trace,
/// and a tail call to the linked body's address, a constant, with php's
/// interrupt test before it where the trace is a side linking to its own
/// looping root; with none to link to, its last exit as before.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_link_jumps_to_the_trace_it_links_to() -> Result<(), String> {
    let ir = goldens::traced(
        &[
            ("record.function", "f"),
            ("record.start", "enter"),
            ("record.compiled", "g"),
        ],
        "function g($x) { return $x + 1; }
        function f() { $a = g(1); return $a * 2; }
        g(0); echo f(), \"\\n\";",
    )?;
    let backend = golden_backend().map_err(|e| format!("{e:?}"))?;
    let lowered = |linked| -> Result<String, String> {
        Ok(backend
            .clif(&ir, linked)
            .map_err(|abort| format!("{abort:?}"))?
            .display()
            .to_string())
    };
    let link = (ir.exits.iter())
        .find_map(|exit| exit.link)
        .ok_or("the trace links to nothing")?;
    // The link stops at `g`'s entry, past its `RECV`, where the frame
    // stands: its exit names no opline, and the linked trace opens there.
    let end = (ir.exits.iter())
        .find(|exit| exit.link.is_some())
        .ok_or("no exit links")?;
    assert_eq!(end.opline, None);
    let g = (ir.frames.get(usize::from(end.frame.0)))
        .and_then(|frame| match frame.func.offset() {
            php_unit::MapPtrOffset::Engine(func) => Some(func),
            php_unit::MapPtrOffset::Request(_) => None,
        })
        .ok_or("g's op array")?;
    let body = 0x5000_0000_usize;
    let linked = |interrupt| super::Linked {
        body,
        trace: link.get(),
        interrupt,
        opens: Some(super::Stands { func: g, opline: 1 }),
    };
    let plain = lowered(None)?;
    assert!(!plain.contains("return_call_indirect"), "{plain}");
    let clif = lowered(Some(linked(false)))?;
    let constant = format!("iconst.i64 {body:#x}").replace("0x5000", "0x5000_");
    assert!(
        clif.contains("return_call_indirect") && clif.contains(&constant),
        "the link jumps to the linked body:\n{clif}"
    );
    // A call made in place reads the interrupt too; the link's test is
    // one read more.
    let reads = |clif: &str| {
        let pointer = format!("load.i64 notrap aligned v0+{INTERRUPT_AT}");
        clif.lines().filter(|line| line.contains(&pointer)).count()
    };
    let tested = lowered(Some(linked(true)))?;
    assert_eq!(
        reads(&tested),
        reads(&clif) + 1,
        "a side's link to its looping root tests the interrupt:\n{tested}"
    );
    Ok(())
}
