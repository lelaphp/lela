//! The frame reads a tier makes between the steps it drives
//! (`jit-design.md` §2.4, revision 8; design §5.8, revision 51): where a
//! frame stands and what it serves, the frame below it, the serving's
//! entry frame, and the throwable it has pending. Each is a copy taken
//! when asked, so a tier holds no borrow of the request across the next
//! [`step`](super::step), and none names a thing the engine does not
//! already hold for a diagnostic or a backtrace.

use php_heap::{Fatal, FrameRef};
use php_unit::{CacheSlot, Opcode};

use crate::api::request::{Raised, Request};
use crate::exec;

/// Where a frame stands: the file and line of the opline the loop
/// dispatches next, its index in the op array, its opcode, and the lines
/// the op array spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub file: Vec<u8>,
    pub line: u32,
    pub opline: u32,
    pub opcode: Opcode,
    /// `op_array->line_start` and `line_end`.
    pub lines: (u32, u32),
    /// `op_array->last`, how many oplines the op array holds.
    pub oplines: u32,
    /// The first opline past the `RECV*` oplines, `num_args` and one more
    /// for a variadic: where a function's body begins.
    pub body: u32,
}

/// What a frame serves: a function or a method by the name it was
/// declared under, with the class it was declared in, or the code of a
/// file or an `eval`, which is no function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Serves {
    /// A file's or an `eval`'s own code, php's `op_array->function_name`
    /// being `NULL`.
    Code,
    /// A function, a closure or a method: `function_name`, and `scope`'s
    /// name where it has one.
    Function {
        name: Vec<u8>,
        class: Option<Vec<u8>>,
    },
}

/// The entry frame of the innermost serving: the frame whose return
/// ends the `execute` a tier was asked in, as `Site::Return` is never
/// raised for it.
#[must_use]
pub fn entry(req: &Request) -> FrameRef {
    req.stack().entry()
}

/// `EX(prev_execute_data)` where it is user code: the frame an opline of
/// which pushed `frame`, or which the loop stood in when a native or the
/// engine served it; none for the bottom of the chain.
#[must_use]
pub fn caller(req: &Request, frame: FrameRef) -> Option<FrameRef> {
    req.vm().at(frame).and_then(|state| state.caller.frame())
}

/// The next user frame `EX(prev_execute_data)`'s chain reaches from
/// `frame`, past the frame of any native between the two: the frame
/// below it on the request's stack, which a native that called back into
/// PHP code stands over, as `array_map` over its callback; none for the
/// bottom of the stack. What phpdbg walks for a catch of the throwable
/// pending before it reports it uncaught, skipping each frame that is no
/// user code
/// (`Z/sapi/phpdbg/phpdbg_prompt.c:1672-1684`).
#[must_use]
pub fn below(req: &Request, frame: FrameRef) -> Option<FrameRef> {
    let under = FrameRef(frame.0.checked_sub(1)?);
    req.vm().at(under).map(|_| under)
}

/// Where `frame` stands; none for a frame that is not standing.
#[must_use]
pub fn position(req: &Request, frame: FrameRef) -> Option<Position> {
    let state = req.vm().at(frame)?;
    let code = exec::body_of(&state.unit, state.body)?;
    let op = exec::opline_at(code, state.opline)?;
    Some(Position {
        file: code.filename.0.to_vec(),
        line: op.lineno,
        opline: state.opline,
        opcode: op.opcode,
        lines: (code.line_start, code.line_end),
        oplines: u32::try_from(code.oplines.len()).unwrap_or(u32::MAX),
        body: code.safepoints.first().map_or(0, |at| at.0),
    })
}

/// Each opline of the op array `frame` serves, its opcode and its line,
/// in order: what phpdbg's `until`, `next`, `finish` and `leave` read to
/// know where a seek ends.
#[must_use]
pub fn oplines(req: &Request, frame: FrameRef) -> Vec<(Opcode, u32)> {
    req.vm()
        .serving(frame)
        .and_then(|serving| {
            serving.code().map(|code| {
                code.oplines
                    .iter()
                    .map(|op| (op.opcode, op.lineno))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// What `frame` serves; a file's code for a frame that is not standing.
#[must_use]
pub fn serves(req: &Request, frame: FrameRef) -> Serves {
    let Some(function) = req.vm().at(frame).and_then(|state| state.function) else {
        return Serves::Code;
    };
    let class = req
        .frame_scope(frame)
        .map(|class| req.class_name(class).into_bytes());
    Serves::Function {
        name: req.declared_name(function).into_bytes(),
        class,
    }
}

/// The throwable pending on the request as php's phpdbg reads it off
/// the object (`phpdbg_prompt.c:1690-1696`): its class, and its `file`,
/// `line` and `message` properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// `spl_object_id`, which tells two throwables apart.
    pub id: u32,
    pub class: Vec<u8>,
    pub file: Vec<u8>,
    pub line: i64,
    pub message: Vec<u8>,
}

/// The throwable pending on the request; none while nothing is, or an
/// exit is what stands. The graceful exit a destroyed fiber is resumed
/// with is one, as php's is an object of class `GracefulExit` with no
/// file, line or message.
#[must_use]
pub fn pending(req: &Request) -> Option<Pending> {
    if req.raised() == Some(Raised::GracefulExit) {
        return Some(Pending {
            id: u32::MAX,
            class: b"GracefulExit".to_vec(),
            file: Vec::new(),
            line: 0,
            message: Vec::new(),
        });
    }
    let object = req.pending()?;
    Some(Pending {
        id: object.index(),
        class: req.class_name(object.class()).into_bytes(),
        file: req.throwable_prop_bytes(object, b"file"),
        line: req.throwable_prop_int(object, b"line"),
        message: req.throwable_prop_bytes(object, b"message"),
    })
}

/// Whether `frame` takes the throwable pending on the request, as
/// phpdbg's `phpdbg_check_caught_ex` asks it before the unwinder runs
/// (`Z/sapi/phpdbg/phpdbg_utils.c:603`): the innermost `try` element
/// covering the opline the frame stands at has a `finally` block, or a
/// `catch` clause whose class the throwable is an instance of; or the
/// frame stands at a `CATCH` already.
#[must_use]
pub fn catches(req: &mut Request, frame: FrameRef) -> bool {
    // The graceful exit is no class a `catch` names; a `finally` block
    // takes it still.
    let pending = match req.raised() {
        Some(Raised::Throwable(object)) => Some(object),
        Some(Raised::GracefulExit) => None,
        _ => return false,
    };
    let Some(serving) = req.vm().serving(frame) else {
        return false;
    };
    let Some(code) = serving.code() else {
        return false;
    };
    let op_num = req.vm().opline(frame);
    for element in &code.try_catch {
        if element.try_op.0 > op_num {
            break;
        }
        let (catch, finally) = (element.catch_op.0, element.finally_op.0);
        if op_num > catch && op_num > finally {
            continue;
        }
        if finally != 0 {
            return true;
        }
        let mut at = catch;
        while let Some(op) = exec::opline_at(code, at) {
            let slot = Some(CacheSlot(op.extended_value & !exec::LAST_CATCH));
            if let Some(pending) = pending
                && exec::cached_class(req, frame, code, op.op1, slot)
                    .is_some_and(|of| req.instance_of(pending.class(), of))
            {
                return true;
            }
            if op.extended_value & exec::LAST_CATCH != 0 {
                return false;
            }
            at = op.op2;
        }
        return false;
    }
    exec::opline_at(code, op_num).is_some_and(|op| op.opcode == Opcode::Catch)
}

/// The unwind exit, standing where a throwable would, as `exit` leaves
/// it: a tier that hands the frame back with `Flow::Resume` has the loop
/// unwind every frame of the serving without entering a `finally`
/// block, and the request ends as an `exit` ends it, its shutdown
/// functions and destructors served.
pub fn exit(req: &mut Request) {
    if let Some(Raised::Throwable(object)) = req.take_raised() {
        req.release(object.as_value());
    }
    req.throw_unwind_exit();
}

/// Whether `exit`'s unwind exit is what the request is unwinding, or a
/// fatal is leaving: nothing a tier stops at.
#[must_use]
pub fn exiting(req: &Request) -> bool {
    req.is_pending() && req.pending().is_none() && req.raised() != Some(Raised::GracefulExit)
}

/// What stands pending on the request, set aside while a tier serves
/// something of its own on the request, and put back after.
#[derive(Debug)]
pub struct Aside(Option<Raised>);

/// The pending throwable or exit taken off the request, as phpdbg's
/// `DO_INTERACTIVE` clears `EG(exception)` for its prompt.
pub fn set_aside(req: &mut Request) -> Aside {
    Aside(req.take_raised())
}

/// What was set aside put back; what the request had pending meanwhile
/// is given up for it.
// Taking the receipt by value is the point: what was set aside is put
// back once.
#[allow(clippy::needless_pass_by_value)]
pub fn restore(req: &mut Request, aside: Aside) {
    if let Some(Raised::Throwable(object)) = req.take_raised() {
        req.release(object.as_value());
    }
    req.set_raised(aside.0);
}

/// What an evaluation served: the value it returned as `print_r`
/// renders it, or nothing, what it left uncaught having been rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluated {
    Printed(Vec<u8>),
    Rendered,
}

/// What code a tier has the engine serve on its own account stands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Under {
    /// The frame the tier stopped in, the request's innermost: evaluated
    /// code shares its scope, and a function is called from it.
    Frame(FrameRef),
    /// No frame: the request's frames set aside while the code is
    /// served, as php's `EG(current_execute_data)` is `NULL` before
    /// `zend_execute` has begun the program. Evaluated code runs in the
    /// global scope, what it raises names no active file but its own,
    /// and a trace of it ends at `{main}`.
    Nothing,
}

/// `code` evaluated as phpdbg's `ev` evaluates it, `return code;`
/// compiled as the `eval()'d code` and served by the engine's loop on
/// what `under` names; the `Err` is a fatal, which ends the request.
pub fn evaluate(req: &mut Request, under: Under, code: &[u8]) -> Result<Evaluated, Fatal> {
    req.evaluate_in(under, code)
}

/// An argument a host passes a function by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Argument {
    Int(i64),
    Str(Vec<u8>),
}

/// Whether the request's function table holds a function of this name.
#[must_use]
pub fn function_exists(req: &Request, name: &[u8]) -> bool {
    crate::api::call::lookup_function(req, name).is_some()
}

/// How a call a tier made ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Called {
    /// It returned, or raised a fatal, which is the request's.
    Returned,
    /// What it left uncaught was rendered as `zend_exception_error`
    /// renders it at `E_ERROR`, or the fatal it raised was, and the
    /// fatal was caught where the tier asked, as php's `zend_try` there
    /// catches it.
    Rendered,
    /// It called `exit`, with nothing executing, which was caught where
    /// the tier asked.
    Exited,
}

/// The function named `name` called with `arguments` on what `under`
/// names, as `zend_call_known_function` calls it, what it returns given
/// up; a throwable it leaves is rendered uncaught with `E_DONT_BAIL`; a
/// fatal it raised is the request's, which ends with it.
pub fn call(req: &mut Request, under: Under, name: &[u8], arguments: &[Argument]) -> Called {
    req.call_by_name(under, name, arguments)
}

#[cfg(test)]
mod tests {
    //! The face as a debugging host drives it: a tier that takes the
    //! serving over at its entry and steps it one handler at a time,
    //! unwinding a frame that catches what it threw and handing the rest
    //! back, as `sapi/phpdbg` does.

    use std::cell::RefCell;

    use php_heap::FrameRef;

    use crate::api::request::Request;
    use crate::engine::Engine;
    use crate::test::Capture;
    use crate::tier::{Flow, Site, Stepped, Tier};

    thread_local! {
        /// The line of every opline the tier stood at, and what it read.
        static SEEN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    struct Stepper;

    impl Tier for Stepper {
        fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
            if site != Site::Entry || frame != super::entry(req) {
                return Flow::Continue;
            }
            let mut at = frame;
            let mut unwinding = false;
            loop {
                let Some(position) = super::position(req, at) else {
                    return Flow::Resume(at);
                };
                let what = match super::pending(req) {
                    Some(pending) if unwinding => {
                        format!(
                            "{} threw {}",
                            position.line,
                            String::from_utf8_lossy(&pending.class)
                        )
                    }
                    _ => format!("{} {}", position.line, position.opcode.name()),
                };
                SEEN.with_borrow_mut(|seen| seen.push(what));
                let stepped = if unwinding {
                    if at == super::entry(req) && !super::catches(req, at) {
                        return Flow::Resume(at);
                    }
                    crate::tier::unwind(req, at)
                } else if at == super::entry(req) && position.opcode == php_unit::Opcode::Return {
                    return Flow::Resume(at);
                } else {
                    crate::tier::step(req, at)
                };
                match stepped {
                    Ok(Stepped::Threw(frame)) => {
                        at = frame;
                        unwinding = true;
                    }
                    Ok(
                        Stepped::Same(frame)
                        | Stepped::Pushed(frame)
                        | Stepped::Popped(frame)
                        | Stepped::Moved(frame),
                    ) => {
                        at = frame;
                        unwinding = false;
                    }
                    Ok(Stepped::Done) => return Flow::Continue,
                    Err(fatal) => return Flow::Bail(fatal),
                }
            }
        }
    }

    fn stepped(source: &str) -> (String, Vec<String>) {
        SEEN.with_borrow_mut(Vec::clear);
        let engine = Engine::builder().tier::<Stepper>().build();
        let served = engine.serve_capture(source, Capture::default());
        (
            String::from_utf8_lossy(&served.stdout).into_owned(),
            SEEN.with_borrow_mut(std::mem::take),
        )
    }

    /// Every opline of the main op array and of the function it calls
    /// is seen, one handler a step, the callee's from its first body
    /// opline, and what the program echoes is what the loop alone makes.
    #[test]
    fn a_tier_steps_the_serving_one_handler_at_a_time() {
        let (out, seen) = stepped("<?php\nfunction f($a) {\n  return $a + 1;\n}\necho f(1);\n");
        assert_eq!(out, "2");
        assert_eq!(
            seen,
            [
                "5 ZEND_INIT_FCALL",
                "5 ZEND_SEND_VAL",
                "5 ZEND_DO_UCALL",
                "3 ZEND_ADD",
                "3 ZEND_RETURN",
                "5 ZEND_ECHO",
                "6 ZEND_RETURN",
            ]
        );
    }

    /// A throwable a callee leaves is unwound frame by frame, the
    /// callee left and the caller's `catch` landed at, and the handler
    /// that took it goes on.
    #[test]
    fn a_tier_unwinds_what_a_step_threw_into_the_frame_that_catches_it() {
        let (out, seen) = stepped(
            "<?php\nfunction f() {\n  throw new Exception('x');\n}\ntry { f(); } catch (Exception $e) { echo $e->getMessage(); }\n",
        );
        assert_eq!(out, "x");
        assert!(
            seen.contains(&String::from("3 threw Exception")),
            "{seen:?}"
        );
        assert!(
            seen.contains(&String::from("5 threw Exception")),
            "{seen:?}"
        );
        assert!(seen.contains(&String::from("5 ZEND_CATCH")), "{seen:?}");
    }
}
