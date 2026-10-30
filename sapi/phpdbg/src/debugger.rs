//! The debugging host's tier (`jit-design.md` §2.4, revision 8): its
//! `safepoint` is where it stops and `step` is its loop, as the recorder's.
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

use php_ext::abi::AfterLog;
use php_sapi::tier::{self, Flow, FrameRef, Hooked, Request, Site, Stepped, Tier};

use crate::bp::{At, OpArray};
use crate::opcodes;
use crate::out;
use crate::prompt::{self, Here, Outcome};
use crate::read;
use crate::session::{self, AfterServe, ExecCompile, Execution, Origin, Plan, SeekKind, Session};
use crate::shown;

/// The tier the `phpdbg` binary binds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Debugger;

impl Tier for Debugger {
    type State = ();

    /// phpdbg's `zend_execute_ex = phpdbg_execute_ex`
    /// (`Z/sapi/phpdbg/phpdbg.c:167`); `zend_execute_internal` it leaves
    /// alone, so a native call is still `DO_ICALL` in its `print` and a
    /// user call is `DO_FCALL`:
    ///
    /// ```php
    /// function foo($b) { return strrev($b); }
    /// foo('ab');
    /// ```
    /// ```request phpdbg
    /// p foo
    /// p e
    /// q
    /// ```
    /// ```expect
    /// [Successful compilation of %SCRATCH%/witness.php]
    /// prompt> [User Function foo (6 ops)]
    /// prompt> [Context %SCRATCH%/witness.php (4 ops)]
    /// prompt>
    /// ```
    /// ```stderr
    /// foo:
    ///      ; (lines=6, args=1, vars=1, tmps=1)
    ///      ; %SCRATCH%/witness.php:2-2
    /// L0002 0000 CV0($b) = RECV 1
    /// L0002 0001 INIT_FCALL 1 96 string("strrev")
    /// L0002 0002 SEND_VAR CV0($b) 1
    /// L0002 0003 V1 = DO_ICALL
    /// L0002 0004 RETURN V1
    /// L0002 0005 RETURN null
    ///
    /// $_main:
    ///      ; (lines=4, args=0, vars=0, tmps=1)
    ///      ; %SCRATCH%/witness.php:1-3
    /// L0003 0000 INIT_FCALL 1 112 string("foo")
    /// L0003 0001 SEND_VAL string("ab") 1
    /// L0003 0002 DO_FCALL
    /// L0003 0003 RETURN int(1)
    /// ```
    const HOOKED: Hooked = Hooked {
        user: true,
        natives: false,
    };

    /// php compiles the `exec` file and executes it only at `run`
    /// (`zend_execute(PHPDBG_G(ops))`, `Z/sapi/phpdbg/phpdbg_prompt.c:866`),
    /// so an observer is handed the main file's begin there, after what
    /// the prompt served before it, and nothing for a cycle never served
    /// ([`executed`]).
    const HOLDS: bool = true;

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, _: tier::Held<'_>) -> Flow {
        match site {
            Site::Entry => entered(req, frame),
            Site::Loop(_) | Site::Return(_) => Flow::Continue,
        }
    }
}

/// A serving entered at `frame`: the held request's prompt or the idle
/// request's command where the session asked for one, and the loop
/// driven over every serving after it, what the prompt serves through
/// `ev` or a registered function among them, as php's
/// `phpdbg_execute_ex` runs every op array the request executes. A
/// registered function is the program's code to the loop: it stops at a
/// breakpoint with its listing, and goes on from there.
///
/// ```php
/// function foo($a = 0) {
///   echo "in foo $a\n";
/// }
/// foo();
/// ```
/// ```request phpdbg
/// register foo
/// b foo
/// foo 7
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Registered foo]
/// prompt> [Breakpoint #0 added at foo]
/// prompt> [Breakpoint #0 in foo() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:   echo "in foo $a\n";
///  00004: }
///  00005: foo();
/// prompt> in foo 7
///
/// prompt>
/// ```
fn entered(req: &mut Request, frame: FrameRef) -> Flow {
    let Some(plan) = session::with(req, |s| {
        // The init files' own request comes before the context's first
        // compile, as do the servings inside them.
        s.started |= !s.initializing && s.plan != Some(Plan::Init);
        s.out.entered();
        s.plan
    })
    .flatten() else {
        return Flow::Continue;
    };
    match plan {
        Plan::Hold => {
            session::with(req, |s| s.plan = Some(Plan::Serve));
            held(req, frame)
        }
        Plan::Init => {
            session::with(req, |s| s.plan = Some(Plan::Serve));
            let Some(mut session) = session::take(req) else {
                return Flow::Continue;
            };
            crate::init::serve(&mut session, &mut Here::Idle { req });
            let quits = session.after_serve == AfterServe::Quit;
            let runs = std::mem::take(&mut session.init_runs);
            if runs && session.stopped.is_none() {
                // Before the run's compile, php's own after the init files,
                // under `PHPDBG_DISCARD_OUTPUT` where a run is due
                // (`Z/sapi/phpdbg/phpdbg.c:1540-1553`): its log written
                // nowhere, each diagnostic displayed. php is fully started
                // after it, before its startup run asks anything.
                out::discarded(&mut session, |session| {
                    prompt::refused_again(session, &mut Here::Idle { req });
                });
                session.started = true;
            }
            if session.stopped.is_some() {
            } else if runs && let Some(line) = session.first_command.take() {
                resubmitted(&mut session, req, line, quits);
            } else if runs && session.execution == Execution::BailedOut {
                // The run they come before finds `in_execution` set, and
                // asks before it compiles.
                if prompt::restart_permitted(&mut session) && !quits {
                    session.stopped = Some(session::Stop::Restart(None));
                } else {
                    idle_prompt(&mut session, req, quits);
                }
            } else if quits {
                // `-rr`'s run compiles it under `PHPDBG_PREVENT_INTERACTIVE`:
                // what the compiler says alone, and the session ends.
                prompt::refused_again(&mut session, &mut Here::Idle { req });
                session.stopped = Some(session::Stop::Quit);
            } else if runs {
                // `-r`'s run compiles the context once the session is
                // fully started, so its log stands at a prompt.
                session.started = true;
                prompt::refused_again(&mut session, &mut Here::Idle { req });
                let _ = prompted(&mut session, req, |req| Here::Idle { req });
            } else {
                refused(&mut session, req);
                let _ = prompted(&mut session, req, |req| Here::Idle { req });
            }
            session::put(req, session);
            tier::exit(req);
            Flow::Resume(frame)
        }
        Plan::Idle => {
            session::with(req, |s| s.plan = Some(Plan::Serve));
            let Some(mut session) = session::take(req) else {
                return Flow::Continue;
            };
            if let Some(command) = session.job.take() {
                prompt::idle_job(&mut session, req, &command);
            } else {
                // A cycle after the first compiles a context that does not
                // compile under `PHPDBG_DISCARD_OUTPUT` once php is fully
                // started, so its log stands at a prompt that writes
                // nothing (`Z/sapi/phpdbg/phpdbg.c:1540-1553`).
                let compiles = session.init_due && session.exec_compile == ExecCompile::Refused;
                crate::init::serve(&mut session, &mut Here::Idle { req });
                if compiles && session.stopped.is_none() {
                    out::discarded(&mut session, |session| {
                        prompt::refused_again(session, &mut Here::Idle { req });
                    });
                }
                let _ = prompted(&mut session, req, |req| Here::Idle { req });
            }
            session::put(req, session);
            tier::exit(req);
            Flow::Resume(frame)
        }
        Plan::Serve => {
            if initialized(req) {
                tier::exit(req);
                return Flow::Resume(frame);
            }
            // The run's own entry, not what the init files or the prompt
            // serve inside the request.
            session::with(req, |s| {
                if !s.initializing
                    && s.origin == Origin::Program
                    && s.execution == Execution::Serving
                {
                    announce(s);
                }
            });
            // A fiber's first frame stands at the bottom of its own
            // stack too, and was begun where it was called.
            if tier::below(req, frame).is_none() && read::serves(req, frame) == read::Serves::Code {
                executed(req, frame)
            } else {
                drive(req, frame)
            }
        }
    }
}

/// The init files are read where a cycle starts with a `run`, before the `run`'s
/// first opline, in the `run`'s request, so what they leave the program
/// finds; true where they stopped the session. php's `run` begins after
/// them: a code block that bailed out of the loop left `in_execution`
/// set, so `run` asks before it restarts, and stands at the prompt where
/// the answer is no; where they left something pending, the `exec` file
/// does not open; and the `run` cleans up after them.
fn initialized(req: &mut Request) -> bool {
    if session::with(req, |s| !s.init_due).unwrap_or(true) {
        return false;
    }
    let Some(mut session) = session::take(req) else {
        return false;
    };
    let execution = std::mem::replace(&mut session.execution, Execution::Not);
    session.program_held = true;
    crate::init::serve(&mut session, &mut Here::Idle { req });
    session.program_held = false;
    // The run's compile after them comes under `PHPDBG_DISCARD_OUTPUT`.
    compiled_after_init(&mut session, req, true);
    // `run`'s clean up from the last execution, after the init files:
    // no seek and no throwable handled, the hits at zero, and a step the
    // init files' prompt asked for kept.
    session.seek = None;
    session.handled = None;
    session.breaks.reset_hits();
    let quits = session.after_serve == AfterServe::Quit;
    if session.inline_serve {
        // A `run` among them runs the program now.
        let frame = tier::entry(req);
        inline(&mut session, req, frame);
    } else if session.stopped.is_none()
        && let Some(line) = session.first_command.take()
    {
        resubmitted(&mut session, req, line, quits);
    } else if session.stopped.is_none() && session.execution == Execution::BailedOut {
        if prompt::restart_permitted(&mut session) && !quits {
            session.stopped = Some(session::Stop::Restart(None));
        } else {
            idle_prompt(&mut session, req, quits);
        }
    } else if session.stopped.is_none() && unopened(&mut session, req) {
        session.out.error(
            format!(
                "Failed to compile {}, cannot run",
                String::from_utf8_lossy(session.exec.as_deref().unwrap_or_default())
            )
            .as_bytes(),
        );
        // `EG(exit_status) = FAILURE`.
        session.status = Some(255);
        session.execution = Execution::Not;
        idle_prompt(&mut session, req, quits);
    } else if session.execution != Execution::BailedOut {
        session.execution = execution;
    }
    let stopped = session.stopped.is_some();
    session::put(req, session);
    stopped
}

/// `phpdbg_compile`'s notice, once for the `exec` file, where nothing has
/// written it yet: at the held prompt, or where `run` compiles an `exec` file
/// the init files left unopened.
pub fn announce(session: &mut Session) {
    if session.exec_compile == ExecCompile::Announced {
        return;
    }
    session.exec_compile = ExecCompile::Announced;
    if let Some(exec) = &session.exec {
        let message = if session.stdin_code.is_some() {
            b"Successful compilation of stdin input".to_vec()
        } else {
            [&b"Successful compilation of "[..], exec].concat()
        };
        session.out.notice(&message);
    }
}

/// The `exec` file's compile after the init files, which the compiler
/// refused: its log as `php_sapi_phpdbg_log_message` writes it, the
/// listing where nothing executes, and the status 255; the first cycle
/// is fully started after it, so a later fatal's log stops at a prompt.
fn refused(session: &mut Session, req: &mut Request) {
    prompt::refused_again(session, &mut Here::Idle { req });
    session.started = true;
}

/// A `run` in the init files about to serve the program at `frame`: the
/// `run`'s clean up, and its `phpdbg_clean` after it, which lets go of the
/// compiled `exec` file, so a `print exec` after it compiles it again.
fn inline(session: &mut Session, req: &mut Request, frame: FrameRef) {
    // The run's `phpdbg_clean(1, 0)` lets the line go.
    session.command_line = None;
    session.execution = Execution::Serving;
    session.seek = None;
    session.handled = None;
    session.breaks.reset_hits();
    session.ran_code = read::frame_code(req, frame);
    session.init_compiled = false;
}

/// A cycle a yes to its startup `run`'s question restarted: its own startup
/// `run` is `phpdbg_interactive(1, first_command)`, the line the cycle
/// before parsed last read at a prompt in place of `run`
/// (`Z/sapi/phpdbg/phpdbg.c:1588-1592`), and the prompt after it, where
/// nothing executes. An init file whose `ev` bailed out has it bail out
/// again, after the record a refused compile left emitted ahead of its
/// fatal, so the question is not asked again.
fn resubmitted(session: &mut Session, req: &mut Request, line: Vec<u8>, quits: bool) {
    let mut parsed = line.clone();
    session.command_line = Some(line);
    match crate::cmd::parse(&mut parsed) {
        Ok(commands) => session.queued = commands,
        Err(crate::cmd::Malformed(message)) => {
            session
                .out
                .error(format!("Parse Error: {message}").as_bytes());
        }
    }
    idle_prompt(session, req, quits);
}

/// Where `-rr` quits once `run` has come back, which it does here
/// without running: the session ends; else the prompt, with nothing
/// executing, stands in this request until a command ends it.
fn idle_prompt(session: &mut Session, req: &mut Request, quits: bool) {
    if quits {
        session.stopped = Some(session::Stop::Quit);
        return;
    }
    let _ = prompted(session, req, |req| Here::Idle { req });
}

/// What `phpdbg_compile` answers where the init files left a throwable
/// or an exit pending: `php_stream_open_for_zend_ex` refuses the file,
/// `Could not open file`, and the `exec` file is not compiled. True where
/// that is so.
fn unopened(session: &mut Session, req: &mut Request) -> bool {
    if !read::open_refused(req) {
        return false;
    }
    // `run` compiles it again, and writes the notice then.
    session.exec_compile = ExecCompile::Unannounced;
    session.out.error(
        format!(
            "Could not open file {}",
            String::from_utf8_lossy(session.exec.as_deref().unwrap_or_default())
        )
        .as_bytes(),
    );
    true
}

/// What the cycle's compile of the `exec` file wrote, which php
/// writes after the init files (`Z/sapi/phpdbg/phpdbg.c:1502-1553`),
/// written there, its log discarded where `discard` says; where the init
/// files stopped the session or left something pending, php's compile
/// does not come, and what the engine's wrote is let go.
fn compiled_after_init(session: &mut Session, req: &Request, discard: bool) {
    if session.stopped.is_some() || read::open_refused(req) {
        session.out.drop_compiled();
    } else {
        session.out.compiled(discard);
    }
}

/// The `exec` file compiled and standing at its first opline: the
/// compile notice, then the prompt, until `run` goes on in this same
/// request or a command ends it. Where the init files left something
/// pending, php's compile after them fails, and the prompt stands with
/// nothing compiled.
fn held(req: &mut Request, frame: FrameRef) -> Flow {
    let Some(mut session) = session::take(req) else {
        return Flow::Continue;
    };
    // The first cycle's compile writes its log; a later cycle's comes
    // under `PHPDBG_DISCARD_OUTPUT`.
    let later = session.init_ran;
    session.program_held = true;
    crate::init::serve(&mut session, &mut Here::Idle { req });
    session.program_held = false;
    compiled_after_init(&mut session, req, later);
    if session.stopped.is_none() && session.inline_serve {
        // A `run` among them runs the program now, the loop stopping for
        // nothing but a throwable nothing catches.
        inline(&mut session, req, frame);
        session::put(req, session);
        return executed(req, frame);
    }
    if session.stopped.is_none() && unopened(&mut session, req) {
        idle_prompt(&mut session, req, false);
    }
    if session.stopped.is_some() {
        session::put(req, session);
        tier::exit(req);
        return Flow::Resume(frame);
    }
    announce(&mut session);
    // What a code block of the init files bailed out of left
    // `in_execution` set.
    if session.execution != Execution::BailedOut {
        session.execution = Execution::Not;
    }
    let outcome = prompted(&mut session, req, |req| Here::Held { req });
    let stopped = session.stopped.is_some();
    session::put(req, session);
    if stopped || outcome == Outcome::Stopped {
        tier::exit(req);
        return Flow::Resume(frame);
    }
    // `run`'s `zend_execute` serves nothing while a throwable stands
    // pending, as one an `ev` that did not compile leaves, and the run
    // ends in it.
    if read::pending(req).is_some() {
        return Flow::Returned;
    }
    executed(req, frame)
}

/// `run`'s `zend_execute` over the `exec` file held at `frame`, the
/// request's first: the observer's begin of the main file, which the
/// engine raises for no frame this tier holds, then the loop.
fn executed(req: &mut Request, frame: FrameRef) -> Flow {
    tier::begin(req, frame);
    drive(req, frame)
}

/// The prompt where nothing is executing, stood at again after a
/// command that goes on with a program none is running, until one ends
/// the request or, in the held request, `run` goes on in it. With no
/// `exec` file to hold, or one the compiler refused, the idle
/// request stands across the prompt's commands as php's one request
/// does, so what one `ev` leaves the next finds.
///
/// ```php
/// function (
/// ```
/// ```request phpdbg
/// ev $y = 2
/// ev $y
/// q
/// ```
/// ```expect exit=255
/// Parse error: Unclosed '(' in %SCRATCH%/witness.php on line 2
/// prompt> 2
/// prompt> 2
/// prompt>
/// ```
fn prompted(
    session: &mut Session,
    req: &mut Request,
    here: for<'r> fn(&'r mut Request) -> Here<'r>,
) -> Outcome {
    loop {
        let outcome = prompt::interactive(session, &mut here(req));
        if outcome != Outcome::NotRunning {
            return outcome;
        }
    }
}

/// What the loop does with the opline at hand.
enum Next {
    /// Step it with `step`.
    Step,
    /// Hand the frame to the unwinder, which takes its throwable.
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

/// `phpdbg_execute_ex`'s loop over the serving entered at `frame`, the
/// frame an outer serving was stepping put back as the session's once it
/// ends; where it ends in a fatal, the frames it stood on stand until the
/// fatal is rendered, and so does the frame it was stepping, which the
/// log's prompt stands in ([`fatal_logged`]).
fn drive(req: &mut Request, frame: FrameRef) -> Flow {
    let outer = session::with(req, |s| s.standing).flatten();
    let flow = stepped(req, frame);
    if !matches!(flow, Flow::Bail(_)) {
        session::with(req, |s| s.standing = outer);
    }
    flow
}

/// What `php_sapi_phpdbg_log_message` does once it has written a fatal's
/// entry (`Z/sapi/phpdbg/phpdbg.c:731-770`), outside what `ev` serves and
/// other than under `-rr`: the listing where the request stands, the frame the
/// loop was stepping or `[no active file]` where nothing is executing,
/// then, once the `exec` file's first compile is behind the
/// session, a prompt there until a command goes on, after which the fatal
/// ends the `run`; a prompt that stopped, `quit` among them, bails out of
/// the log before php's `php_error_cb` writes the status.
///
/// ```php
/// ini_set("memory_limit", "2M");
/// $s = str_repeat("x", 4 << 20);
/// echo "after\n";
/// ```
/// ```request phpdbg
/// ev ini_set("display_errors", 0)
/// ev ini_set("log_errors", 1)
/// r
/// ev 1
/// c
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> 1
/// prompt> 0
/// prompt> [PHP Fatal error:  Allowed memory size of 2097152 bytes exhausted (tried to allocate 4210688 bytes) in %SCRATCH%/witness.php on line 3]
/// >00003: $s = str_repeat("x", 4 << 20);
///  00004: echo "after\n";
/// prompt> prompt> after
/// [Script ended normally]
/// prompt>
/// ```
pub fn fatal_logged(req: &mut Request) -> AfterLog {
    let Some(mut session) = session::take(req) else {
        return AfterLog::Returned;
    };
    // `-rr`'s `PHPDBG_PREVENT_INTERACTIVE` is set when its run begins,
    // after the init files.
    if session.out.logging()
        || session.origin == Origin::Eval
        || (session.after_serve == AfterServe::Quit && !session.initializing)
    {
        session::put(req, session);
        return AfterLog::Returned;
    }
    let standing = session
        .standing
        .filter(|_| session.execution == Execution::Serving)
        .and_then(|frame| read::position(req, frame).map(|position| (frame, position)));
    let (file, line) = standing
        .as_ref()
        .map_or((prompt::NO_ACTIVE_FILE, 0), |(_, position)| {
            (position.file.as_slice(), position.line)
        });
    crate::list::code(&mut session, file, line);
    if !session.started {
        session::put(req, session);
        return AfterLog::Returned;
    }
    // The context's own compile refused it: nothing is held, and the
    // first frame an `ev` at this prompt enters is no program's.
    if session.plan == Some(Plan::Hold) {
        session.plan = Some(Plan::Serve);
        session.exec_compile = ExecCompile::Refused;
    }
    let mut here = match standing {
        Some((frame, _)) => Here::paused(req, frame),
        None if session.exec.is_some() && session.exec_compile != ExecCompile::Refused => {
            Here::Held { req }
        }
        None => Here::Idle { req },
    };
    let outcome = out::in_log(&mut session, |s| prompt::interactive(s, &mut here));
    let after = outcome.after_log(session.stopped.is_some());
    session::put(req, session);
    if after == AfterLog::Returned {
        let _ = crate::watch::changed(req);
    }
    after
}

/// The loop itself. php's `HANDLE_EXCEPTION` over a frame where nothing
/// it takes is pending, what was thrown into the frame at a prompt having
/// been rendered there by `ev`, leaves the frame as a return of nothing:
/// a nested frame's caller goes on past the call, and the frame the
/// serving entered ends the serving, handed back with `Flow::Returned`,
/// the status as `ev` left it.
///
/// ```php
/// $a = 1;
/// $b = 2;
/// $c = 3;
/// echo "x\n";
/// ```
/// ```request phpdbg
/// register intdiv
/// b 3
/// r
/// intdiv 1
/// ev 9
/// c
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Registered intdiv]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003: $b = 2;
///  00004: $c = 3;
///  00005: echo "x\n";
/// prompt>%s
/// prompt>%s
/// Fatal error: Uncaught ArgumentCountError: intdiv() expects exactly 2 arguments, 1 given in %SCRATCH%/witness.php:3
/// Stack trace:
/// #0 %SCRATCH%/witness.php(3): intdiv(1)
/// #1 {main}
///   thrown in %SCRATCH%/witness.php on line 3
/// prompt> [Script ended normally]
/// prompt>
/// ```
fn stepped(req: &mut Request, frame: FrameRef) -> Flow {
    let mut at = Standing {
        frame,
        unwinding: false,
        raised_line: 0,
    };
    loop {
        let Some(position) = read::position(req, at.frame) else {
            return Flow::Resume(at.frame);
        };
        if read::exiting(req) {
            return Flow::Resume(at.frame);
        }
        if at.unwinding {
            at.raised_line = position.line;
        }
        if let Some(stop) = checked(req, &at, &position) {
            return stop;
        }
        if session::with(req, |s| {
            s.thrown_into.take_if(|frame| *frame == at.frame).is_some()
        })
        .unwrap_or(false)
        {
            at.unwinding = true;
            at.raised_line = position.line;
        }
        let entry = tier::entry(req);
        // The unwinder leaves every frame but the serving's entry with
        // what it takes, and the entry too where nothing is pending, as a
        // return of nothing; a throwable the entry does not catch is the
        // loop's to end the serving with.
        let next = if at.unwinding {
            if at.frame != entry || tier::catches(req, at.frame) || read::pending(req).is_none() {
                Next::Unwind
            } else {
                Next::HandBack
            }
        } else if at.frame == entry && opcodes::is(position.opcode, opcodes::LEAVES) {
            Next::HandBack
        } else {
            Next::Step
        };
        session::with(req, |s| s.standing = Some(at.frame));
        let stepped = match next {
            Next::HandBack => return Flow::Resume(at.frame),
            Next::Unwind => tier::unwind(req, at.frame),
            Next::Step => tier::step(req, at.frame),
        };
        if let Ok(edge) = &stepped {
            let vmret = vm_return(req, at.frame, *edge);
            session::with(req, |s| s.vmret = vmret);
        }
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
            Ok(Stepped::Done) => return Flow::Returned,
            Err(fatal) => return Flow::Bail(fatal),
        }
    }
}

/// `PHPDBG_G(vmret)`, what `zend_vm_call_opcode_handler` answered for
/// the handler called from `before`: 0 where the loop stays in the frame,
/// 1 where it entered one whose caller is `before`, 2 where it went to
/// any other, and -1 where the serving ended
/// (`Z/Zend/zend_vm_execute.h`'s hybrid `zend_vm_call_opcode_handler`).
fn vm_return(req: &Request, before: FrameRef, edge: Stepped) -> i32 {
    let (Stepped::Same(after)
    | Stepped::Pushed(after)
    | Stepped::Popped(after)
    | Stepped::Moved(after)
    | Stepped::Threw(after)) = edge
    else {
        return -1;
    };
    if after == before {
        0
    } else if read::entered_from(req, after, before) {
        1
    } else {
        2
    }
}

/// What php checks before a handler runs, in `phpdbg_execute_ex`'s
/// order, and the prompt where one of them stops: `Some` where the
/// prompt ended the request.
fn checked(req: &mut Request, at: &Standing, position: &read::Position) -> Option<Flow> {
    // `PHPDBG_BP_RESOLVE_MASK` (`phpdbg_prompt.c:1650-1653`), first,
    // whatever else the loop is under: the breakpoints set on this op
    // array's oplines resolved, and said so, before each of its oplines.
    if session::with(req, |s| s.breaks.resolves()).unwrap_or(false) {
        let serves = read::serves(req, at.frame);
        let (function, class) = match &serves {
            read::Serves::Code => (None, None),
            read::Serves::Function { name, class } => (Some(name.as_slice()), class.as_deref()),
        };
        let op = OpArray {
            file: &position.file,
            function,
            class,
        };
        session::with(req, |s| s.breaks.resolve(&mut s.out, op, position.oplines));
    }
    // `PHPDBG_IS_INITIALIZING` (`phpdbg_prompt.c:1704-1708`): what the
    // init files serve runs uninterrupted, logged by no oplog, but for
    // the throwable nothing catches, which is checked first. php's `-rr`
    // flag is set when the startup run begins, after the init files.
    if session::with(req, |s| s.initializing).unwrap_or(false) {
        let pending = shown::pending(req);
        if !uncaught(req, at.frame, pending.as_ref()) {
            return None;
        }
        return interactive(req, at.frame, &position.file, at.raised_line);
    }
    // `-rr`'s `PHPDBG_PREVENT_INTERACTIVE` (`phpdbg_prompt.c:1666-1669`):
    // the opline logged and nothing checked, so an uncaught throwable is
    // the run's end to report.
    if session::with(req, |s| s.after_serve == AfterServe::Quit).unwrap_or(false) {
        if !at.unwinding {
            stepped_opline(req, at.frame);
            let serves = read::serves(req, at.frame);
            logged(req, position, &serves);
        }
        return None;
    }
    let pending = shown::pending(req);
    // `zend_get_executed_lineno`: the raising opline's while a throwable
    // is pending, as `zend_clear_exception` puts it back.
    let shown = if pending.is_some() {
        at.raised_line
    } else {
        position.line
    };
    let line = if at.unwinding { 0 } else { position.line };
    let serves = read::serves(req, at.frame);
    let (function, class) = match &serves {
        read::Serves::Code => (None, None),
        read::Serves::Function { name, class } => (Some(name.as_slice()), class.as_deref()),
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
    let stop = uncaught(req, at.frame, pending.as_ref());
    if !at.unwinding {
        if !stop {
            stepped_opline(req, at.frame);
        }
        logged(req, position, &serves);
    }
    let uncaught_here = pending.is_some() && !tier::catches(req, at.frame);
    let checks = Checks {
        frame: at.frame.0,
        line,
        uncaught_here,
    };
    let stops = stop || decide(req, &here, &checks);
    // A report that ran PHP code, a header callback, ended the request
    // where a prompt inside it quit.
    if read::exiting(req) {
        return Some(Flow::Resume(at.frame));
    }
    if !stops {
        session::with(req, |s| s.last_line = line);
        return None;
    }
    let flow = interactive(req, at.frame, &position.file, shown);
    // `next:`'s `PHPDBG_G(last_line)`, after the prompt, where what an
    // `ev` there ran has moved it.
    session::with(req, |s| s.last_line = line);
    flow
}

/// The uncaught throwable, reported once per throwable, outside what
/// `ev` serves: whether the loop stops for it.
fn uncaught(req: &mut Request, frame: FrameRef, pending: Option<&shown::Pending>) -> bool {
    let origin = session::with(req, |s| s.origin).unwrap_or(Origin::Program);
    let Some(thrown) = pending.filter(|_| origin == Origin::Program) else {
        return false;
    };
    let handled = session::with(req, |s| s.handled == Some(thrown.id)).unwrap_or(true);
    if handled || caught_anywhere(req, frame) {
        return false;
    }
    let message = thrown.message.get(..80).unwrap_or(&thrown.message);
    let message = format!(
        "Uncaught {} in {} on line {}: {}",
        String::from_utf8_lossy(&thrown.class),
        String::from_utf8_lossy(&thrown.file),
        thrown.line,
        String::from_utf8_lossy(message)
    );
    session::with(req, |s| {
        s.handled = Some(thrown.id);
        s.out.error(message.as_bytes());
    });
    true
}

/// `phpdbg_print_opline` with the flags read: the opline written where
/// quietness is off and the loop is stepping, every opline it reaches
/// until the step stops, the one it stops at among them.
///
/// ```php
/// function f($x) {
///   $y = $x + 1;
///   return $y;
/// }
/// echo f(1), "\n";
/// echo "a\n";
/// echo "b\n";
/// ```
/// ```request phpdbg
/// set quiet 0
/// b f
/// r
/// n
/// n
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> prompt> [Breakpoint #0 added at f]
/// prompt> [Breakpoint #0 in f() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:   $y = $x + 1;
///  00004:   return $y;
///  00005: }
/// prompt> >00004:   return $y;
///  00005: }
///  00006: echo f(1), "\n";
/// prompt> >00006: echo f(1), "\n";
///  00007: echo "a\n";
///  00008: echo "b\n";
/// prompt> 2
/// a
/// b
/// [Script ended normally]
/// prompt>
/// ```
/// ```stderr
/// L0003 0002 ASSIGN CV1($y) T2
/// L0004 0003 RETURN CV1($y)
/// L0006 0003 ECHO V0
/// ```
fn stepped_opline(req: &mut Request, frame: FrameRef) {
    if session::with(req, |s| !s.quiet && s.stepping).unwrap_or(false) {
        prompt::print_opline_ex(req, frame);
    }
}

/// `phpdbg_print_opline`'s oplog: the opline written to the oplog open
/// last, where one is.
fn logged(req: &mut Request, position: &read::Position, serves: &read::Serves) {
    let (function, class) = match serves {
        read::Serves::Code => (None, None),
        read::Serves::Function { name, class } => (Some(name.clone()), class.clone()),
    };
    session::with(req, |s| {
        if let Some(log) = s.oplogs.last_mut() {
            log.push(session::Logged {
                file: position.file.clone(),
                function,
                class,
                opline: position.opline,
                opcode: position.opcode,
                line: position.line,
            });
        }
    });
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
/// order (`phpdbg_prompt.c:1713-1788`): whether the loop stops here. The
/// watchpoints are checked with the session in the request's sink, since their
/// diff writes through the request's sink and may call PHP code the
/// debugger stops in.
fn decide(req: &mut Request, here: &At<'_>, checks: &Checks) -> bool {
    match session::with(req, |s| moved(s, here, checks)) {
        Some(Some(stops)) => return stops,
        Some(None) => {}
        None => return false,
    }
    if crate::watch::changed(req) {
        return true;
    }
    session::with(req, |s| broken(s, here, checks)).unwrap_or(false)
}

/// The seek and the step: whether they stop the loop here or let it go
/// on, none where the checks after them decide.
fn moved(s: &mut Session, here: &At<'_>, checks: &Checks) -> Option<bool> {
    if let Some(seek) = s.seek.as_ref().filter(|_| s.origin == Origin::Program) {
        if seek.frame != checks.frame {
            if s.stepping {
                s.stepping = false;
                return Some(true);
            }
            return Some(false);
        }
        let reached = seek.oplines.contains(&here.opline) || checks.uncaught_here;
        match seek.kind {
            SeekKind::Until => {
                if !reached {
                    return Some(false);
                }
                s.seek = None;
            }
            SeekKind::Finish => {
                if reached {
                    s.seek = None;
                }
                return Some(false);
            }
            SeekKind::Leave => {
                if !reached {
                    return Some(false);
                }
                s.seek = None;
                s.out.notice(
                    format!(
                        "Breaking for leave at {}:{}",
                        String::from_utf8_lossy(here.file),
                        here.line
                    )
                    .as_bytes(),
                );
                return Some(true);
            }
        }
    }
    if s.stepping && checks.line != s.last_line {
        s.stepping = false;
        return Some(true);
    }
    None
}

/// The breakpoints: whether one stops the loop here.
fn broken(s: &mut Session, here: &At<'_>, checks: &Checks) -> bool {
    let mut at = here.clone();
    at.line = checks.line;
    if let Some(found) = s.breaks.find(&at)
        && (!matches!(s.breaks.list[found].kind, crate::bp::Kind::File { .. })
            || checks.line != s.last_line)
    {
        s.breaks.hit(&mut s.out, found, here);
        return true;
    }
    false
}

/// Whether a user frame of the chain from `frame` down catches the
/// throwable pending, past the natives between them: a throwable a
/// callback raised under `array_map` and the program catches is no
/// uncaught one, and the `run` goes on to the catch.
///
/// ```php
/// function cb() { throw new Exception("x"); }
/// try { array_map("cb", [1]); } catch (Exception $e) { echo "caught\n"; }
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> caught
/// [Script ended normally]
/// prompt>
/// ```
fn caught_anywhere(req: &mut Request, frame: FrameRef) -> bool {
    let mut at = Some(frame);
    while let Some(frame) = at {
        if tier::catches(req, frame) {
            return true;
        }
        at = tier::below(req, frame);
    }
    false
}

/// `DO_INTERACTIVE`: the listing, but where `ev` serves (`list_code`),
/// then the prompt with the throwable set aside; `Some` where the prompt
/// ended the request. Where it goes on, the watches are compared as
/// `phpdbg_interactive` ends (`phpdbg_print_changed_zvals`,
/// `Z/sapi/phpdbg/phpdbg_prompt.c:1582`), and what changed is reported
/// without a stop, as what `ev` wrote to a variable it reached through
/// the frame's table, which the evaluation has given back by then.
///
/// ```php
/// function f() {
///   echo "in f\n";
/// }
/// $x = 1;
/// f();
/// echo "end\n";
/// ```
/// ```request phpdbg
/// b 6
/// r
/// w $x
/// ev $x = 5
/// c
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:6]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:6, hits: 1]
/// >00006: f();
///  00007: echo "end\n";
/// prompt> [Added watchpoint #0 for $x]
/// prompt> 5
/// prompt> [Breaking on watchpoint $x]
/// Old value: 1
/// New value: 5
/// in f
/// end
/// [$x has been removed, removing watchpoint]
/// [Script ended normally]
/// prompt>
/// ``` A breakpoint inside what `ev` serves stops there,
/// at a prompt with no listing.
///
/// ```php
/// function f() {
///   echo "in f\n";
/// }
/// f();
/// ```
/// ```request phpdbg
/// b f
/// ev f()
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at f]
/// prompt> [Breakpoint #0 in f() at %SCRATCH%/witness.php:3, hits: 1]
/// prompt>
/// ```
fn interactive(req: &mut Request, frame: FrameRef, file: &[u8], line: u32) -> Option<Flow> {
    let mut session = session::take(req)?;
    if session.origin == Origin::Program {
        crate::list::code(&mut session, file, line);
    }
    let aside = tier::set_aside(req);
    let outcome = {
        let mut here = Here::paused(req, frame);
        prompt::interactive(&mut session, &mut here)
    };
    let stopped = session.stopped.is_some();
    session::put(req, session);
    if !stopped && outcome != Outcome::Stopped {
        let _ = crate::watch::changed(req);
    }
    tier::restore(req, aside);
    if stopped || outcome == Outcome::Stopped {
        tier::exit(req);
        return Some(Flow::Resume(frame));
    }
    None
}
