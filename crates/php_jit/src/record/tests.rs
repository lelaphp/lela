//! `record` at its seam (`jit-design.md` §4.1 *Tested*, §7): the
//! recorder driven on real requests by a tier that records from a chosen
//! safepoint, one fixture per `Stepped` arm and one per stop of php's
//! list it takes, each held to the record it wrote and to `dump` then
//! `parse`; and the text form as a property over generated records.

use php_engine::tier::{self, FuncRef, Sample};
use php_heap::ZvalType;
use php_unit::{MapPtrOffset, Opcode, OplineIdx};

use super::{Event, Start, Stop, TraceRecord};
use crate::table::TraceId;
use crate::testing;
use crate::testing::records::{at, kept, recorded, trace};
use crate::types::ExitId;

/// The record's events by name, an `Op` by its opline.
fn events(record: &TraceRecord) -> Vec<String> {
    record
        .events
        .iter()
        .map(|event| match event {
            Event::Op { opline, .. } => format!("op{}", opline.0),
            Event::InitCall { .. } => "init".to_owned(),
            Event::Enter { .. } => "enter".to_owned(),
            Event::Back => "back".to_owned(),
        })
        .collect()
}

fn enters(record: &TraceRecord) -> usize {
    record
        .events
        .iter()
        .filter(|event| matches!(event, Event::Enter { .. }))
        .count()
}

fn side(parent: u32, exit: u16) -> Start {
    Start::Side {
        parent: trace(parent),
        exit: ExitId(exit),
    }
}

/// A call and its return: `INIT_FCALL` writes the call down, `Pushed`
/// writes `Enter`, `Popped` back to the root writes `Back`, and the
/// root's own return at depth zero ends an entry trace with `return`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_and_its_return_are_entered_and_left() -> Result<(), String> {
    let (record, out) = recorded(
        &at("f", "enter"),
        "function g($x) { return $x + 1; }
        function f() { $a = g(1); return $a * 2; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!(out, "4\n");
    assert_eq!((record.start, record.stop), (Start::Enter, Stop::Return));
    assert_eq!(enters(&record), 1);
    let events = events(&record);
    let at = |name: &str| events.iter().position(|e| e == name);
    assert!(
        at("init") < at("enter") && at("enter") < at("back"),
        "{events:?}"
    );
    Ok(())
}

/// A return trace roots at its caller's landing (`jit-design.md` §2.2,
/// #3036): its first opline is the one past `f`'s call to `g`, in `f`,
/// and it goes on there to `f`'s own return, which, with no recursion
/// below, ends it as php's "Return from function" does.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_trace_roots_at_its_callers_landing_and_goes_on_there() -> Result<(), String> {
    let (record, out) = recorded(
        &at("g", "return"),
        "function g($x) { return $x + 1; }
        function f() { $a = g(1); return $a * 2; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!(out, "4\n");
    assert_eq!((record.start, record.stop), (Start::Return, Stop::Return));
    assert_eq!(enters(&record), 0);
    let Some(Event::Op {
        func: first,
        opline,
        ..
    }) = record.events.first()
    else {
        return Err(format!("no first opline: {:?}", record.events));
    };
    assert_eq!(*first, record.root, "the landing is f's");
    assert!(opline.0 > 0, "past the call: {opline:?}");
    let all_in_f = (record.events.iter())
        .all(|event| matches!(event, Event::Op { func, .. } if *func == record.root));
    assert!(all_in_f, "the trace stays in f: {:?}", record.events);
    Ok(())
}

/// A call to a generator function: its frame is activated and then moved
/// to the heap, where the recorder stops.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_yield_that_creates_a_generator_stops_at_the_moved_frame() -> Result<(), String> {
    let (record, out) = recorded(
        &at("f", "enter"),
        "function gen() { yield 1; yield 2; }
        function f() { $g = gen(); foreach ($g as $v) { echo $v; } }
        f(); echo \"\\n\";",
    )?;
    assert_eq!(out, "12\n");
    assert_eq!(record.stop, Stop::MovedFrame);
    assert_eq!(enters(&record), 1);
    Ok(())
}

/// A generator function a native calls, as `yield from` calls an
/// `IteratorAggregate`'s `getIterator`: its frame is the entry frame of
/// the native's `execute`, and `GENERATOR_CREATE` leaves it, so the
/// entry-frame rule stops the trace with `return` before the step that
/// would pop the entry, as php's trace stops with `RETURN` where the
/// handler of a top frame halts (`zend_jit_vm_helpers.c:1080-1087`;
/// `Zend/tests/generators/yield_from_iterator_agregate.phpt`,
/// `Zend/tests/gh19326.phpt`, `Zend/tests/fibers/gh15108-005.phpt`).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_generator_a_native_creates_stops_before_it_leaves_the_entry_frame() -> Result<(), String> {
    let (record, out) = recorded(
        &at("getIterator", "enter"),
        "class It implements IteratorAggregate { function getIterator(): Generator { echo 'in '; yield 1; } }
        function f() { yield from new It(); }
        foreach (f() as $v) { echo $v; } echo \"\\n\";",
    )?;
    assert_eq!(out, "in 1\n");
    assert_eq!(record.stop, Stop::Return);
    assert_eq!(events(&record), ["op0"]);
    Ok(())
}

/// A throw: the step leaves a throwable pending and the recorder stops
/// with `exception`, the program going on to catch it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_throw_stops_with_exception() -> Result<(), String> {
    let (record, out) = recorded(
        &at("f", "enter"),
        "function f() { $a = 1; throw new Exception(\"x$a\"); }
        try { f(); } catch (Exception $e) { echo $e->getMessage(), \"\\n\"; }",
    )?;
    assert_eq!(out, "x1\n");
    assert_eq!(record.stop, Stop::Exception);
    Ok(())
}

/// `do { } while (!$x)`: `BOOL_NOT` then a backward `JMPNZ` to the
/// header, which closes the loop at the root.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_do_while_not_loop_closes_at_its_header() -> Result<(), String> {
    let (record, out) = recorded(
        &at("f", "loop"),
        "function f() { $i = 0; do { $i++; $done = $i > 3; } while (!$done); return $i; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!(out, "4\n");
    assert_eq!((record.start, record.stop), (Start::Loop, Stop::Loop));
    let oplines: Vec<u32> = record
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Op { opline, .. } => Some(opline.0),
            _ => None,
        })
        .collect();
    assert!(
        oplines.windows(2).all(|pair| pair[0] < pair[1]),
        "one pass down the body to the back-edge: {oplines:?}"
    );
    Ok(())
}

/// A `for` loop's trace roots at its condition, the loop's header php's
/// CFG finds, where the back edge lands on the body: `JMP` to the
/// condition at 3, the body at 4 and 5, `IS_SMALLER` at 6 fused to the
/// `JMPNZ` at 7, one `Op` (design §5.8), so the record is the condition,
/// then the body, as the pinned php's `TRACE_BYTECODE` dump lists
/// `IS_SMALLER`, `JMPNZ`, then the body (#3134).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_for_loop_roots_at_its_condition_as_php_does() -> Result<(), String> {
    let (recorded, out) = testing::records::recording(
        &at("a", "loop"),
        "function a($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        echo a(5), \"\\n\";",
    )?;
    assert_eq!(out, "10\n");
    let record = &recorded.record;
    assert_eq!((record.start, record.stop), (Start::Loop, Stop::Loop));
    let code = recorded
        .view
        .op_array(record.root)
        .ok_or("the view holds a()")?;
    let ops: Vec<(u32, Opcode, Option<bool>)> = record
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Op { opline, branch, .. } => {
                Some((opline.0, code.oplines[opline.0 as usize].opcode, *branch))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        ops,
        [
            (6, Opcode::IsSmaller, Some(true)),
            (4, Opcode::AssignOp, None),
            (5, Opcode::PreInc, None),
        ]
    );
    assert_eq!(code.oplines[7].opcode, Opcode::Jmpnz);
    // The issue's own loop, in the file's code: the condition first.
    let plan = [
        ("record.function", ""),
        ("record.start", "loop"),
        ("record.headers", "0"),
    ];
    let (recorded, _) = testing::records::recording(
        &plan,
        "for ($i = 0; $i < 100; $i++) { $x = 5; } echo $x, \"\\n\";",
    )?;
    let record = &recorded.record;
    let code = recorded
        .view
        .op_array(record.root)
        .ok_or("the view holds the file's code")?;
    let opcodes: Vec<Opcode> = (record.events.iter())
        .filter_map(|event| match event {
            Event::Op { opline, .. } => Some(code.oplines[opline.0 as usize].opcode),
            _ => None,
        })
        .collect();
    assert_eq!(
        (record.stop, opcodes.as_slice()),
        (
            Stop::Loop,
            [Opcode::IsSmaller, Opcode::Assign, Opcode::PreInc].as_slice()
        )
    );
    Ok(())
}

/// A fused compare-and-branch is one `Op` with its direction on it: the
/// `for` condition jumping back to the header, and an `if` falling
/// through and jumping over.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fused_branch_is_one_op_taken_each_way() -> Result<(), String> {
    let (record, _) = recorded(
        &at("f", "loop"),
        "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        echo f(5), \"\\n\";",
    )?;
    assert_eq!(record.stop, Stop::Loop);
    let branches: Vec<bool> = record
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Op { branch, .. } => *branch,
            _ => None,
        })
        .collect();
    assert_eq!(branches, [true], "the condition jumped back to the header");
    for (a, taken) in [("1", Some(false)), ("5", Some(true))] {
        let program = format!(
            "function f($a) {{ $b = 3; if ($a < $b) {{ echo \"less\"; }} return $a; }}
            echo f({a}), \"\\n\";"
        );
        let (record, _) = recorded(&at("f", "enter"), &program)?;
        let branch = record.events.iter().find_map(|event| match event {
            Event::Op { branch, .. } => *branch,
            _ => None,
        });
        assert_eq!(branch, taken, "f({a})");
    }
    Ok(())
}

/// A conditional jump of its own, on a value no comparison wrote, says
/// which way it went too, so a trace that ends on one says where it ends.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_conditional_jump_says_which_way_it_went() -> Result<(), String> {
    for (a, taken) in [("true", Some(false)), ("false", Some(true))] {
        let program = format!(
            "function f($a) {{ if ($a) {{ echo \"yes\"; }} return 1; }}
            echo f({a}), \"\\n\";"
        );
        let (record, _) = recorded(&at("f", "enter"), &program)?;
        let branch = record.events.iter().find_map(|event| match event {
            Event::Op { branch, .. } => *branch,
            _ => None,
        });
        assert_eq!(
            branch, taken,
            "f({a}): JMPZ jumps over the echo when $a is false"
        );
    }
    Ok(())
}

/// A call into a function an `eval` declared: its op array is the
/// request's, which no trace may read, and the recorder stops at it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_into_an_evaled_function_stops_at_the_request_unit() -> Result<(), String> {
    let (record, out) = recorded(
        &at("f", "enter"),
        "eval('function h() { return 7; }');
        function f() { return h() + 1; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!(out, "8\n");
    assert_eq!(record.stop, Stop::RequestUnit);
    assert_eq!(enters(&record), 0);
    Ok(())
}

/// php's stops before a step: `FAST_CALL` is not supported,
/// `INCLUDE_OR_EVAL` exits to the interpreter, and a recording no longer
/// than php's least limit is too long at its first opline.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn the_stops_before_a_step() -> Result<(), String> {
    let (record, out) = recorded(
        &at("f", "enter"),
        "function f() { try { $a = 1; } finally { echo \"fin\"; } return $a; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!((record.stop, out.as_str()), (Stop::NotSupported, "fin1\n"));
    let (record, _) = recorded(
        &at("f", "enter"),
        "function f() { $a = 1; eval('echo 2;'); return $a; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!(record.stop, Stop::Interpreter);
    let mut plan = at("f", "enter");
    plan.push(("opcache.jit_max_trace_length", "4"));
    let (record, _) = recorded(
        &plan,
        "function f() { $a = 1; return $a; } echo f(), \"\\n\";",
    )?;
    assert_eq!(record.stop, Stop::TooLong);
    Ok(())
}

/// php's stops at a header and at a call: an entry trace reaching a loop
/// stops at the inner loop, a loop trace leaving its function exits the
/// loop, an outer loop's trace stops at its inner loop, a callee whose
/// entry is compiled is linked to, a blacklisted one stops the trace, and
/// an outer loop's trace reaching a compiled inner loop stops there.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn the_stops_at_a_header_and_a_call() -> Result<(), String> {
    let (record, _) = recorded(
        &at("f", "enter"),
        "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        echo f(3), \"\\n\";",
    )?;
    assert_eq!(record.stop, Stop::InnerLoop);
    let (record, _) = recorded(
        &at("f", "loop"),
        "function f($n) { for ($i = 0; $i < $n; $i++) { if ($i == 0) { return 9; } } return 0; }
        echo f(3), \"\\n\";",
    )?;
    assert_eq!(record.stop, Stop::LoopExit);
    let (record, _) = recorded(
        &at("f", "loop"),
        "function f() { $s = 0; for ($i = 0; $i < 2; $i++) { for ($j = 0; $j < 2; $j++) { $s++; } } return $s; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!(record.stop, Stop::InnerLoop, "the inner header is cold");
    let calls =
        "function g() { return 1; } function f() { return g() + 1; } g(); echo f(), \"\\n\";";
    let mut plan = at("f", "enter");
    plan.push(("record.compiled", "g"));
    let (record, out) = recorded(&plan, calls)?;
    assert_eq!((record.stop, out.as_str()), (Stop::Link(trace(7)), "2\n"));
    let mut plan = at("f", "enter");
    plan.push(("record.blacklisted", "g"));
    let (record, _) = recorded(&plan, calls)?;
    assert_eq!(record.stop, Stop::BlackList);
    let mut plan = at("f", "loop");
    plan.push(("record.compiled_inner", "f"));
    let (record, out) = recorded(
        &plan,
        "function f() { $s = 0; for ($i = 0; $i < 2; $i++) { for ($j = 0; $j < 2; $j++) { $s++; } } return $s; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!(
        (record.stop, out.as_str()),
        (Stop::CompiledLoop, "4\n"),
        "an outer loop's trace reaching a compiled inner loop"
    );
    Ok(())
}

/// php's megamorphic call under `opcache.jit_max_polymorphic_calls=0`: a
/// method call is one the interpreter makes (`zend_jit_may_be_polymorphic_call`).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_megamorphic_call_stops_for_the_interpreter() -> Result<(), String> {
    let program = "class C { function m() { return 1; } }
        function f($o) { return $o->m() + 1; } echo f(new C), \"\\n\";";
    let mut plan = at("f", "enter");
    plan.push(("opcache.jit_max_polymorphic_calls", "0"));
    let (record, out) = recorded(&plan, program)?;
    assert_eq!((record.stop, out.as_str()), (Stop::Interpreter, "2\n"));
    let (record, _) = recorded(&at("f", "enter"), program)?;
    assert_ne!(
        record.stop,
        Stop::Interpreter,
        "two polymorphic calls are php's default"
    );
    Ok(())
}

/// Recursion: a call back into the root's entry is followed once and the
/// second stops as a recursive call, php's
/// `opcache.jit_max_recursive_calls` of two.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn recursion_into_the_root_stops_as_a_recursive_call() -> Result<(), String> {
    let (record, out) = recorded(
        &at("fib", "enter"),
        "function fib($n) { return $n < 2 ? $n : fib($n - 1) + fib($n - 2); }
        echo fib(6), \"\\n\";",
    )?;
    assert_eq!(out, "8\n");
    assert_eq!(record.stop, Stop::RecursiveCall);
    Ok(())
}

/// A fatal a step raised ends the recording with no record, and the
/// request records no more: the shutdown function the fatal leaves to
/// serve is asked with the exclusion clear.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fatal_from_a_step_ends_the_recording_with_no_record() {
    let mut ini = at("f", "enter");
    ini.push(("memory_limit", "2M"));
    let (kept, served) = kept(
        &ini,
        "function f() { $s = 'xxxxxxxx'; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; $s .= $s; return 1; }
        function down() { echo 'shut'; }
        register_shutdown_function('down');
        echo f();",
    );
    assert!(
        kept.taken.is_empty(),
        "nothing was recorded: {:?}",
        kept.taken.first().map(|taken| taken.record.dump())
    );
    let fatal = served.fatal.clone().unwrap_or_default();
    assert!(fatal.contains("Allowed memory size"), "{fatal}");
    assert!(
        testing::stdout(&served).ends_with("shut"),
        "{}",
        testing::stdout(&served)
    );
    assert_eq!(kept.asked.last(), Some(&false), "{:?}", kept.asked);
}

/// While a recording runs, the request records, so a nested `execute` is
/// asked with the exclusion set, and it is clear again after.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn the_exclusion_is_set_for_the_recording_alone() {
    let (kept, served) = kept(
        &at("f", "enter"),
        "class D { function __destruct() { $s = 0; for ($i = 0; $i < 3; $i++) { $s += $i; } echo $s; } }
        function f() { $d = new D; $d = null; return 1; }
        echo f(), \"\\n\";",
    );
    assert_eq!(testing::stdout(&served), "31\n");
    assert_eq!(kept.taken.len(), 1);
    assert!(
        kept.asked.contains(&true),
        "the destructor's loop was asked while recording: {:?}",
        kept.asked
    );
    assert_eq!(kept.asked.last(), Some(&false), "{:?}", kept.asked);
}

/// Every stop by its code and back, a link coding as none, and php's
/// words for each.
#[test]
fn every_stop_is_named_in_phps_words_and_coded_once() {
    let link = Stop::Link(trace(2));
    assert_eq!(link.code(), 0);
    assert_eq!(link.describe(), "link to another trace");
    let mut words = Vec::new();
    for code in 1..=u8::MAX {
        let Some(stop) = Stop::of_code(code) else {
            break;
        };
        assert_eq!(stop.code(), code);
        words.push(stop.describe());
    }
    assert_eq!(words.len(), 26);
    assert_eq!(words[0], "loop");
    assert_eq!(words[17], "trace blacklisted");
    assert_eq!(words[20], "JIT compilation error");
    assert_eq!(words[25], "too many side exits");
    let starts = [Start::Loop, Start::Enter, Start::Return, side(1, 0)];
    let words: Vec<&str> = starts.into_iter().map(Start::describe).collect();
    assert_eq!(words, ["loop", "enter", "return", "side"]);
    assert!(Stop::Loop.compiles() && link.compiles() && !Stop::InnerLoop.compiles());
    assert!(Stop::CompilerError.recovers() && !Stop::TooManyTraces.recovers());
}

/// A generator of records, a linear congruential one so a failure names
/// its seed.
struct Draw(u64);

impl Draw {
    fn next(&mut self, below: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        u32::try_from((self.0 >> 33) % u64::from(below.max(1))).unwrap_or(0)
    }

    fn func(&mut self) -> FuncRef {
        let n = self.next(1000);
        FuncRef::at(if self.next(4) == 0 {
            MapPtrOffset::Request(n)
        } else {
            MapPtrOffset::Engine(n)
        })
    }

    fn sample(&mut self) -> Option<Sample> {
        const KINDS: [ZvalType; 14] = [
            ZvalType::Undef,
            ZvalType::Null,
            ZvalType::False,
            ZvalType::True,
            ZvalType::Long,
            ZvalType::Double,
            ZvalType::String,
            ZvalType::Array,
            ZvalType::Object,
            ZvalType::Resource,
            ZvalType::Reference,
            ZvalType::ConstExpr,
            ZvalType::Indirect,
            ZvalType::Ptr,
        ];
        if self.next(3) == 0 {
            return None;
        }
        let kind = KINDS
            .get(self.next(14) as usize)
            .copied()
            .unwrap_or_default();
        Some(Sample {
            kind,
            indirect: self.next(2) == 0,
            reference: self.next(2) == 0,
            packed: self.next(2) == 0,
            class: (self.next(2) == 0).then(|| tier::numbered_class(self.next(u32::MAX))),
        })
    }

    fn event(&mut self) -> Event {
        match self.next(4) {
            0 => Event::Op {
                func: self.func(),
                opline: OplineIdx(self.next(5000)),
                op1: self.sample(),
                op2: self.sample(),
                data: self.sample(),
                branch: [None, Some(true), Some(false)]
                    .get(self.next(3) as usize)
                    .copied()
                    .flatten(),
            },
            1 => Event::InitCall {
                target: (self.next(2) == 0).then(|| self.func()),
            },
            2 => Event::Enter { func: self.func() },
            _ => Event::Back,
        }
    }

    fn record(&mut self) -> TraceRecord {
        let start = match self.next(4) {
            0 => Start::Loop,
            1 => Start::Enter,
            2 => Start::Return,
            _ => {
                let exit = u16::try_from(self.next(400)).unwrap_or(0);
                side(1 + self.next(TraceId::LIMIT - 1), exit)
            }
        };
        let code = u8::try_from(1 + self.next(27)).unwrap_or(1);
        let link = Stop::Link(trace(1 + self.next(TraceId::LIMIT - 1)));
        let stop = Stop::of_code(code).unwrap_or(link);
        let events = (0..self.next(40)).map(|_| self.event()).collect();
        let entered = match start {
            Start::Side { .. } => (0..self.next(4))
                .map(|var| super::Entered {
                    var: var * 3,
                    tag: if self.next(2) == 0 {
                        ZvalType::Long
                    } else {
                        ZvalType::Double
                    },
                })
                .collect(),
            _ => Vec::new(),
        };
        let landed = match start {
            Start::Side { .. } if self.next(2) == 0 => super::Landed::Stood,
            _ => super::Landed::Named,
        };
        let link_at = matches!(stop, Stop::Link(_)).then(|| (self.func(), OplineIdx(self.next(9))));
        TraceRecord {
            start,
            root: self.func(),
            entered,
            landed,
            events,
            stop,
            link_at,
        }
    }
}

/// `dump` then `parse` is the identity, over records drawn from every
/// event, sample, start and stop, two thousand natively and twenty under
/// Miri, which interprets each; and `parse` refuses a text with no stop
/// or with a word it does not know.
#[test]
fn dump_then_parse_is_the_identity() {
    let seeds = if cfg!(miri) { 20 } else { 2000 };
    for seed in 0..seeds {
        let record = Draw(seed).record();
        let text = record.dump();
        assert_eq!(
            TraceRecord::parse(&text),
            Ok(record),
            "seed {seed}:\n{text}"
        );
    }
    assert_eq!(tier::class_number(tier::numbered_class(3)), 3);
    assert!(TraceRecord::parse("start loop\nroot e1\n").is_err());
    assert!(
        TraceRecord::parse("start loop\nroot e1\nop e1 0 long.square - - -\nstop loop\n").is_err()
    );
    assert!(TraceRecord::parse("start sideways\nroot e1\nstop loop\n").is_err());
}
