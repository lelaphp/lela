//! `lower` at its seam (`jit-design.md` §4.3 *Tested*, §7): the CLIF of
//! each `types` fixture's trace, held to a golden file, every loop's
//! carrying the interrupt test but a recursive return's, which carries
//! none; the same function compiled to bytes for
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
    self, ARRAY_WRITE, BINARY_TREES_CHECK, BINARY_TREES_MAKE, DEFAULT_CALL, DIM_AND, DIM_CONCAT,
    DIM_DIV, DIM_MOD, DIM_OR, DIM_SL, DIM_SR, DIM_XOR, ENUM_OWN, FIB, Fixture, HASH_FETCH_W,
    HASH_IS, HASH_ISSET, HASH_READ, HASH_WRITE, INDIRECT_REF_WRITE, MANDELBROT, METHOD_CALL,
    METHOD_OWN, METHOD_SCOPED, NATIVE_CALL, NBODY, NESTED_READ, NESTED_WRITE, NEW_CALL,
    OBJECT_SCOPED, OBJECT_THIS, OBJECT_VALUE, OWNING_LEAVE, PARENT_THIS, REF_CALL, REF_WRITE,
    SIEVE, SPECTRALNORM, STATIC_CALL, STATIC_OWN, STATIC_SCOPED, STATIC_SELF, STATIC_THIS,
    STRING_BUILD, STRING_MATCH, TEMP_CALL, held,
};
use crate::testing::{self, Lane, policy};
use crate::types::{
    Check, Constant, End, Form, Guard, GuardAt, Imm, Interrupt, Op, Operand, Returned, TraceIr,
};

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
    let returned = ir.ops.iter().flat_map(|op| match op {
        Op::Line(line) => match line.imm {
            Imm::Leave {
                value: Returned::Constant(address),
                ..
            } => vec![address],
            // The defaults a call made in place copies.
            Imm::Make { defaults, .. } => defaults.each().map(|(_, at)| at.get()).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    });
    // A constant no scalar is, which an inline form copies from its op
    // array's table.
    let copied = ir.ops.iter().flat_map(|op| match op {
        Op::Line(line) if line.form == Form::Inline => [line.op1, line.op2, line.data]
            .into_iter()
            .filter_map(|operand| match operand {
                Operand::Constant(Constant::Other(idx)) => ir
                    .frames
                    .get(usize::from(line.frame.0))
                    .map(|frame| frame.literals + 16 * idx.0 as usize),
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    });
    let mut seen = Vec::new();
    for address in returned.chain(copied) {
        if !seen.contains(&address) {
            seen.push(address);
        }
    }
    for (at, address) in seen.into_iter().enumerate() {
        // CLIF prints a constant in groups of four hex digits, the first
        // group padded with zeros.
        let digits = format!("{address:x}");
        let digits = format!("{}{digits}", "0".repeat((4 - digits.len() % 4) % 4));
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
        match header.interrupt {
            Interrupt::Exit(exit) => interrupt_test(&clif, i64::from(exit.0))?,
            // CLIF writes an `i32` constant signed.
            Interrupt::Stub(_) => interrupt_test(
                &clif,
                i64::from(i32::from_ne_bytes(INTERRUPTED.to_ne_bytes())),
            )?,
            Interrupt::None => untested(&clif)?,
        }
    }
    let host = Backend::new(&policy(&[]), symbols()).map_err(|e| format!("{e:?}"))?;
    for backend in [&backend, &host] {
        let compiled = backend
            .compile(&ir, named(fixture), None, None)
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

/// That a loop's CLIF tests no interrupt, a loop by recursive return's
/// (`types::Interrupt::None`): no byte read through a pointer loaded from
/// the `ExitCtx`'s interrupt word is branched on, as [`interrupt_test`]
/// finds the test. A call made in place reads the byte for a check of
/// its own, which compares it before it branches.
fn untested(clif: &str) -> Result<(), String> {
    let lines: Vec<&str> = clif.lines().map(instruction).collect();
    let loaded = |from: &dyn Fn(&str) -> bool| -> Vec<String> {
        (lines.iter())
            .filter_map(|line| {
                let (value, load) = line.split_once(" = ")?;
                from(load).then(|| value.to_owned())
            })
            .collect()
    };
    let pointers = loaded(&|load| load == format!("load.i64 notrap aligned v0+{INTERRUPT_AT}"));
    let bytes = loaded(&|load| {
        (pointers.iter()).any(|pointer| load == format!("load.i8 notrap aligned {pointer}"))
    });
    match (lines.iter())
        .find(|line| (bytes.iter()).any(|byte| line.starts_with(&format!("brif {byte}, "))))
    {
        Some(branch) => Err(format!(
            "a loop by recursive return tests the interrupt, `{branch}`:\n{clif}"
        )),
        None => Ok(()),
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

/// Strings compared, measured and cast inline: the comparison of two
/// strings by their pointers, then their lengths, then the `identical`
/// kernel; a length read at `ZSTR_LEN` (#3321).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn strings_lower_inline() -> Result<(), String> {
    lowered(STRING_MATCH)?;
    let clif = std::fs::read_to_string(goldens_dir().join("string_match.clif"))
        .map_err(|e| e.to_string())?;
    let address =
        format!("{:#x}", symbols().address(Kernel::Identical)).replace("0x7000", "0x7000_");
    let compared = (clif.lines())
        .filter(|line| line.contains("call_indirect") && line.contains(&address))
        .count();
    // `$s !== "zz"`'s bytes; `$s === ''` is answered by the lengths.
    assert!(compared >= 1, "{clif}");
    Ok(())
}

/// A blob built in a loop: the path's two concatenations through
/// `concat`, each string piece of the rope through the rope's kernels,
/// the long one through `handle`, and the line appended through
/// `assign_concat` (#3321).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn ropes_and_appends_lower_through_their_kernels() -> Result<(), String> {
    lowered(STRING_BUILD)?;
    let clif = std::fs::read_to_string(goldens_dir().join("string_build.clif"))
        .map_err(|e| e.to_string())?;
    let calls = |kernel: Kernel| {
        let address = format!("{:#x}", symbols().address(kernel)).replace("0x7000", "0x7000_");
        (clif.lines())
            .filter(|line| line.contains("call_indirect") && line.contains(&address))
            .count()
    };
    assert_eq!(calls(Kernel::RopeInit), 1, "{clif}");
    assert_eq!(calls(Kernel::RopeEnd), 1, "{clif}");
    assert_eq!(calls(Kernel::AssignConcat), 1, "{clif}");
    assert_eq!(calls(Kernel::Concat), 2, "{clif}");
    Ok(())
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
/// handler through [`Kernel::Native`], with no dispatch (#2963, #3340).
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
    assert!(!calls_of(&clif, Kernel::Native).is_empty(), "{clif}");
    assert!(calls_of(&clif, Kernel::Handle).is_empty(), "{clif}");
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
        .compile(&ir, named(SPECTRALNORM), None, None)
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

/// A loop over `$this` and one over an object a function holds: the class
/// guard compares the object's class at `Obj::CLASS_AT`, each untyped
/// slot is read and written at `Obj::SLOTS_AT`, and the typed property's
/// write and every slot the inline form does not serve go through the
/// `handle` kernel (#3337).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn properties_lower_in_place_under_their_class_guard() -> Result<(), String> {
    for fixture in [OBJECT_THIS, OBJECT_VALUE, OBJECT_SCOPED] {
        lowered(fixture)?;
        let clif = std::fs::read_to_string(goldens_dir().join(format!("{}.clif", fixture.name)))
            .map_err(|e| e.to_string())?;
        let address =
            format!("{:#x}", symbols().address(Kernel::Handle)).replace("0x7000", "0x7000_");
        let handled = (clif.lines())
            .filter(|line| line.contains("call_indirect") && line.contains(&address))
            .count();
        assert!(handled >= 1, "{}: {clif}", fixture.name);
    }
    Ok(())
}

/// A trace compiled over objects of a published class is the engine's,
/// entered by every request: on the next request an object of a class
/// that request declared, whose slots stand elsewhere, fails its class
/// guard and the interpreter serves it, while the published class's
/// objects go on in place (#3337).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_class_the_next_request_declares_fails_the_class_guard() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Lane>(&[
        ("jit.mode", "force"),
        ("jit.compile", "inline"),
        ("opcache.jit_debug", "258048"),
    ]);
    let program = "class P { public $x = 1; public $y = 2; }
        function k($o, $m) { $t = 0; for ($i = 0; $i < $m; $i++) { $t += $o->x; $o->y = $i; $o->x += 1; } return $t + $o->y; }
        if (defined('SECOND')) {
            eval('class Q { public $y = 0; public $x = 5; }');
            echo k(new Q, 10), ' ', k(new P, 10), \"\\n\";
        } else {
            echo k(new P, 10), \"\\n\";
        }";
    let first = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&first), "64\n");
    let second = engine.serve_capture_in(
        &scratch.0,
        b"main.php",
        "<?php const SECOND = true; include 'body.php';",
        php_engine::test::Capture::default(),
    );
    assert_eq!(testing::stdout(&second), "104 64\n");
    let lines = String::from_utf8_lossy(&second.stderr);
    assert!(lines.contains("TRACE 1 exit"), "{lines}");
}

/// A trace over a request-scoped class's properties, entered on a request
/// that has not yet called the function whose property oplines it inlines:
/// the touch finds that function's cache not laid down, as php lays it at
/// the call's `INIT_*` and no sooner, so the read leaves for the
/// interpreter, the call lays the cache, and the next entry touches the
/// slot and reads its place in place on every turn after (#3421, #3346,
/// #2957's note).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_trace_entered_before_its_callee_ran_reads_the_slot_its_call_laid() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Lane>(&[
        ("jit.mode", "force"),
        ("jit.compile", "inline"),
        ("opcache.jit_debug", "258048"),
    ]);
    let program = "eval('class Base { public $x = 1; }');
        class C extends Base { }
        function g($o) { return $o->x; }
        function k($o, $m) { $t = 0; for ($i = 0; $i < $m; $i++) { if ($i > 0) { $t += g($o); } $o->x = $i; } return $t; }
        echo k(new C, 20), \"\\n\";";
    let first = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&first), "171\n");
    let second = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&second), "171\n");
    let lines = String::from_utf8_lossy(&second.stderr);
    let exits = lines.lines().filter(|line| line.contains(" exit ")).count();
    assert!(exits <= 2, "{lines}");
}

/// A trace over a request-scoped class's properties serves an object of
/// another class through its handler where the request's cache slot names
/// the class it placed the name on last, and in place once the slot names
/// the object's own: the write is the handler's, and the read leaves for
/// the interpreter, as php's code goes to its slow path where
/// `CACHED_PTR(cache_slot)` is not `obj->ce` (#3346).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_cache_slot_that_names_another_class_sends_the_opline_to_its_handler() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Lane>(&[
        ("jit.mode", "force"),
        ("jit.compile", "inline"),
        ("opcache.jit_debug", "258048"),
    ]);
    let program = "eval('class A { public $x = 1; public $y = 2; } class B { public $y = 7; public $x = 5; }');
        function k($o, $p, $m) { $t = 0; for ($i = 0; $i < $m; $i++) { $t += $o->x; $o->y = $i; $o = $i % 2 ? $p : $o; } return $t; }
        echo k(new A, new B, 10), ' ', k(new B, new B, 10), \"\\n\";";
    let served = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&served), "42 50\n");
}

/// A slot of `true` alone, met through a request-scoped class's cache
/// slot after an untyped one, takes `true` and sends `false` to the
/// handler's `TypeError`, at the fourth pass as php's interpreter raises it
/// (#3346). php 8.5.12's own compiled tier raises it on the first pass
/// here under the oracle's hot counters, so the interpreter's answer is
/// held, not the oracle's, and the program is no witness.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_slot_of_true_alone_refuses_false_to_its_handler() {
    let scratch = testing::Scratch::new();
    for mode in ["force", "exit-fuzz"] {
        let engine = testing::engine::<Lane>(&[("jit.mode", mode), ("jit.compile", "inline")]);
        let program = "eval('class A { public $t = 0; } class B { public true $t = true; }');
            function f($o, $m) { for ($i = 0; $i < $m; $i++) { try { $o->t = $i < 3; } catch (TypeError $e) { echo $i, ' ', $e->getMessage(), \"\\n\"; return; } } echo 'done ', $o->t ? 'true' : 'false', \"\\n\"; }
            f(new A, 10); f(new B, 10); f(new A, 2);";
        let served = scratch.serve(&engine, program);
        assert_eq!(
            testing::stdout(&served),
            "done false\n3 Cannot assign false to property B::$t of type true\ndone true\n",
            "{mode}"
        );
    }
}

/// The hash slice's dimension oplines lowered, each element reached
/// through the kernel php's code calls the table's lookup through: the
/// `find` kernel for a read, a test and a compound write, the `lookup`
/// kernel for a write and a fetch for writing, and `.=` onto the element
/// through `assign_concat` (#3339).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_hashs_dimension_oplines_lower_through_the_lookup_kernels() -> Result<(), String> {
    let kernels: [(Fixture, &[(Kernel, usize)]); 13] = [
        (HASH_READ, &[(Kernel::Find, 3)]),
        (HASH_IS, &[(Kernel::Find, 3)]),
        (HASH_ISSET, &[(Kernel::Find, 3)]),
        (HASH_WRITE, &[(Kernel::Lookup, 3)]),
        (HASH_FETCH_W, &[(Kernel::Lookup, 2), (Kernel::Find, 1)]),
        (DIM_DIV, &[(Kernel::Find, 1)]),
        (DIM_MOD, &[(Kernel::Find, 2)]),
        (DIM_SL, &[(Kernel::Find, 2)]),
        (DIM_SR, &[(Kernel::Find, 2)]),
        (DIM_AND, &[(Kernel::Find, 1)]),
        (DIM_OR, &[(Kernel::Find, 1)]),
        (DIM_XOR, &[(Kernel::Find, 1)]),
        (DIM_CONCAT, &[(Kernel::Find, 1), (Kernel::AssignConcat, 1)]),
    ];
    for (fixture, calls) in kernels {
        lowered(fixture)?;
        let clif = std::fs::read_to_string(goldens_dir().join(format!("{}.clif", fixture.name)))
            .map_err(|e| e.to_string())?;
        for (kernel, n) in calls {
            assert_eq!(
                calls_of(&clif, *kernel).len(),
                *n,
                "{}: {kernel:?}\n{clif}",
                fixture.name
            );
        }
    }
    Ok(())
}

/// The shapes the hash slice left to the handler lowered in place
/// (#3370): `isset()`, `??` and a read over a fetch's temporary through
/// the `find` kernel, the temporary freed after; a write, a compound write
/// and a fetch for writing through the `VAR` a fetch for writing left
/// indirect, and a write to the array a reference holds, through the
/// lookup kernels, the handler called only where a write is not made in
/// place and nothing stepped but the `COALESCE`, as php calls its handler.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_dimension_over_a_temporary_an_indirect_var_or_a_reference_lowers_in_place()
-> Result<(), String> {
    let kernels: [(Fixture, &[(Kernel, usize)]); 4] = [
        (
            NESTED_READ,
            &[
                (Kernel::Find, 5),
                (Kernel::Release, 3),
                (Kernel::Step, 1),
                (Kernel::Handle, 0),
            ],
        ),
        (
            NESTED_WRITE,
            &[
                (Kernel::Lookup, 5),
                (Kernel::Find, 2),
                (Kernel::Handle, 7),
                (Kernel::Step, 0),
            ],
        ),
        (
            REF_WRITE,
            &[(Kernel::Lookup, 2), (Kernel::Handle, 2), (Kernel::Step, 0)],
        ),
        // #3410: through a `VAR` that points at a reference, as through
        // one that points at the array.
        (INDIRECT_REF_WRITE, &[(Kernel::Step, 0)]),
    ];
    for (fixture, calls) in kernels {
        lowered(fixture)?;
        let clif = std::fs::read_to_string(goldens_dir().join(format!("{}.clif", fixture.name)))
            .map_err(|e| e.to_string())?;
        for (kernel, n) in calls {
            assert_eq!(
                calls_of(&clif, *kernel).len(),
                *n,
                "{}: {kernel:?}\n{clif}",
                fixture.name
            );
        }
    }
    Ok(())
}

/// A call that sends arrays by reference is made in place: its
/// `SEND_REF`s copy the references with an owner more, the reference
/// made in place where the variable holds none, its handler cold where
/// the memory limit refuses it (#3560), and the call's two checks
/// read each reference's value before the activation, the call kernel
/// cold where one fails (#3340).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_by_reference_lowers_its_sends_and_checks() -> Result<(), String> {
    lowered(REF_CALL)?;
    let clif =
        std::fs::read_to_string(goldens_dir().join("ref_call.clif")).map_err(|e| e.to_string())?;
    let calls = |kernel: Kernel| {
        let address = format!("{:#x}", symbols().address(kernel)).replace("0x7000", "0x7000_");
        (clif.lines())
            .filter(|line| line.contains("call_indirect") && line.contains(&address))
            .count()
    };
    // Each `SEND_REF`'s reference made in place, its refused arm, and
    // each `ASSIGN_DIM_OP`'s of the callee.
    assert_eq!(calls(Kernel::MakeRef), 2, "{clif}");
    assert_eq!(calls(Kernel::Handle), 4, "{clif}");
    // The made call's cold arm, where the interrupt is raised, the cache
    // not laid down or a check fails.
    assert_eq!(calls(Kernel::Call), 1, "{clif}");
    // The push's and the leave's cold arms, and no send stepped.
    assert_eq!(calls(Kernel::Step), 2, "{clif}");
    Ok(())
}

/// A method's leave in place gives its receiver up after its variables:
/// one owner fewer in place, and [`Kernel::FreeThis`] cold where that is
/// the last or the collector may take the object as a root (#3343).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_methods_leave_gives_its_receiver_up() -> Result<(), String> {
    lowered(METHOD_CALL)?;
    let clif = std::fs::read_to_string(goldens_dir().join("method_call.clif"))
        .map_err(|e| e.to_string())?;
    let address =
        format!("{:#x}", symbols().address(Kernel::FreeThis)).replace("0x7000", "0x7000_");
    let freed = (clif.lines())
        .filter(|line| line.contains("call_indirect") && line.contains(&address))
        .count();
    assert_eq!(freed, 1, "{clif}");
    Ok(())
}

/// The calls `k`, `s`, `n` and `t` make: `k`'s and `t`'s methods pushed in place with
/// their receivers, and `s`'s and `n`'s held to their callee after the
/// handler pushed them, each made in place with no send stepped, the
/// call kernel cold (#3343).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn method_calls_lower_their_push_call_and_leave() -> Result<(), String> {
    for fixture in [METHOD_CALL, STATIC_CALL, NEW_CALL, TEMP_CALL] {
        lowered(fixture)?;
        let clif = std::fs::read_to_string(goldens_dir().join(format!("{}.clif", fixture.name)))
            .map_err(|e| e.to_string())?;
        let calls = |kernel: Kernel| {
            let address = format!("{:#x}", symbols().address(kernel)).replace("0x7000", "0x7000_");
            (clif.lines())
                .filter(|line| line.contains("call_indirect") && line.contains(&address))
                .count()
        };
        assert_eq!(calls(Kernel::Call), 1, "{}: {clif}", fixture.name);
        assert_eq!(calls(Kernel::FreeThis), 1, "{}: {clif}", fixture.name);
    }
    Ok(())
}

/// The calls pushed in place under the pair their runtime-cache slot
/// holds lower to the guard on the slot, read past the trace's headers,
/// and one cold arm where the handler pushes the call, which no other
/// kernel serves (#3557, #3558).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_pushed_under_its_slots_pair_lowers_one_handler_arm() -> Result<(), String> {
    for fixture in [
        STATIC_CALL,
        METHOD_SCOPED,
        STATIC_SCOPED,
        STATIC_SELF,
        STATIC_THIS,
        PARENT_THIS,
        METHOD_OWN,
        STATIC_OWN,
        ENUM_OWN,
    ] {
        let ir = lowered(fixture)?;
        let clif = std::fs::read_to_string(goldens_dir().join(format!("{}.clif", fixture.name)))
            .map_err(|e| e.to_string())?;
        let at = (ir.ops.iter())
            .position(|op| matches!(op, Op::Line(line) if matches!(line.imm, Imm::Pair { .. })))
            .ok_or_else(|| format!("{}: no push under a pair", fixture.name))?;
        let address =
            format!("{:#x}", symbols().address(Kernel::Step)).replace("0x7000", "0x7000_");
        let stepped = (clif.lines())
            .filter(|line| line.starts_with(&format!("@{at:04x} ")))
            .filter(|line| line.contains("call_indirect") && line.contains(&address))
            .count();
        assert_eq!(stepped, 1, "{}: {clif}", fixture.name);
        assert_eq!(
            calls_of(&clif, Kernel::Call).len(),
            1,
            "{}: {clif}",
            fixture.name
        );
    }
    Ok(())
}

/// A trace that pushes a static method's call in place, entered on a
/// request whose serving of the op array has not primed the call's slot: the
/// slot holds no pair, so the handler pushes the call, looking the class
/// and the method up as php's `zend_jit_find_static_method_helper` does,
/// the callee is held after it, and every pass after, the slot primed,
/// pushes it in place; the call's `static` is the class the request
/// resolved the name to (#3557).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_empty_slot_sends_a_static_call_to_its_handler() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Lane>(&[
        ("jit.mode", "force"),
        ("jit.compile", "inline"),
        ("opcache.jit_debug", "258048"),
    ]);
    let program = "class Getter { public static function twice($v) { return static::class === 'Child' ? $v + $v : -1; } }
        eval('class Base extends Getter { }');
        class Child extends Base { }
        class Pub { public static function three($v) { return 3 * $v; } }
        function k($m) { $t = 0; for ($i = 0; $i < $m; $i++) { if ($i > 0) { $t += Child::twice($i) + Pub::three($i); } } return $t; }
        echo k(20), \"\\n\";";
    let first = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&first), "950\n");
    let second = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&second), "950\n");
    let lines = String::from_utf8_lossy(&second.stderr);
    // The second request links into the loop trace the first compiled,
    // TRACE 2, whose first push finds its slot empty, and the cold arm's
    // handler serves it: the loop never leaves through an exit of its own.
    assert!(lines.contains("(link to 2)"), "{lines}");
    assert!(!lines.contains("TRACE 2 exit"), "{lines}");
    let exits = lines.lines().filter(|line| line.contains(" exit ")).count();
    assert!(exits <= 2, "{lines}");
}

/// A trace that pushes a request-scoped class's inherited method in place
/// under its slot's pair serves an object of another request-scoped class
/// through the handler where the slot names the class it resolved the
/// name on last, and in place once it names the object's own; the callee,
/// the one method both inherit, holds, and each call's `static` is its
/// receiver's class (#3558).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_slot_that_names_another_class_sends_a_methods_call_to_its_handler() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Lane>(&[
        ("jit.mode", "force"),
        ("jit.compile", "inline"),
        ("opcache.jit_debug", "258048"),
    ]);
    let program = "class Getter { public $b = 1; public function get($v) { return static::class === 'A' ? $this->b + $v : $v; } }
        eval('class A extends Getter { } class B extends Getter { public $b = 9; }');
        function k($o, $p, $m) { $t = 0; for ($i = 0; $i < $m; $i++) { $t += $o->get($i); $o = $i % 2 ? $p : $o; } return $t; }
        echo k(new A, new B, 10), ' ', k(new B, new A, 10), \"\\n\";";
    let served = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&served), "47 53\n");
}

/// A call passed fewer arguments than its callee has parameters lowers
/// to its activation in place, each default copied from the op array's
/// constants, the call kernel cold where the call is refused (#3340).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_passed_fewer_arguments_lowers_its_defaults() -> Result<(), String> {
    lowered(DEFAULT_CALL)?;
    let clif = std::fs::read_to_string(goldens_dir().join("default_call.clif"))
        .map_err(|e| e.to_string())?;
    let calls = |kernel: Kernel| {
        let address = format!("{:#x}", symbols().address(kernel)).replace("0x7000", "0x7000_");
        (clif.lines())
            .filter(|line| line.contains("call_indirect") && line.contains(&address))
            .count()
    };
    assert_eq!(calls(Kernel::Call), 1, "{clif}");
    assert_eq!(calls(Kernel::Handle), 0, "{clif}");
    Ok(())
}
