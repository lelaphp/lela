//! Seam 9 under seam 8: the observer's events where a tier is bound
//! (design §4.3 rows 8 and 9; `jit-design.md` §4.3). The two homes,
//! [`crate::observer::call`] at a frame's push and
//! [`crate::observer::returned`] at `returned_to_caller` and
//! `unwind_from`'s uncaught arm, stand in code no tier parameter reaches,
//! so a stepped program raises them as the loop does: [`step`] and
//! [`unwind`] dispatch with `Nop` at the safepoints, so a debugger's own
//! `safepoint` is never asked from inside its stepping, and reach the
//! same homes through the same handlers; a tier raises no event of its
//! own (#1799, #1813).
//!
//! Each program [`super::observer`] holds the loop to is served four
//! ways, and the events each recording observer is handed must be the
//! loop's: under `Nop`; under `Probe`, the seam's test adapter; under a
//! debugging host that steps every serving one handler at a time as
//! `sapi/phpdbg` does, handing back the entry frame's own leaving and
//! an uncaught throwable; and under a recorder that steps the entry
//! frame's return as well.
//!
//! [`step`]: crate::tier::step
//! [`unwind`]: crate::tier::unwind

use std::cell::Cell;

use php_heap::FrameRef;

use super::observer::{observed, scratch, serve_main};
use super::test_ext::{Event, Recording, TestExt};
use crate::api::request::Request;
use crate::engine::{Engine, EngineBuilder};
use crate::probe::Probe;
use crate::tier::{self, Flow, Opcode, Site, Stepped, Tier};

thread_local! {
    /// How many handlers a stepping tier dispatched through `step` or
    /// `unwind`, so a serving that stepped nothing does not pass as one.
    static STEPS: Cell<usize> = const { Cell::new(0) };
}

/// Whether the stepping host hands the entry frame's own leaving back
/// to the loop, as phpdbg does, or steps it too.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leaving {
    HandedBack,
    Stepped,
}

/// The stepping loop of `sapi/phpdbg`'s `stepped`, the prompt taken out:
/// a frame unwinding is unwound where it is not the serving's entry,
/// where the entry catches what is pending or where nothing is, and
/// handed back otherwise; anything else is one `step`.
fn stepped(req: &mut Request, frame: FrameRef, leaving: Leaving) -> Flow {
    let mut at = frame;
    let mut unwinding = false;
    loop {
        let Some(position) = tier::position(req, at) else {
            return Flow::Resume(at);
        };
        if tier::exiting(req) {
            return Flow::Resume(at);
        }
        let entry = tier::entry(req);
        let stepped = if unwinding {
            if at != entry || tier::catches(req, at) || tier::pending(req).is_none() {
                tier::unwind(req, at)
            } else {
                return Flow::Resume(at);
            }
        } else if at == entry && leaving == Leaving::HandedBack && leaves(position.opcode) {
            return Flow::Resume(at);
        } else {
            tier::step(req, at)
        };
        STEPS.set(STEPS.get() + 1);
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
        opcode,
        Opcode::Return
            | Opcode::ReturnByRef
            | Opcode::GeneratorReturn
            | Opcode::Yield
            | Opcode::YieldFrom
    )
}

/// The debugging host: every serving taken over at its entry frame's
/// `Site::Entry`, the reentered ones a native's callback serves too.
struct Debugger;

impl Tier for Debugger {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        if site != Site::Entry || frame != tier::entry(req) {
            return Flow::Continue;
        }
        stepped(req, frame, Leaving::HandedBack)
    }
}

/// A recorder: as the debugger, with the entry frame's return stepped
/// rather than handed back, so `step` itself ends the serving.
struct Recorder;

impl Tier for Recorder {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        if site != Site::Entry || frame != tier::entry(req) {
            return Flow::Continue;
        }
        stepped(req, frame, Leaving::Stepped)
    }
}

/// A program and the files it includes, written beside it.
struct Program {
    name: &'static str,
    source: &'static str,
    beside: &'static [(&'static str, &'static str)],
}

/// The programs `super::observer` pins the loop's events for: calls two
/// observers deep, a declined function and a callback a native calls,
/// an include and an eval, a `RECV` that throws, unwinds caught,
/// uncaught and by `exit`, fatals the loop and a native raise, the time
/// limit inside a function a generator's body called, which the loop
/// resumes and no tier steps, and a generator sent to.
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

/// The observer's events of every program and the stdout each served,
/// under the engine `builder` makes, and how many handlers a stepping
/// tier dispatched.
fn served(builder: fn() -> EngineBuilder) -> (Vec<(Vec<Event>, String)>, usize) {
    let rec = Recording::start();
    STEPS.set(0);
    let engine = builder()
        .extensions(&[TestExt::<0>::observing(), TestExt::<1>::observing()])
        .build();
    let mut outs = Vec::new();
    for program in PROGRAMS {
        let dir = scratch(&format!("stepped_{}", program.name));
        for (name, source) in program.beside {
            let _ = std::fs::write(dir.join(name), source);
        }
        rec.mark(program.name);
        let out = serve_main(&engine, &dir, program.source);
        outs.push(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    rec.mark("end");
    let events = rec.events();
    let ends = PROGRAMS.iter().skip(1).map(|p| p.name).chain(["end"]);
    let each = PROGRAMS
        .iter()
        .zip(ends)
        .zip(outs)
        .map(|((program, to), out)| (observed(&events.between(program.name, to)), out))
        .collect();
    (each, STEPS.get())
}

/// What the loop raised for each program, held against what `tier`
/// raised for the same program, by name.
fn same_as_the_loop(tier: &str, loop_: &[(Vec<Event>, String)], under: &[(Vec<Event>, String)]) {
    for ((program, plain), bound) in PROGRAMS.iter().zip(loop_).zip(under) {
        assert_eq!(
            plain, bound,
            "{}: the loop's events and stdout (left), {tier}'s (right)",
            program.name
        );
    }
}

/// A stepped program raises the events the loop raises, under `Probe`
/// and under a debugging host and a recorder that drive `step` and
/// `unwind` one handler at a time: every begin and end in the same
/// order, paired through the unwinds, the `RECV` refusals and the
/// fatals, and the loop's own events are php's as `super::observer`
/// pins them, so a sequence that observed nothing does not pass here.
#[test]
fn a_stepped_program_raises_the_events_the_loop_raises() {
    let (plain, _) = served(Engine::cli_builder);
    for (program, (events, _)) in PROGRAMS.iter().zip(&plain) {
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Ext(_, super::test_ext::Phase::Begin(_)))),
            "{}: the loop observed nothing: {events:?}",
            program.name
        );
    }
    let (probed, _) = served(|| Engine::cli_builder().tier::<Probe>());
    same_as_the_loop("Probe", &plain, &probed);
    let (debugged, steps) = served(|| Engine::cli_builder().tier::<Debugger>());
    assert!(steps > 0, "the debugger stepped nothing");
    same_as_the_loop("the debugger", &plain, &debugged);
    let (recorded, steps) = served(|| Engine::cli_builder().tier::<Recorder>());
    assert!(steps > 0, "the recorder stepped nothing");
    same_as_the_loop("the recorder", &plain, &recorded);
}
