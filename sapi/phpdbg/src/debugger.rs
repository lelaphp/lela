//! The debugging host's tier (`jit-design.md` §2.4, revision 8): its
//! `safepoint` is the gate and `step` is its loop, as the recorder's.
//! At the activation of the frame a serving enters, the session takes
//! the serving over and drives it one handler at a time, checking what
//! php's `phpdbg_execute_ex` checks between two handlers
//! (`Z/sapi/phpdbg/phpdbg_prompt.c:1628-1822`): a throwable nothing will
//! catch, a seek, a step, a watchpoint, a breakpoint. It hands the
//! serving back with `Flow::Resume` where the loop's own work is what
//! comes next: the return that pops the frame the serving entered, an
//! exit, a throwable that frame does not catch, which ends the serving
//! as the loop ends it. Everywhere else the tier answers `Continue` and
//! the program runs at the interpreter's speed.

use php_sapi::tier::{self, Flow, FrameRef, Opcode, Request, Site, Stepped, Tier};

use crate::bp::At;
use crate::out;
use crate::prompt::{self, Here, Outcome};
use crate::session::{self, Context, Plan, SeekKind, Session};

/// The tier the `phpdbg` binary binds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Debugger;

impl Tier for Debugger {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        match site {
            Site::Entry => entered(req, frame),
            Site::Loop(_) | Site::Return(_) => Flow::Continue,
        }
    }
}

/// A serving entered at `frame`: the held request's prompt where the
/// session asked for one, then the loop driven.
fn entered(req: &mut Request, frame: FrameRef) -> Flow {
    let Some(plan) = session::with(|s| s.plan).flatten() else {
        return Flow::Continue;
    };
    match plan {
        Plan::Hold => {
            session::with(|s| s.plan = Some(Plan::Run));
            held(req, frame)
        }
        Plan::Idle => {
            let Some(mut session) = session::take() else {
                return Flow::Continue;
            };
            let outcome = prompt::idle_job(&mut session, req, frame);
            session::put(session);
            let _ = outcome;
            tier::exit(req);
            Flow::Resume(frame)
        }
        Plan::Run => drive(req, frame),
    }
}

/// The execution context compiled and standing at its first opline: the
/// compile notice, then the prompt, until `run` goes on in this same
/// request or a command ends it.
fn held(req: &mut Request, frame: FrameRef) -> Flow {
    let Some(mut session) = session::take() else {
        return Flow::Continue;
    };
    if session.context != Context::Announced {
        session.context = Context::Announced;
        if let Some(exec) = &session.exec {
            let message = if session.stdin_code.is_some() {
                b"Successful compilation of stdin input".to_vec()
            } else {
                [&b"Successful compilation of "[..], exec].concat()
            };
            out::notice(&message);
        }
    }
    session.executing = false;
    let outcome = {
        let mut here = Here::Held { req, frame };
        prompt::interactive(&mut session, &mut here)
    };
    let stopped = session.stopped.is_some();
    session::put(session);
    if stopped || outcome == Outcome::Stopped {
        tier::exit(req);
        return Flow::Resume(frame);
    }
    drive(req, frame)
}

/// What the loop does with the opline at hand.
enum Next {
    /// Run it with `step`.
    Step,
    /// Run the unwinder over the frame, which takes its throwable.
    Unwind,
    /// Hand the serving back: the loop does what comes next.
    HandBack,
}

/// Where the loop stands between two handlers.
struct Standing {
    frame: FrameRef,
    /// The edge the loop took last left a throwable for the unwinder:
    /// php's `EX(opline)` at `EG(exception_op)`, `ZEND_HANDLE_EXCEPTION`,
    /// whose line is zero.
    unwinding: bool,
    /// The line of the opline that raised the throwable pending, php's
    /// `EG(opline_before_exception)`, which a listing names while one is.
    raised_line: u32,
}

/// `phpdbg_execute_ex`'s loop over the serving entered at `frame`.
fn drive(req: &mut Request, frame: FrameRef) -> Flow {
    let mut at = Standing {
        frame,
        unwinding: false,
        raised_line: 0,
    };
    loop {
        let Some(position) = tier::position(req, at.frame) else {
            return Flow::Resume(at.frame);
        };
        if tier::exiting(req) {
            return Flow::Resume(at.frame);
        }
        if at.unwinding {
            at.raised_line = position.line;
        }
        if let Some(stop) = checked(req, &at, &position) {
            return stop;
        }
        let entry = tier::entry(req);
        let next = if at.unwinding {
            if at.frame != entry || tier::catches(req, at.frame) {
                Next::Unwind
            } else {
                Next::HandBack
            }
        } else if at.frame == entry && leaves(position.opcode) {
            Next::HandBack
        } else {
            Next::Step
        };
        let stepped = match next {
            Next::HandBack => return Flow::Resume(at.frame),
            Next::Unwind => tier::unwind(req, at.frame),
            Next::Step => tier::step(req, at.frame),
        };
        match stepped {
            Ok(
                Stepped::Same(frame)
                | Stepped::Pushed(frame)
                | Stepped::Popped(frame)
                | Stepped::Moved(frame),
            ) => {
                at.frame = frame;
                at.unwinding = false;
            }
            Ok(Stepped::Threw(frame)) => {
                if matches!(next, Next::Unwind) && frame == at.frame && frame == entry {
                    return Flow::Resume(frame);
                }
                at.frame = frame;
                at.unwinding = true;
            }
            Ok(Stepped::Done) => return Flow::Continue,
            Err(fatal) => return Flow::Bail(fatal),
        }
    }
}

/// Whether the opline pops the frame it stands in, which on the frame a
/// serving entered is the loop's to run.
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

/// What php checks before a handler runs, in `phpdbg_execute_ex`'s
/// order, and the prompt where one of them stops: `Some` where the
/// prompt ended the request.
fn checked(req: &mut Request, at: &Standing, position: &tier::Position) -> Option<Flow> {
    let pending = tier::pending(req);
    // `zend_get_executed_lineno`: the raising opline's while a throwable
    // is pending, as `zend_clear_exception` puts it back.
    let shown = if pending.is_some() {
        at.raised_line
    } else {
        position.line
    };
    let line = if at.unwinding { 0 } else { position.line };
    let serves = tier::serves(req, at.frame);
    let (function, class) = match &serves {
        tier::Serves::Code => (None, None),
        tier::Serves::Function { name, class } => (Some(name.as_slice()), class.as_deref()),
    };
    let here = At {
        file: &position.file,
        line: shown,
        opline: position.opline,
        body: position.body,
        opcode: if at.unwinding {
            "ZEND_HANDLE_EXCEPTION"
        } else {
            position.opcode.name()
        },
        function,
        class,
    };
    // The uncaught throwable, once per throwable.
    let mut stop = false;
    if let Some(thrown) = &pending {
        let handled = session::with(|s| s.handled == Some(thrown.id)).unwrap_or(true);
        if !handled && !caught_anywhere(req, at.frame) {
            session::with(|s| s.handled = Some(thrown.id));
            let message = thrown.message.get(..80).unwrap_or(&thrown.message);
            out::error(
                format!(
                    "Uncaught {} in {} on line {}: {}",
                    String::from_utf8_lossy(&thrown.class),
                    String::from_utf8_lossy(&thrown.file),
                    thrown.line,
                    String::from_utf8_lossy(message)
                )
                .as_bytes(),
            );
            stop = true;
        }
    }
    let uncaught_here = pending.is_some() && !tier::catches(req, at.frame);
    let checks = Checks {
        frame: at.frame.0,
        line,
        uncaught_here,
    };
    let stops = stop || session::with(|s| decide(s, req, &here, &checks)).unwrap_or(false);
    session::with(|s| s.last_line = line);
    if !stops {
        return None;
    }
    interactive(req, at.frame, &position.file, shown)
}

/// What the checks read off the loop.
struct Checks {
    frame: u32,
    /// The opline's line, zero at `ZEND_HANDLE_EXCEPTION`.
    line: u32,
    /// A throwable is pending that this frame does not catch.
    uncaught_here: bool,
}

/// The seek, the step, the watchpoints and the breakpoints, in `phpdbg_execute_ex`'s
/// order (`phpdbg_prompt.c:1713-1788`): whether the loop stops here.
fn decide(s: &mut Session, req: &mut Request, here: &At<'_>, checks: &Checks) -> bool {
    if let Some(seek) = &s.seek {
        if seek.frame != checks.frame {
            if s.stepping {
                s.stepping = false;
                return true;
            }
            return false;
        }
        let reached = seek.oplines.contains(&here.opline) || checks.uncaught_here;
        match seek.kind {
            SeekKind::Until => {
                if !reached {
                    return false;
                }
                s.seek = None;
            }
            SeekKind::Finish => {
                if reached {
                    s.seek = None;
                }
                return false;
            }
            SeekKind::Leave => {
                if !reached {
                    return false;
                }
                s.seek = None;
                out::notice(
                    format!(
                        "Breaking for leave at {}:{}",
                        String::from_utf8_lossy(here.file),
                        here.line
                    )
                    .as_bytes(),
                );
                return true;
            }
        }
    }
    if s.stepping && checks.line != s.last_line {
        s.stepping = false;
        return true;
    }
    if crate::watch::changed(s, req) {
        return true;
    }
    if s.breaks.has_pending() {
        s.breaks.resolve_pending(here.file);
    }
    let mut at = here.clone();
    at.line = checks.line;
    if let Some(found) = s.breaks.find(&at)
        && (!matches!(s.breaks.list[found].kind, crate::bp::Kind::File { .. })
            || checks.line != s.last_line)
    {
        s.breaks.hit(found, here);
        return true;
    }
    false
}

/// Whether a user frame of the chain from `frame` down catches the
/// throwable pending.
fn caught_anywhere(req: &mut Request, frame: FrameRef) -> bool {
    let mut at = Some(frame);
    while let Some(frame) = at {
        if tier::catches(req, frame) {
            return true;
        }
        at = tier::caller(req, frame);
    }
    false
}

/// `DO_INTERACTIVE`: the listing, then the prompt with the throwable
/// set aside; `Some` where the prompt ended the request.
fn interactive(req: &mut Request, frame: FrameRef, file: &[u8], line: u32) -> Option<Flow> {
    let mut session = session::take()?;
    crate::list::code(&session, file, line);
    let aside = tier::set_aside(req);
    let outcome = {
        let mut here = Here::Paused { req, frame };
        prompt::interactive(&mut session, &mut here)
    };
    let stopped = session.stopped.is_some();
    session::put(session);
    tier::restore(req, aside);
    if stopped || outcome == Outcome::Stopped {
        tier::exit(req);
        return Some(Flow::Resume(frame));
    }
    None
}
