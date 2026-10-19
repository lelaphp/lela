//! `record` at its seam (`jit-design.md` §4.1 *Tested*, §7): the
//! recorder driven on real requests by a tier that records from a chosen
//! safepoint, one fixture per `Stepped` arm and one per stop of php's
//! list it takes, each held to the record it wrote and to `dump` then
//! `parse`; and the text form as a property over generated records.

use php_engine::tier::{
    self, Extension, ExtensionBuilder, Flow, FrameRef, FuncRef, Refusal, Request, SafepointIdx,
    Sample, Site, Start as EngineStart, Stepped, Tier, TraceNum, UnitView,
};
use php_heap::ZvalType;
use php_unit::{MapPtrOffset, OplineIdx};

use super::{Event, RECORDING, Start, Stop, TraceRecord, header_at, returns, run};
use crate::policy::{self, Holder, Outcome, Policy};
use crate::table::TraceId;
use crate::testing::{self, function_of};
use crate::types::ExitId;

/// Where the recording tier starts, as the engine's ini says: the
/// activation of the function `record.function` names (empty for the
/// included file's own code), then, for `record.start=loop`, the first
/// loop header it reaches and, for `return`, the first return, stepping
/// only within that frame. `record.compiled` and `record.blacklisted`
/// name functions met before whose entry words are settled so first;
/// `record.compiled_inner` names one whose loop headers after its first
/// are settled compiled.
const PLAN: [&str; 5] = [
    "record.function",
    "record.start",
    "record.compiled",
    "record.blacklisted",
    "record.compiled_inner",
];

/// What the recording tier keeps, the request's: the op arrays of the
/// functions it met, the records it took, and the trace number every
/// site was asked at.
#[derive(Debug, Default)]
struct Kept {
    funcs: Vec<(Vec<u8>, FuncRef)>,
    taken: Vec<TraceRecord>,
    asked: Vec<TraceNum>,
    done: bool,
}

/// The `Policy` the recording tier records under, the engine's.
#[derive(Debug)]
struct Recorder(Policy);

/// The tier that records once, where its plan says.
struct Records;

impl Tier for Records {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        let asked = tier::trace_num(req);
        req.state::<Kept>().asked.push(asked);
        if site != Site::Entry {
            return Flow::Continue;
        }
        let name = function_of(req, frame);
        let held = tier::func(req, frame).filter(|f| matches!(f.offset(), MapPtrOffset::Engine(_)));
        if let (Some(name), Some(func)) = (&name, held) {
            let kept = req.state::<Kept>();
            if !kept.funcs.iter().any(|(n, _)| n == name) {
                kept.funcs.push((name.clone(), func));
            }
        }
        let wanted =
            plan(req, "record.function").as_deref() == Some(name.as_deref().unwrap_or_default());
        if !wanted || held.is_none() || req.state::<Kept>().done {
            return Flow::Continue;
        }
        req.state::<Kept>().done = true;
        settle_words(req);
        let start = match plan(req, "record.start").as_deref() {
            Some(b"loop") => Start::Loop,
            Some(b"return") => Start::Return,
            _ => Start::Enter,
        };
        let at = match walk_to(req, frame, start) {
            Ok(at) => at,
            Err(flow) => return flow,
        };
        let Some(policy) = req.engine::<Recorder>().map(|recorder| recorder.0.clone()) else {
            return Flow::Continue;
        };
        match run(req, frame, start, at, &policy) {
            Ok((recorded, ended)) => {
                req.state::<Kept>().taken.push(recorded.record);
                if ended.stepped {
                    Flow::Resume(ended.at)
                } else {
                    Flow::Continue
                }
            }
            Err(super::Bailed::Fatal(fatal)) => Flow::Bail(fatal),
            Err(super::Bailed::Unwinding(at)) => Flow::Resume(at),
        }
    }

    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .ini(
                    testing::declared(&PLAN)
                        .into_iter()
                        .chain(testing::declared(&testing::OPCACHE_JIT))
                        .collect(),
                )
                .engine_state(recorder)
                .request_state::<Kept>()
                .build("records"),
        )
    }
}

/// The recording tier's engine state.
#[expect(
    clippy::unnecessary_wraps,
    reason = "an engine start answers a Refusal where it fails; this one never does"
)]
fn recorder(start: &EngineStart) -> Result<Recorder, Refusal> {
    Ok(Recorder(Policy::from_ini(start.ini(), &mut Vec::new())))
}

/// The plan's entry `name`, set.
fn plan(req: &Request, name: &str) -> Option<Vec<u8>> {
    req.ini_get(name.as_bytes()).map(<[u8]>::to_vec)
}

/// The entry words the plan names settled, through claims of their own.
fn settle_words(req: &mut Request) {
    let policy = testing::policy(&[]);
    let compiled = plan(req, "record.compiled").unwrap_or_default();
    let blacklisted = plan(req, "record.blacklisted").unwrap_or_default();
    let settled = (compiled
        .split(|b| *b == b',')
        .map(|name| (name, Outcome::Published(trace(7)))))
    .chain(
        blacklisted
            .split(|b| *b == b',')
            .map(|name| (name, Outcome::Blacklist(Stop::NotSupported))),
    );
    for (name, outcome) in settled {
        let Some(func) = met(req, name) else { continue };
        let holder = Holder::Root {
            epoch: tier::epoch(req),
            func,
            at: SafepointIdx::ENTRY,
        };
        if let Some(claim) = policy::claim(holder, &policy, Start::Enter) {
            policy::settle(claim, &outcome);
        }
    }
    let inner = plan(req, "record.compiled_inner").unwrap_or_default();
    let Some(func) = met(req, &inner) else { return };
    let mut view = UnitView::new(req);
    let headers: Vec<SafepointIdx> = view
        .hold(req, func)
        .then(|| view.op_array(func))
        .flatten()
        .map(|code| {
            (0..code.oplines.len())
                .filter_map(|at| header_at(code, OplineIdx(u32::try_from(at).ok()?)))
                .skip(1)
                .collect()
        })
        .unwrap_or_default();
    for at in headers {
        let holder = Holder::Root {
            epoch: tier::epoch(req),
            func,
            at,
        };
        if let Some(claim) = policy::claim(holder, &policy, Start::Loop) {
            policy::settle(claim, &Outcome::Published(trace(7)));
        }
    }
}

/// The op array of the function `name` the tier met.
fn met(req: &mut Request, name: &[u8]) -> Option<FuncRef> {
    req.state::<Kept>()
        .funcs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, f)| *f)
}

/// `frame` stepped within itself to where `start` records from: the
/// safepoint's number there.
fn walk_to(req: &mut Request, frame: FrameRef, start: Start) -> Result<SafepointIdx, Flow> {
    let mut view = UnitView::new(req);
    let Some(func) = tier::func(req, frame).filter(|f| view.hold(req, *f)) else {
        return Err(Flow::Continue);
    };
    loop {
        let at = tier::frame(req, frame).map_or(OplineIdx(0), |view| view.opline());
        let Some(op) = view
            .op_array(func)
            .and_then(|code| code.oplines.get(at.0 as usize))
        else {
            return Err(Flow::Continue);
        };
        let header = view.op_array(func).and_then(|code| header_at(code, at));
        match (start, header) {
            (Start::Enter, _) => return Ok(SafepointIdx::ENTRY),
            (Start::Loop, Some(header)) => return Ok(header),
            (Start::Return, _) if returns(op.opcode) => return Ok(SafepointIdx(op.out)),
            _ => {}
        }
        match tier::step(req, frame) {
            Ok(Stepped::Same(next)) if next == frame => {}
            Ok(_) => return Err(Flow::Continue),
            Err(fatal) => return Err(Flow::Bail(fatal)),
        }
    }
}

/// A plan: the function to record at and the start.
fn at(function: &'static str, start: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("record.function", function), ("record.start", start)]
}

/// `program` served under the recording tier with `ini`: what the tier
/// kept and what was served.
fn kept(ini: &[(&str, &str)], program: &str) -> (Kept, php_engine::test::Served) {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Records>(ini);
    let (served, kept) = scratch.serve_kept::<Kept>(&engine, program);
    (kept.unwrap_or_default(), served)
}

/// `program` served under the recording tier with `ini`: the one record
/// it took, held to `dump` then `parse`, and what the program wrote.
fn recorded(ini: &[(&str, &str)], program: &str) -> Result<(TraceRecord, String), String> {
    let (mut kept, served) = kept(ini, program);
    let record = kept.taken.pop().ok_or_else(|| {
        format!(
            "nothing was recorded: {:?} {:?}",
            testing::stdout(&served),
            served.fatal
        )
    })?;
    assert_eq!(TraceRecord::parse(&record.dump()).as_ref(), Ok(&record));
    if let Some(fatal) = served.fatal {
        return Err(format!("the program ended in a fatal: {fatal}"));
    }
    Ok((record, testing::stdout(&served)))
}

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

fn trace(n: u32) -> TraceId {
    TraceId::new(n).unwrap_or_else(|| {
        assert!(
            n != 0 && n < TraceId::LIMIT,
            "trace {n} is a number a word holds"
        );
        trace(1)
    })
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

/// A return trace: its first opline is the `RETURN` it started at, the
/// first `Popped` roots it where it lands, and it goes on in the caller
/// to the caller's own return.
#[test]
fn a_return_trace_starts_at_its_return_and_goes_on_in_the_caller() -> Result<(), String> {
    let (record, out) = recorded(
        &at("g", "return"),
        "function g($x) { return $x + 1; }
        function f() { $a = g(1); return $a * 2; }
        echo f(), \"\\n\";",
    )?;
    assert_eq!(out, "4\n");
    assert_eq!((record.start, record.stop), (Start::Return, Stop::Return));
    assert_eq!(enters(&record), 0);
    let Some(Event::Op { func: first, .. }) = record.events.first() else {
        return Err(format!("no first opline: {:?}", record.events));
    };
    assert_eq!(*first, record.root, "the RETURN is g's");
    let in_caller = record
        .events
        .iter()
        .any(|event| matches!(event, Event::Op { func, .. } if *func != record.root));
    assert!(in_caller, "the trace goes on in f: {:?}", record.events);
    Ok(())
}

/// A call to a generator function: its frame is activated and then moved
/// to the heap, where the recorder stops.
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

/// A throw: the step leaves a throwable pending and the recorder stops
/// with `exception`, the program going on to catch it.
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

/// A fused compare-and-branch is one `Op` with its direction on it: the
/// `for` condition jumping back to the header, and an `if` falling
/// through and jumping over.
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

/// A call into a function an `eval` declared: its op array is the
/// request's, which no trace may read, and the recorder stops at it.
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
/// running execution's trace number is put back: the shutdown function
/// the fatal leaves to serve is asked with none.
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
        kept.taken.first().map(TraceRecord::dump)
    );
    let fatal = served.fatal.clone().unwrap_or_default();
    assert!(fatal.contains("Allowed memory size"), "{fatal}");
    assert!(
        testing::stdout(&served).ends_with("shut"),
        "{}",
        testing::stdout(&served)
    );
    assert_eq!(kept.asked.last(), Some(&TraceNum::NONE), "{:?}", kept.asked);
}

/// While a recording runs, the running execution's trace number is set,
/// so a nested `execute` is asked with it, and it is none again after.
#[test]
fn the_trace_number_is_set_for_the_recording_alone() {
    let (kept, served) = kept(
        &at("f", "enter"),
        "class D { function __destruct() { $s = 0; for ($i = 0; $i < 3; $i++) { $s += $i; } echo $s; } }
        function f() { $d = new D; $d = null; return 1; }
        echo f(), \"\\n\";",
    );
    assert_eq!(testing::stdout(&served), "31\n");
    assert_eq!(kept.taken.len(), 1);
    assert!(
        kept.asked.contains(&RECORDING),
        "the destructor's loop was asked while recording: {:?}",
        kept.asked
    );
    assert_eq!(kept.asked.last(), Some(&TraceNum::NONE), "{:?}", kept.asked);
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
        TraceRecord {
            start,
            root: self.func(),
            events,
            stop,
        }
    }
}

/// `dump` then `parse` is the identity, over records drawn from every
/// event, sample, start and stop; and `parse` refuses a text with no stop
/// or with a word it does not know.
#[test]
fn dump_then_parse_is_the_identity() {
    for seed in 0..2000 {
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
