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

use std::cell::Cell;

use php_heap::FrameRef;

use super::observer::{observed, scratch, serve_main};
use super::test_ext::{Event, Phase, Recording, TestExt};
use crate::api::request::Request;
use crate::engine::{Engine, EngineBuilder};
use crate::probe::Probe;
use crate::tier::{self, Flow, Opcode, Site, Stepped, Tier};

thread_local! {
    /// How many handlers a stepping tier dispatched through `step` or
    /// `unwind`, so a serving that stepped nothing does not pass as one.
    static STEPS: Cell<usize> = const { Cell::new(0) };
    /// Whether the next serving's entry raises the interrupt an
    /// execution timer of one second leaves when it fires.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// The interrupt [`ARMED`] asks for, raised once, at the first entry
/// that takes it.
fn fire_if_armed(req: &mut Request) {
    if ARMED.replace(false) {
        req.interrupt().elapsed(1);
    }
}

/// The stepping loop of `sapi/phpdbg`'s `stepped`, the prompt taken out:
/// a frame unwinding is unwound where it is not the serving's entry,
/// where the entry catches what is pending or where nothing is, and
/// handed back otherwise; the entry frame's leaving is handed back
/// unless `steps_leaving`; anything else is one `step`.
fn stepped(req: &mut Request, frame: FrameRef, steps_leaving: bool) -> Flow {
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
        } else if at == entry && !steps_leaving && leaves(position.opcode) {
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

/// A stepping host: every serving taken over at its entry frame's
/// `Site::Entry`, the reentered ones a native's callback serves too.
/// `Stepper<false>` is the debugger, which hands the entry frame's
/// leaving back to the loop as phpdbg does; `Stepper<true>` a recorder,
/// which steps it, so `step` itself ends the serving.
pub(super) struct Stepper<const STEPS_LEAVING: bool>;

impl<const STEPS_LEAVING: bool> Tier for Stepper<STEPS_LEAVING> {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        if site != Site::Entry || frame != tier::entry(req) {
            return Flow::Continue;
        }
        fire_if_armed(req);
        stepped(req, frame, STEPS_LEAVING)
    }
}

/// The loop alone, with the interrupt [`ARMED`] asks for raised at its
/// first entry, as the stepping hosts raise it at theirs.
struct Firing;

impl Tier for Firing {
    fn safepoint(req: &mut Request, _: FrameRef, site: Site) -> Flow {
        if site == Site::Entry {
            fire_if_armed(req);
        }
        Flow::Continue
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

/// What each of `programs` raised under the engine `builder` makes, each
/// served with the interrupt armed where `armed`, and how many handlers
/// a stepping tier dispatched.
fn served(builder: fn() -> EngineBuilder, programs: &[Program], armed: bool) -> (Vec<Seen>, usize) {
    let rec = Recording::start();
    STEPS.set(0);
    let engine = builder()
        .extensions(&[TestExt::<0>::observing(), TestExt::<1>::observing()])
        .build();
    let mut stdouts = Vec::new();
    for program in programs {
        let dir = scratch(&format!("stepped_{}", program.name));
        for (name, source) in program.beside {
            let _ = std::fs::write(dir.join(name), source);
        }
        rec.mark(program.name);
        ARMED.set(armed);
        let out = serve_main(&engine, &dir, program.source);
        stdouts.push(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    ARMED.set(false);
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
    (seen, STEPS.get())
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
    let (plain, _) = served(Engine::cli_builder, PROGRAMS, false);
    for (program, seen) in PROGRAMS.iter().zip(&plain) {
        assert!(
            seen.events
                .iter()
                .any(|e| matches!(e, Event::Ext(_, Phase::Begin(_)))),
            "{}: the loop observed nothing: {seen:?}",
            program.name
        );
    }
    let (probed, _) = served(|| Engine::cli_builder().tier::<Probe>(), PROGRAMS, false);
    same_as_the_loop("Probe", PROGRAMS, &plain, &probed);
    let (debugged, steps) = served(
        || Engine::cli_builder().tier::<Stepper<false>>(),
        PROGRAMS,
        false,
    );
    assert!(steps > 0, "the debugger stepped nothing");
    same_as_the_loop("the debugger", PROGRAMS, &plain, &debugged);
    let (recorded, steps) = served(
        || Engine::cli_builder().tier::<Stepper<true>>(),
        PROGRAMS,
        false,
    );
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
    let (plain, _) = served(|| Engine::cli_builder().tier::<Firing>(), INTERRUPTED, true);
    let fatal = plain.first().and_then(|seen| {
        seen.events.iter().position(
            |e| matches!(e, Event::Ext(0, Phase::Error(m)) if m.starts_with("Maximum execution time")),
        )
    });
    assert!(fatal.is_some(), "the loop met the interrupt: {plain:?}");
    let ended: Vec<_> = plain
        .first()
        .map(|seen| {
            seen.events
                .iter()
                .skip(fatal.unwrap_or_default())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    assert!(
        ended.contains(&Event::Ext(0, Phase::End(String::from("f"), None))),
        "the callee begun and ended at the fatal: {plain:?}"
    );
    let (debugged, _) = served(
        || Engine::cli_builder().tier::<Stepper<false>>(),
        INTERRUPTED,
        true,
    );
    same_as_the_loop("the debugger", INTERRUPTED, &plain, &debugged);
    let (recorded, _) = served(
        || Engine::cli_builder().tier::<Stepper<true>>(),
        INTERRUPTED,
        true,
    );
    same_as_the_loop("the recorder", INTERRUPTED, &plain, &recorded);
}
