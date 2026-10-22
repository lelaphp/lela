//! Seam 9 under seam 8: the observer's events where a tier is bound
//! (design §4.3 rows 8 and 9; `jit-design.md` §4.3). The two homes,
//! [`crate::observer::call`] at a frame's push and
//! [`crate::observer::returned`] at `returned_to_caller` and
//! `unwind_from`'s uncaught arm, stand in code no tier parameter reaches,
//! so a stepped program raises them as the loop does: [`step`] and
//! [`unwind`] dispatch with `Nop` at the safepoints, so a debugger's own
//! `safepoint` is never asked from inside its stepping, and reach the
//! same homes through the same handlers; a tier raises no event of its
//! own (#1799, #1813). What the loop does between the handlers and not
//! in one, the interrupt check at a pushed frame's activation, `step`
//! does too, so an interrupt meets a stepped call where it meets the
//! loop's.
//!
//! The programs `super::observer` holds the loop to, and a generator
//! sent to, are served four ways, and the events each recording
//! observer is handed must be the loop's: under `Nop`; under `Probe`,
//! the seam's test adapter; under a debugging host that steps every
//! serving one handler at a time as `sapi/phpdbg` does, handing back
//! the entry frame's own leaving and an uncaught throwable; and under a
//! recorder that steps the entry frame's return as well.
//!
//! [`step`]: crate::tier::step
//! [`unwind`]: crate::tier::unwind

use php_heap::FrameRef;

use super::observer::observed;
use super::test_ext::{Event, Phase, Recording, TestExt};
use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::request::{Opcode, Raised, Request};
use crate::engine::{Engine, EngineBuilder};
use crate::probe::Probe;
use crate::test::Files;
use crate::tier::{self, Flow, Site, Stepped, Tier};

/// What a stepping tier keeps, its request state: how many handlers it
/// dispatched through `step` or `unwind`, so a serving that stepped
/// nothing does not pass as one, whether it has raised the interrupt it
/// is armed with, and, for a [`Stepper`] that `SEES`, where it stood
/// before each handler, `"<line> <opcode>"`, or `"<line> threw <class>"`
/// where it stood unwinding a throwable.
#[derive(Debug, Default)]
pub(crate) struct Stepping {
    steps: usize,
    fired: bool,
    pub(crate) seen: Vec<String>,
}

/// The extension a stepping tier declares its state through.
fn stepping() -> Extension {
    ExtensionBuilder::default()
        .request_state::<Stepping>()
        .build("stepping")
}

/// The interrupt an execution timer of one second leaves when it fires,
/// raised once a request, at the first entry that takes it.
fn fire(req: &mut Request) {
    if !std::mem::replace(&mut req.state::<Stepping>().fired, true) {
        req.limits().interrupt().elapsed(1);
    }
}

/// The stepping loop of `sapi/phpdbg`'s `stepped`, the prompt taken out:
/// a frame unwinding is unwound where it is not the serving's entry,
/// where the entry catches what is pending or where nothing is, and
/// handed back otherwise; the entry frame's leaving is handed back
/// unless `steps_leaving`; anything else is one `step`. Where it stood
/// is kept in [`Stepping::seen`] where `sees`.
fn stepped(req: &mut Request, frame: FrameRef, steps_leaving: bool, sees: bool) -> Flow {
    let mut at = frame;
    let mut unwinding = false;
    loop {
        let Some((line, opcode)) = tier::frame(req, at).and_then(|view| {
            let code = view.op_array()?;
            let opcode = code.opline(view.opline()).map(|op| op.opcode())?;
            Some((view.line(), opcode))
        }) else {
            return Flow::Resume(at);
        };
        // The unwind exit, or a fatal leaving: nothing a debugger stops at.
        if matches!(req.diagnostics().raised(), Some(Raised::UnwindExit))
            || (req.diagnostics().raised().is_none() && req.diagnostics().bailing_out())
        {
            return Flow::Resume(at);
        }
        if sees {
            let stood = match req.diagnostics().raised() {
                Some(Raised::Throwable(object)) if unwinding => {
                    format!("{line} threw {}", req.symbols().class_name(object.class()))
                }
                _ => format!("{line} {}", opcode.name()),
            };
            req.state::<Stepping>().seen.push(stood);
        }
        let entry = tier::entry(req);
        let stepped = if unwinding {
            let pending = matches!(
                req.diagnostics().raised(),
                Some(Raised::Throwable(_) | Raised::GracefulExit)
            );
            if at != entry || tier::catches(req, at) || !pending {
                tier::unwind(req, at)
            } else {
                return Flow::Resume(at);
            }
        } else if at == entry && !steps_leaving && leaves(opcode) {
            return Flow::Resume(at);
        } else {
            tier::step(req, at)
        };
        req.state::<Stepping>().steps += 1;
        match stepped {
            Ok(
                Stepped::Same(next)
                | Stepped::Pushed(next)
                | Stepped::Popped(next)
                | Stepped::Moved(next),
            ) => {
                at = next;
                unwinding = false;
            }
            Ok(Stepped::Threw(next)) => {
                if unwinding && next == at && next == entry {
                    return Flow::Resume(next);
                }
                at = next;
                unwinding = true;
            }
            Ok(Stepped::Done) => return Flow::Returned,
            Err(fatal) => return Flow::Bail(fatal),
        }
    }
}

/// The oplines that pop the frame they stand in, as phpdbg's `leaves`.
fn leaves(opcode: Opcode) -> bool {
    matches!(
        opcode.name(),
        "ZEND_RETURN"
            | "ZEND_RETURN_BY_REF"
            | "ZEND_GENERATOR_RETURN"
            | "ZEND_YIELD"
            | "ZEND_YIELD_FROM"
    )
}

/// A stepping host, the one stepping double the engine's tests drive
/// the tier face with: every serving taken over at its entry frame's
/// `Site::Entry`, the reentered ones a native's callback serves too.
/// `Stepper<false>` is the debugger, which hands the entry frame's
/// leaving back to the loop as phpdbg does; `Stepper<true>` a recorder,
/// which steps it, so `step` itself ends the serving. An `ARMED` one
/// raises the interrupt ([`fire`]) at its first entry; one that `SEES`
/// keeps where it stood before each handler ([`Stepping::seen`]).
///
/// Its exit rule is phpdbg's (`sapi/phpdbg/src/debugger.rs`'s `stepped`):
/// the serving handed back is `Flow::Resume` at the frame it stands in,
/// a fatal is `Flow::Bail`, and a `step` or `unwind` that ended the
/// serving, `Stepped::Done`, answers `Flow::Returned`, since the entry
/// frame has left and there is nothing for the loop to go on with.
pub(crate) struct Stepper<
    const STEPS_LEAVING: bool,
    const ARMED: bool = false,
    const SEES: bool = false,
>;

impl<const STEPS_LEAVING: bool, const ARMED: bool, const SEES: bool> Tier
    for Stepper<STEPS_LEAVING, ARMED, SEES>
{
    type State = ();

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, _: crate::tier::Held<'_>) -> Flow {
        if site != Site::Entry || frame != tier::entry(req) {
            return Flow::Continue;
        }
        if ARMED {
            fire(req);
        }
        stepped(req, frame, STEPS_LEAVING, SEES)
    }

    fn extension() -> Option<Extension> {
        Some(stepping())
    }
}

/// The loop alone, with the interrupt raised at its first entry
/// ([`fire`]), as the armed stepping hosts raise it at theirs.
struct Firing;

impl Tier for Firing {
    type State = ();

    fn safepoint(req: &mut Request, _: FrameRef, site: Site, _: crate::tier::Held<'_>) -> Flow {
        if site == Site::Entry {
            fire(req);
        }
        Flow::Continue
    }

    fn extension() -> Option<Extension> {
        Some(stepping())
    }
}

/// A program and the files it includes, written beside it.
struct Program {
    name: &'static str,
    source: &'static str,
    beside: &'static [(&'static str, &'static str)],
}

/// The programs whose events `super::observer` pins as php's: calls two
/// observers deep, a declined function and a callback a native calls,
/// an include and an eval, a `RECV` that throws, unwinds caught,
/// uncaught and by `exit`, fatals the loop and a native raise, and the
/// time limit inside a function a generator's body called, which the
/// loop resumes and no tier steps; and, held to the loop alone, a
/// generator sent to.
const PROGRAMS: &[Program] = &[
    Program {
        name: "calls",
        source: "<?php\nfunction g($y) { return $y * 2; }\nfunction f($x) { return g($x) + 1; }\necho f(1), f(2);\n",
        beside: &[],
    },
    Program {
        name: "declined",
        source: "<?php\nfunction declined() { return 1; }\nclass C { static function m() { return declined(); } }\n\
                 C::m(); C::m();\nset_error_handler(function () { return declined(); });\necho $undefined;\n",
        beside: &[],
    },
    Program {
        name: "include",
        source: "<?php\necho include __DIR__ . '/inc.php';\necho eval('$v = 7; return $v;');\n",
        beside: &[(
            "inc.php",
            "<?php\nfunction h() { return 'h'; }\nreturn h();\n",
        )],
    },
    Program {
        name: "recv",
        source: "<?php\nfunction t(array $a) { return $a; }\n\
                 try { t(1); } catch (TypeError $e) { echo 'a'; }\n\
                 set_error_handler('t');\ntry { echo $u; } catch (TypeError $e) { echo 'b'; }\n",
        beside: &[],
    },
    Program {
        name: "caught",
        source: "<?php\nfunction b() { throw new Exception('x'); }\nfunction a() { b(); }\n\
                 try { a(); } catch (Exception $e) { echo 'c'; } finally { echo 'f'; }\n",
        beside: &[],
    },
    Program {
        name: "uncaught",
        source: "<?php\nfunction b() { throw new Exception('x'); }\nfunction a() { b(); }\na();\n",
        beside: &[],
    },
    Program {
        name: "exited",
        source: "<?php\nfunction b() { exit(3); }\nfunction a() { b(); }\na();\n",
        beside: &[],
    },
    Program {
        name: "memory",
        source: "<?php\nini_set('memory_limit', '2M');\n\
                 function s() { echo 's'; }\nregister_shutdown_function('s');\n\
                 function b() { $s = '.'; while (true) { $s .= $s; } }\nfunction a() { return b(); }\na();\n",
        beside: &[],
    },
    Program {
        name: "user",
        source: "<?php\nfunction b($no, $str) { trigger_error('boom', E_USER_ERROR); }\n\
                 function a() { set_error_handler('b'); echo $u; }\na();\n",
        beside: &[],
    },
    Program {
        name: "timeout",
        source: "<?php\nset_time_limit(1);\n\
                 function b() { while (true); }\n\
                 function g() { b(); yield 1; }\nfunction a() { foreach (g() as $v) {} }\na();\n",
        beside: &[],
    },
    Program {
        name: "generator",
        source: "<?php\nfunction g() { $x = yield 1; yield h($x); }\nfunction h($v) { return $v * 2; }\n\
                 function a() { $g = g(); $g->current(); $g->send(4); return $g->current(); }\necho a();\n",
        beside: &[],
    },
];

/// A call the main file makes before any back-edge or native: the loop
/// checks the interrupt at the callee's activation, after its push has
/// begun it.
const INTERRUPTED: &[Program] = &[Program {
    name: "interrupted",
    source: "<?php\nfunction f() { return 1; }\nf();\necho 'past';\n",
    beside: &[],
}];

/// What one program raised: the observer's events and what it echoed.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    events: Vec<Event>,
    stdout: String,
}

/// What each of `programs` raised under the engine `builder` makes, and
/// how many handlers a stepping tier dispatched over all of them.
fn served(builder: fn() -> EngineBuilder, programs: &[Program]) -> (Vec<Seen>, usize) {
    let rec = Recording::start();
    let mut steps = 0;
    let engine = builder()
        .extensions(&[TestExt::<0>::observing(), TestExt::<1>::observing()])
        .build();
    let mut stdouts = Vec::new();
    for program in programs {
        let files = Files::new(&format!("stepped-{}", program.name), &[]);
        let dir = files.path();
        for (name, source) in program.beside {
            files.write(name, source);
        }
        rec.mark(program.name);
        let main = dir.join("main.php");
        files.write("main.php", program.source);
        let (out, kept) = tier::serve_kept::<Stepping>(
            &engine,
            dir,
            main.to_string_lossy().as_bytes(),
            program.source,
        );
        steps += kept.map_or(0, |kept| kept.steps);
        stdouts.push(out.stdout_text());
    }
    rec.mark("end");
    let events = rec.events();
    let ends = programs.iter().skip(1).map(|p| p.name).chain(["end"]);
    let seen = programs
        .iter()
        .zip(ends)
        .zip(stdouts)
        .map(|((program, to), stdout)| Seen {
            events: observed(&events.between(program.name, to)),
            stdout,
        })
        .collect();
    (seen, steps)
}

/// What the loop raised for each program, held against what the tier
/// `named` raised for the same program.
fn same_as_the_loop(named: &str, programs: &[Program], plain: &[Seen], bound: &[Seen]) {
    assert_eq!(plain.len(), bound.len(), "{named}: a program unserved");
    for ((program, plain), bound) in programs.iter().zip(plain).zip(bound) {
        assert_eq!(
            plain, bound,
            "{}: the loop's events and stdout (left), {named}'s (right)",
            program.name
        );
    }
}

/// A stepped program raises the events the loop raises, under `Probe`
/// and under a debugging host and a recorder that drive `step` and
/// `unwind` one handler at a time: every begin and end in the same
/// order, paired through the unwinds, the `RECV` refusals and the
/// fatals. A sequence that observed nothing does not pass here.
#[test]
fn a_stepped_program_raises_the_events_the_loop_raises() {
    let (plain, _) = served(Engine::cli_builder, PROGRAMS);
    for (program, seen) in PROGRAMS.iter().zip(&plain) {
        assert!(
            seen.events
                .iter()
                .any(|e| matches!(e, Event::Ext(_, Phase::Begin(_)))),
            "{}: the loop observed nothing: {seen:?}",
            program.name
        );
    }
    let (probed, _) = served(|| Engine::cli_builder().tier::<Probe>(), PROGRAMS);
    same_as_the_loop("Probe", PROGRAMS, &plain, &probed);
    let (debugged, steps) = served(|| Engine::cli_builder().tier::<Stepper<false>>(), PROGRAMS);
    assert!(steps > 0, "the debugger stepped nothing");
    same_as_the_loop("the debugger", PROGRAMS, &plain, &debugged);
    let (recorded, steps) = served(|| Engine::cli_builder().tier::<Stepper<true>>(), PROGRAMS);
    assert!(steps > 0, "the recorder stepped nothing");
    same_as_the_loop("the recorder", PROGRAMS, &plain, &recorded);
}

/// An interrupt standing when a stepped call pushes its frame is met at
/// the callee's activation, as the loop's `activated` meets it and as
/// php's `ZEND_VM_ENTER_EX` checks it inside `ZEND_DO_UCALL`: the
/// callee begun by its push, the time limit's fatal rendered there, and
/// the callee and the main file ended after it, with nothing echoed.
#[test]
fn a_stepped_call_meets_the_interrupt_at_its_activation() {
    let (plain, _) = served(|| Engine::cli_builder().tier::<Firing>(), INTERRUPTED);
    let seen = plain.first().expect("the request was seen");
    let fatal = seen
        .events
        .iter()
        .position(
            |e| matches!(e, Event::Ext(0, Phase::Error(m)) if m.starts_with("Maximum execution time")),
        )
        .unwrap_or_else(|| panic!("the loop met the interrupt: {plain:?}"));
    let ended: Vec<_> = seen.events.iter().skip(fatal).cloned().collect();
    assert!(
        ended.contains(&Event::Ext(0, Phase::End(String::from("f"), None))),
        "the callee begun and ended at the fatal: {plain:?}"
    );
    let (debugged, _) = served(
        || Engine::cli_builder().tier::<Stepper<false, true>>(),
        INTERRUPTED,
    );
    same_as_the_loop("the debugger", INTERRUPTED, &plain, &debugged);
    let (recorded, _) = served(
        || Engine::cli_builder().tier::<Stepper<true, true>>(),
        INTERRUPTED,
    );
    same_as_the_loop("the recorder", INTERRUPTED, &plain, &recorded);
}
