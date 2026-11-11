//! The mechanism a tier drives between its steps (`jit-design.md`
//! §2.4, revision 8; design §5.8, revision 51), and what it writes: the
//! serving's entry frame and the frames below it on the stack a step
//! hands out, whether a frame takes what is pending, and the code a
//! tier has the engine serve on its own account. What a frame is, its
//! function, file, line, op array and variables, a tier reads through
//! the native face ([`frame`]), as an extension reads it (#1911).

use std::os::fd::OwnedFd;

use php_heap::FrameRef;
use php_unit::{CacheSlot, Opcode};

use crate::api::request::{FrameView, Raised, Request};
use crate::exec;

/// The frame a safepoint or a step handed out, read through the native
/// face as an extension reads one ([`FrameView`]); none for a position
/// no frame stands at.
#[must_use]
pub fn frame(req: &Request, frame: FrameRef) -> Option<FrameView<'_>> {
    req.vm().at(frame)?;
    Some(req.frame_view(frame))
}

/// The entry frame of the innermost serving: the frame whose return
/// ends the `execute` a tier was asked in, which pops to no caller, so
/// `Site::Return` is never raised for its return.
#[must_use]
pub fn entry(req: &Request) -> FrameRef {
    req.stack().entry()
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
    let under = req.vm().below(frame.0 as usize)?;
    Some(FrameRef(u32::try_from(under).ok()?))
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
    let pending = match req.diagnostics().raised() {
        Some(Raised::Throwable(object)) => Some(object),
        Some(Raised::GracefulExit) => None,
        _ => return false,
    };
    let Some(serving) = req.serving(frame) else {
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
                    .is_some_and(|of| req.symbols().instance_of(pending.class(), of))
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

/// The observer's begin of the frame a serving's `execute` entered, which
/// a tier that holds that frame before executing it raises where it
/// executes it ([`super::Tier::HOLDS`]), as php's `zend_execute` raises
/// `ZEND_OBSERVER_FCALL_BEGIN` before `zend_execute_ex`
/// (`Z/Zend/zend_vm_execute.h:121973`); phpdbg's `run` is that
/// `zend_execute` over the program it compiled and held
/// (`Z/sapi/phpdbg/phpdbg_prompt.c:866`).
pub fn begin(req: &mut Request, frame: FrameRef) {
    crate::observer::call(req, frame);
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

/// What stands pending on the request, set aside while a tier serves
/// something of its own on the request, and put back after.
#[derive(Debug)]
pub struct Aside(Option<Raised>);

/// The pending throwable or exit taken off the request, as phpdbg's
/// `DO_INTERACTIVE` clears `EG(exception)` for its prompt.
pub fn set_aside(req: &mut Request) -> Aside {
    Aside(req.take_raised())
}

/// What was set aside put back, and what the request had pending
/// meanwhile given up for it, as `DO_INTERACTIVE` puts back the
/// throwable it cleared (`Z/sapi/phpdbg/phpdbg_prompt.c:1596-1623`);
/// where nothing was set aside, what the tier's own serving left pending
/// stays, as php's prompt leaves `EG(exception)` where it found none.
// Taking the receipt by value is the point: what was set aside is put
// back once.
#[allow(clippy::needless_pass_by_value)]
pub fn restore(req: &mut Request, aside: Aside) {
    if aside.0.is_none() {
        return;
    }
    if let Some(Raised::Throwable(object)) = req.take_raised() {
        req.release(object.as_value());
    }
    req.set_raised(aside.0);
}

/// What an evaluation served: the value it returned as `print_r`
/// renders it, or nothing, what it left uncaught having been rendered;
/// from [`evaluate_code`], which asks for no value and renders nothing,
/// `Rendered` says the code was served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluated {
    Printed(Vec<u8>),
    Rendered,
    /// `zend_eval_stringl`'s `FAILURE`: the code did not compile, and the
    /// `ParseError` or `CompileError` the compiler threw is left pending,
    /// unrendered, for what is served next to find.
    NotCompiled,
    /// A fatal bailed out of what it served, rendered where it was
    /// raised, and the evaluation caught it as `ev`'s `zend_catch` does:
    /// the request goes on with the status 0.
    BailedOut,
}

/// How a compile a tier asked for ended ([`compile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compiled {
    /// It compiled; what the file declares is the request's.
    Compiled,
    /// The compiler refused it, what it raised rendered and the jump its
    /// fatal took caught.
    Refused,
    /// The file did not open, and nothing was compiled.
    Unopened,
}

/// The file at `path`, or `source` under that name where the host read
/// it, compiled as phpdbg's `phpdbg_compile` compiles the program it
/// debugs, `zend_compile_file` with nothing executing, and dropped: what
/// the compiler raised rendered as php raises it, and a refusal's fatal
/// caught as the `zend_try` around phpdbg's compile catches it, the
/// request standing with what php leaves after it. During `startup`, a
/// file the `open_basedir` guard refuses is warned of as php warns while
/// the request starts.
pub fn compile(
    req: &mut Request,
    path: &[u8],
    source: Option<&[u8]>,
    startup: Startup,
) -> Compiled {
    req.compile_context(path, source, startup)
}

/// Where a host stands to its request's startup as it compiles,
/// `PG(during_request_startup)`, which phpdbg clears once it is fully
/// started (`Z/sapi/phpdbg/phpdbg.c:1573`): what no function raises is
/// named for the request's startup until then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    /// The request is still starting.
    During,
    /// The host is fully started.
    Over,
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
/// what `under` names; a fatal it raises is caught, as `ev`'s
/// `zend_catch` catches it.
pub fn evaluate(req: &mut Request, under: Under, code: &[u8]) -> Evaluated {
    req.evaluate_in(under, code)
}

/// `code` evaluated as phpdbg evaluates an init file's code block,
/// `zend_eval_stringl` with no value asked for: compiled as an `eval`'s
/// statements under `name`, with nothing executing, and served by the
/// engine's loop. [`Evaluated::Rendered`] says it was served, and what it
/// left uncaught stays pending; [`Evaluated::NotCompiled`] that it did
/// not compile, what the compiler threw pending or the fatal it raised
/// caught; [`Evaluated::BailedOut`] that a fatal or an exit it raised
/// while served was caught, with the status it wrote and the exit left
/// pending. With something pending already, nothing is served.
pub fn evaluate_code(req: &mut Request, name: &[u8], code: &[u8]) -> Evaluated {
    req.evaluate_code_in(name, code)
}

/// An argument a host passes a function by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Argument {
    Int(i64),
    Str(Vec<u8>),
}

/// How a call a tier made ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Called {
    /// It returned, or raised a fatal, which is the request's.
    Returned,
    /// Made from a frame, it left a throwable uncaught, which stays
    /// pending on the request as `zend_call_function` leaves
    /// `EG(exception)` with a frame executing; that frame stands where
    /// php's `zend_throw_exception_internal` leaves it, at
    /// `EG(exception_op)`, so it unwinds when the tier goes on in it.
    Threw,
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
/// up; a throwable it leaves stays pending from a frame and is rendered
/// uncaught with nothing executing; a fatal it raised is the request's,
/// which ends with it.
pub fn call(req: &mut Request, under: Under, name: &[u8], arguments: &[Argument]) -> Called {
    req.call_by_name(under, name, arguments)
}

/// `SG(request_info).argv` replaced in the request standing, and `$argv`,
/// `$argc` and a made `$_SERVER`'s rebuilt from it, as phpdbg's `run`
/// with arguments does before it executes (`php_build_argv`,
/// `Z/sapi/phpdbg/phpdbg_prompt.c:830-838`); a write that meets the
/// memory limit leaves what it had not reached.
pub fn set_argv(req: &mut Request, argv: Vec<Vec<u8>>) {
    req.build_argv(argv).ok();
}

/// `stdin` the program's stdin in the request standing, `STDIN`,
/// `STDOUT` and `STDERR` registered again over it, as phpdbg's `run
/// <file` does before it executes (`phpdbg_register_file_handles`,
/// `Z/sapi/phpdbg/phpdbg_prompt.c:790-796`); a host that asked for no
/// handles has nothing replaced.
pub fn set_stdin(req: &mut Request, stdin: OwnedFd) {
    req.reregister_file_handles(stdin).ok();
}

#[cfg(test)]
mod tests {
    //! The face as a debugging host drives it: the shared stepping double
    //! ([`Stepper`]), which takes the serving over at its entry and steps
    //! it one handler at a time, unwinding a frame that catches what it
    //! threw and handing the rest back, as `sapi/phpdbg` does.

    use php_heap::FrameRef;

    use crate::api::abi::{Extension, ExtensionBuilder};
    use crate::api::request::{Raised, Request};
    use crate::engine::Engine;
    use crate::tests::stepped::{Stepper, Stepping};
    use crate::tier::{Flow, Site, Tier};

    /// The class of the throwable pending, and the `file` and `line` it
    /// carries, read through the native face as phpdbg reads them off it.
    fn described(req: &Request) -> Option<(String, String, i64)> {
        let Some(Raised::Throwable(object)) = req.diagnostics().raised() else {
            return None;
        };
        let prop = |name: &[u8]| object.prop(req, name);
        let file = prop(b"file")
            .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default();
        let line = prop(b"line").and_then(|v| v.as_int()).unwrap_or(0);
        Some((
            String::from_utf8_lossy(req.symbols().class_name(object.class())).into_owned(),
            String::from_utf8_lossy(&file).into_owned(),
            line,
        ))
    }

    /// What the evaluating tier read of every evaluation it served, and
    /// whether it has taken the request over: its request state, read
    /// back by the test once the request ended ([`kept`]).
    #[derive(Default)]
    struct Seen {
        lines: Vec<String>,
        begun: bool,
    }

    /// The extension a test tier declares its state through.
    fn seeing() -> Extension {
        ExtensionBuilder::default()
            .request_state::<Seen>()
            .build("seeing")
    }

    /// What `source` echoed served on `engine`, and the request state of
    /// type `T` its tier kept ([`crate::tier::serve_kept`]).
    fn kept<T: Send + 'static>(engine: &Engine, source: &str) -> (String, T) {
        let dir = std::env::current_dir().expect("the test has a working directory");
        let (served, kept) =
            crate::tier::serve_kept::<T>(engine, &dir, b"Standard input code", source);
        let kept = kept.expect("the tier declared its state, and the request built it");
        (served.stdout_text(), kept)
    }

    /// What `source` echoed under the shared debugging double
    /// ([`Stepper`]), and where it stood before each handler.
    fn stepped(source: &str) -> (String, Vec<String>) {
        let engine = Engine::cli_builder()
            .tier::<Stepper<false, false, true>>()
            .build();
        let (out, stepping) = kept::<Stepping>(&engine, source);
        (out, stepping.seen)
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

    /// The code the tier evaluates at the program's first entry, which
    /// the test registers with the engine.
    struct Code(Vec<&'static [u8]>);

    struct Initializer;

    impl Tier for Initializer {
        type State = ();

        fn safepoint(req: &mut Request, _: FrameRef, site: Site, _: crate::tier::Held<'_>) -> Flow {
            if site != Site::Entry || std::mem::replace(&mut req.state::<Seen>().begun, true) {
                return Flow::Continue;
            }
            let code = req
                .registered::<Code>()
                .flat_map(|code| code.0.iter().copied())
                .collect::<Vec<_>>();
            for code in code {
                let evaluated = super::evaluate_code(req, b"phpdbginit code", code);
                let pending =
                    described(req).map(|(class, file, line)| format!(" {class} in {file}:{line}"));
                let seen = format!("{evaluated:?}{}", pending.unwrap_or_default());
                req.state::<Seen>().lines.push(seen);
            }
            Flow::Continue
        }

        fn extension() -> Option<Extension> {
            Some(seeing())
        }
    }

    fn initialized(code: &[&'static [u8]], source: &str) -> (String, Vec<String>) {
        let code = ExtensionBuilder::default()
            .register(Code(code.to_vec()))
            .build("init");
        let engine = Engine::cli_builder()
            .tier::<Initializer>()
            .extensions(&[code])
            .build();
        let (out, seen) = kept::<Seen>(&engine, source);
        (out, seen.lines)
    }

    /// Statements evaluated under a name of the host's before the
    /// program's first opline, as phpdbg evaluates an init file's code
    /// block: no value printed, what they define the program's, and what
    /// they leave uncaught, or the compiler threw, pending under that name,
    /// with nothing served after it.
    #[test]
    fn a_tier_evaluates_code_under_its_own_name() {
        let (out, seen) = initialized(&[b"echo 'a'; $x = 5;"], "<?php\necho $x;\n");
        assert_eq!(
            (out.as_str(), seen.as_slice()),
            ("a5", ["Rendered".to_owned()].as_slice())
        );
        let (_, seen) = initialized(
            &[b"throw new Exception('x');", b"echo 'not served';"],
            "<?php\n",
        );
        assert_eq!(
            seen,
            [
                "Rendered Exception in phpdbginit code:1",
                "Rendered Exception in phpdbginit code:1"
            ]
        );
        let (_, seen) = initialized(&[b"$y ="], "<?php\n");
        assert_eq!(seen, ["NotCompiled ParseError in phpdbginit code:1"]);
    }
}
