//! What the tier face does to a request between steps
//! ([`crate::tier`]): the code and the calls a tier has the engine serve
//! on its own account, the stack a serving's entry slot stands on, and
//! the uncaught throwable kept for a host that renders it. What a frame,
//! an op array or a linked table holds, a tier reads through the native
//! face (#1911).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

use php_compile::SourceKind;
use php_heap::{Fatal, FrameRef};
use php_unit::PublishedUnit;

use super::Request;
use crate::api::raising::{Level, Thrown};
use crate::api::values::{PhpObject, Value};
use crate::exec::TableId;
use crate::tier::{Called, Compiled, Evaluated, Under};

/// What [`Request::eval_returning`] did with a string.
pub(super) enum Evaluation {
    /// It did not compile; what the compiler threw is pending.
    NotCompiled,
    /// It compiled, and what its declarations raised refused it.
    Refused,
    /// It was served, and what it returned is in `returned`.
    Served,
}

/// The uncaught throwable kept for a host that renders it, whose fields no file but this one
/// reads.
#[derive(Default)]
pub(super) struct Inspected {
    /// What the program left uncaught, kept for a host that renders it.
    kept_uncaught: Option<crate::engine::Uncaught>,
}

impl Request {
    /// The VM stack the request runs on, read.
    pub(crate) fn stack(&self) -> &crate::exec::Stack {
        &self.vm.stack
    }

    /// A property of a throwable as its bytes, `zval_get_string` over
    /// `zend_read_property_ex(..., silent)`: the empty string where it
    /// holds no string.
    pub(crate) fn throwable_prop_bytes(&self, object: PhpObject, name: &[u8]) -> Vec<u8> {
        object
            .prop(self, name)
            .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default()
    }

    /// A property of a throwable as a number, `zval_get_long`: zero where
    /// it holds no int.
    pub(crate) fn throwable_prop_int(&self, object: PhpObject, name: &[u8]) -> i64 {
        object
            .prop(self, name)
            .and_then(|value| value.as_int())
            .unwrap_or(0)
    }

    /// phpdbg's `ev` (`Z/sapi/phpdbg/phpdbg_prompt.c:922`) over
    /// `zend_eval_stringl(code, &retval, "eval()'d code")`: `return code;`
    /// compiled as an `eval` under that name and served by the loop bound
    /// to the engine, in a frame's scope or, with nothing executing, at
    /// the top of the request ([`Request::nothing_executing`]); then what
    /// it returned as `print_r` renders it, or what it left uncaught
    /// rendered as `zend_exception_error` renders it at `E_ERROR`, the
    /// request going on either way. Code that does not compile renders
    /// nothing, `zend_eval_stringl`'s `FAILURE`, which `ev` renders
    /// nothing after, and what the compiler threw stays pending, so the
    /// next evaluation renders it. A fatal, the compiler's among them,
    /// is caught by `ev`'s `zend_catch`
    /// (`Z/sapi/phpdbg/phpdbg_prompt.c:962-972`), which writes the status
    /// 0, and the request goes on as it stood, with the ending it had.
    pub(crate) fn evaluate_in(&mut self, under: Under, code: &[u8]) -> Evaluated {
        // `zend_execute` serves nothing while something is pending
        // (`Z/Zend/zend_vm_execute.h:121953`), so phpdbg's `ev` finds it
        // still pending after `zend_eval_stringl` and renders it.
        if self.is_pending() {
            return self.evaluation_uncaught();
        }
        let ending = self.ending.take();
        let served = match under {
            Under::Frame(frame) => self.evaluate_over(Some(frame), code),
            Under::Nothing => self.nothing_executing(|req| req.evaluate_over(None, code)),
        };
        self.ending = ending;
        match served {
            Ok(evaluated) if !self.diagnostics().bailing_out() => evaluated,
            _ => {
                self.catch_bailout();
                self.exit_status = 0;
                Evaluated::BailedOut
            }
        }
    }

    /// phpdbg's init file's code block (`Z/sapi/phpdbg/phpdbg_prompt.c:206-214`)
    /// over `zend_eval_stringl(code, len, NULL, name)`: `code` compiled as
    /// an `eval`'s statements under `name`, with no `return` around it, and
    /// served by the loop bound to the engine at the top of the request
    /// ([`Request::nothing_executing`]). What it leaves uncaught stays
    /// pending, as php's `EG(exception)` stands after `zend_execute`, and so
    /// does what the compiler threw, `zend_eval_stringl`'s `FAILURE`; with
    /// something pending already, nothing is served. A fatal, the
    /// compiler's or one served, or an exit, at which phpdbg's loop bails
    /// out, leaves the code as `zend_bailout` leaves for the `zend_try`
    /// around phpdbg's init, which catches it here: the status stays what
    /// the fatal or the exit wrote, and the exit stays pending.
    pub(crate) fn evaluate_code_in(&mut self, name: &[u8], code: &[u8]) -> Evaluated {
        if self.is_pending() {
            return Evaluated::Rendered;
        }
        let ending = self.ending.take();
        let served = self.nothing_executing(|req| req.code_over(name, code));
        self.ending = ending;
        match served {
            Ok(Evaluated::NotCompiled) => {
                self.catch_bailout();
                Evaluated::NotCompiled
            }
            Ok(evaluated) if !self.diagnostics().bailing_out() && !self.unwind_exit() => evaluated,
            _ => {
                self.catch_bailout();
                Evaluated::BailedOut
            }
        }
    }

    /// phpdbg's `phpdbg_compile` (`Z/sapi/phpdbg/phpdbg_prompt.c:534-560`):
    /// the file at `path`, or `source` under its name where the host read
    /// it itself, compiled as `zend_compile_file` compiles it,
    /// with nothing executing, and dropped. What the compile raises is
    /// recorded and raised as php raises it, the refusal's fatal through
    /// the host's log and display, and its `zend_bailout` caught as phpdbg's
    /// `zend_try` around the compile catches it, the status 255 left
    /// and the request standing; what php leaves behind it stands too:
    /// the record ([`Request::emit_recorded`]) and the functions the
    /// compile had declared ([`Request::declare_refused`]).
    pub(crate) fn compile_context(&mut self, path: &[u8], source: Option<&[u8]>) -> Compiled {
        let read;
        let source = match source {
            Some(source) => source,
            None => match std::fs::read(self.environment().cwd().join(OsStr::from_bytes(path))) {
                Ok(bytes) => {
                    read = bytes;
                    &read
                }
                Err(_) => return Compiled::Unopened,
            },
        };
        let ending = self.ending.take();
        let compiled = self.nothing_executing(|req| req.compile_context_over(path, source));
        self.ending = ending;
        if self.diagnostics().bailing_out() {
            self.catch_bailout();
            return Compiled::Refused;
        }
        compiled
    }

    /// The compile itself, its unit the request's alone, with its names.
    fn compile_context_over(&mut self, path: &[u8], source: &[u8]) -> Compiled {
        self.take_shared_hold();
        let shared = std::sync::Arc::clone(&self.shared);
        let compiled = shared.compile_for(self, source, path, SourceKind::File);
        match compiled {
            Ok(unit) => {
                crate::engine::note_compiled(self, &unit, SourceKind::File);
                Compiled::Compiled
            }
            Err(refused) => {
                let recording = self.begin_recording();
                // A parse error, and a refusal php throws, leave
                // `zend_compile` as it returns, having declared nothing,
                // so `compile_file` emits what it recorded before phpdbg
                // renders the throwable and bails out; a compile error
                // bails out of the compile itself, past the emit, and
                // after what it had declared.
                let error = &refused.error;
                let thrown = !error.bailed() || error.stage == php_compile::Stage::Opening;
                if let Some(noted) = self.declare_refused(&refused, path) {
                    for raised in error.raised.get(noted..).unwrap_or_default() {
                        self.note_error_in(
                            raised.level,
                            raised.message.as_bytes(),
                            path,
                            raised.line,
                        );
                    }
                    if thrown {
                        self.emit_recorded(recording);
                    }
                    crate::engine::refusal_raised(self, error, path);
                }
                self.emit_recorded(recording);
                Compiled::Refused
            }
        }
    }

    /// The code block itself, on a frame of its own over the global table.
    fn code_over(&mut self, name: &[u8], code: &[u8]) -> Result<Evaluated, Fatal> {
        let Some(unit) = self.compiled_eval(code, name) else {
            return Ok(Evaluated::NotCompiled);
        };
        if !crate::engine::note_compiled(self, &unit, SourceKind::EVAL)
            || self.diagnostics().bailing_out()
        {
            return Ok(Evaluated::NotCompiled);
        }
        self.serve_eval(None, unit)?;
        let returned = std::mem::take(&mut self.returned);
        self.release(returned);
        Ok(Evaluated::Rendered)
    }

    /// The evaluation itself, in `scope`'s frame or with none
    /// ([`Request::eval_returning`]); then what it returned printed, or
    /// what it left rendered as `ev` renders it.
    fn evaluate_over(&mut self, scope: Option<FrameRef>, code: &[u8]) -> Result<Evaluated, Fatal> {
        match self.eval_returning(scope, code, b"eval()'d code")? {
            Evaluation::NotCompiled => return Ok(Evaluated::NotCompiled),
            Evaluation::Refused => return Ok(self.evaluation_uncaught()),
            Evaluation::Served => {}
        }
        if self.unwind_exit() {
            // phpdbg's loop bails out at the unwind exit, and `ev`'s
            // `zend_catch` writes the status 0 and leaves the exit
            // pending, which the next evaluation renders.
            self.exit_status = 0;
            return Ok(Evaluated::Rendered);
        }
        if self.is_pending() {
            return Ok(self.evaluation_uncaught());
        }
        let returned = std::mem::take(&mut self.returned);
        let printed = self.print_r(&returned);
        self.release(returned);
        Ok(Evaluated::Printed(printed))
    }

    /// `zend_eval_stringl(code, &retval, name)`'s compile and serving
    /// (`Z/Zend/zend_execute_API.c:1333-1390`): `return code;` compiled
    /// as an `eval` under `name`, as a host's string is
    /// ([`Request::host_string`]), and served by the loop bound to the
    /// engine, in `scope`'s frame as a nested code frame is, which
    /// `zend_execute` serves with `EG(current_execute_data)` there: its
    /// variables, `$this` and scope, the frame below the evaluated one.
    /// `EG(current_execute_data)` stands at that frame from the compile
    /// on ([`crate::exec::Switch`]), as the debugger that evaluates there moves
    /// it, so a trace taken in the compile or the evaluated code starts
    /// at it and passes the frames above it. With none, on a frame of
    /// its own over the global table, as `zend_execute` serves an op
    /// array with no frame standing. What it returned is left in
    /// [`Request::returned`] where it was served; what the compiler
    /// threw is left pending.
    pub(super) fn eval_returning(
        &mut self,
        scope: Option<FrameRef>,
        code: &[u8],
        name: &[u8],
    ) -> Result<Evaluation, Fatal> {
        let Some(frame) = scope else {
            return self.eval_returning_in(None, code, name);
        };
        self.vm.push_switch(frame);
        let evaluated = self.eval_returning_in(scope, code, name);
        self.vm.pop_switch();
        evaluated
    }

    /// [`Request::eval_returning`] under the switch it stands.
    fn eval_returning_in(
        &mut self,
        scope: Option<FrameRef>,
        code: &[u8],
        name: &[u8],
    ) -> Result<Evaluation, Fatal> {
        let source = [&b"return "[..], code, b";"].concat();
        let Some(unit) = self.compiled_eval(&source, name) else {
            return Ok(Evaluation::NotCompiled);
        };
        if !crate::engine::note_compiled(self, &unit, SourceKind::EVAL) {
            return Ok(Evaluation::Refused);
        }
        self.serve_eval(scope, unit)?;
        Ok(Evaluation::Served)
    }

    /// `source` compiled as an `eval` under `name`, as a host's string
    /// is ([`Request::host_string`]), or none, what the compiler threw
    /// left pending.
    fn compiled_eval(&mut self, source: &[u8], name: &[u8]) -> Option<PublishedUnit> {
        match self.host_string(|req| req.compile_eval(source, name)) {
            Ok(unit) => Some(unit),
            Err(thrown) => {
                self.leave_pending(&thrown);
                None
            }
        }
    }

    /// `unit` served by the loop bound to the engine, in `scope`'s frame
    /// as a nested code frame is, or with none on a frame of its own over
    /// the global table, and every frame it left above the standing ones
    /// left.
    fn serve_eval(&mut self, scope: Option<FrameRef>, unit: PublishedUnit) -> Result<(), Fatal> {
        let standing = self.vm.depth();
        let from = scope.map_or(super::CodeFrom::Top(None), |frame| {
            super::CodeFrom::Nested(frame, None)
        });
        let callee = self.push_code_frame(unit, from)?;
        crate::observer::call(self, callee);
        let reenter = self.shared.loops.reenter;
        let served = self.serve_host_string(|req| reenter(req, callee));
        // `zend_leave_helper`'s arm for a nested code frame: its CVs
        // detached into the table it shares with the frame it was
        // evaluated in, which is attached to it again.
        while self.vm.depth() > standing {
            let top = self.vm.stack.current();
            self.leave_frame(top);
        }
        served
    }

    /// `f` served with the request executing nothing, as php's
    /// `EG(current_execute_data)` is `NULL` before `zend_execute` begins
    /// the program: the frames the request stands on set aside, their
    /// engine half with them, so what `f` serves is the whole chain,
    /// names no active file but its own and traces to `{main}`, and is
    /// pushed above their room on the same page as php pushes it on its
    /// one VM stack. What a frame set aside was lent is its own while
    /// it is off the chain, and the chain of its open observed frames is
    /// set aside with it, as a fiber's switch leaves both
    /// ([`Request::switch_execution`]). The global table's names that
    /// stand in a frame's CV are taken back into the table first, as
    /// `zend_detach_symbol_table` takes them, so a frame `f` attaches to
    /// the table finds them there; those frames are attached again
    /// after, the lowest first, and what `f` left in the table is theirs.
    pub(crate) fn nothing_executing<R>(&mut self, f: impl FnOnce(&mut Request) -> R) -> R {
        let bound = self.unbind_globals();
        self.own_lent_receivers();
        let frames = self.vm.stack.set_aside();
        // The stack's pages stay the running ones, the room of the frames
        // set aside kept, and the rest of the execution is set aside.
        let mut vm = std::mem::take(&mut self.vm);
        std::mem::swap(&mut self.vm.stack, &mut vm.stack);
        let mut observing = super::ObservedChain::default();
        self.swap_observing(&mut observing);
        let served = f(self);
        self.swap_observing(&mut observing);
        std::mem::swap(&mut self.vm.stack, &mut vm.stack);
        self.vm = vm;
        let dropped = self.vm.stack.put_back(&mut self.heap, frames);
        self.drop_frames(dropped);
        for frame in bound {
            self.attach_symbol_table(frame);
        }
        served
    }

    /// Every name of the global table that stands in a frame's CV, its
    /// value moved into the table where the CV holds one and the name
    /// deleted where it holds none, `zend_detach_symbol_table`'s rule
    /// ([`Request::detach_named`]); the frames they stood in, the lowest
    /// first.
    fn unbind_globals(&mut self) -> Vec<FrameRef> {
        (0..self.vm.stack.len())
            .filter_map(|at| {
                let frame = FrameRef(u32::try_from(at).ok()?);
                self.detach_named(frame, TableId::GLOBAL).then_some(frame)
            })
            .collect()
    }

    /// What stands pending after an evaluation, rendered uncaught with
    /// `E_DONT_BAIL`, so the request goes on with the status
    /// `php_error_cb` gave it, 255, as php's `ev` leaves it; an exit is
    /// given up and rendered as nothing, `zend_exception_error`'s
    /// "nothing more to do".
    fn evaluation_uncaught(&mut self) -> Evaluated {
        if self.pending().is_none() {
            self.raised = None;
            return Evaluated::Rendered;
        }
        self.exception_error(Level::Error).ok();
        self.catch_bailout();
        Evaluated::Rendered
    }

    /// `print_r($value, true)` served by the function of that name, as
    /// `zend_print_zval_r` renders a value.
    fn print_r(&mut self, value: &Value) -> Vec<u8> {
        self.addref(value);
        // `zend_print_zval_r` is C phpdbg calls directly, so a memory
        // limit met earlier in the request keeps nothing from printing.
        let rendered = self.call_named_from_c(b"print_r", &[*value, Value::bool(true)]);
        match rendered {
            Ok(returned) => {
                let bytes = returned
                    .as_str_bytes()
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
                self.release(returned);
                bytes
            }
            Err(thrown) => {
                self.leave_pending(&thrown);
                self.exception_error(Level::Error).ok();
                self.catch_bailout();
                Vec::new()
            }
        }
    }

    /// The function named `name` called with `arguments`, each given
    /// up after, as `zend_call_function` calls one by name.
    pub(crate) fn call_named(&mut self, name: &[u8], arguments: &[Value]) -> Result<Value, Thrown> {
        self.call_named_by(name, arguments, crate::api::call::call_made)
    }

    /// [`Request::call_named`] where php calls the function's C directly
    /// ([`crate::api::call::call_made_from_c`]).
    fn call_named_from_c(&mut self, name: &[u8], arguments: &[Value]) -> Result<Value, Thrown> {
        self.call_named_by(name, arguments, crate::api::call::call_made_from_c)
    }

    /// The function named `name` called with `arguments` by `call`, each
    /// given up after.
    fn call_named_by(
        &mut self,
        name: &[u8],
        arguments: &[Value],
        call: fn(&mut Request, &crate::api::abi::Callable, &[Value]) -> Result<Value, Thrown>,
    ) -> Result<Value, Thrown> {
        let name = self.string(name)?;
        let callable = crate::api::coerce::to_callable(self, &Value::string(name));
        self.release(Value::string(name));
        let callable = callable?;
        let called = call(self, &callable, arguments);
        for argument in arguments {
            self.release(*argument);
        }
        called
    }

    /// A host's call of a function by name ([`crate::tier::call`]): from
    /// the frame standing, or with nothing executing, where
    /// `zend_call_function` renders what the callee left uncaught itself
    /// as a fatal, `zend_throw_exception_internal` with no frame
    /// ([`Request::thrown_without_frame`]), and leaves as a fatal the
    /// callee raised and an `exit` leave. That is caught where the host
    /// asked, as phpdbg's prompt catches it in its `zend_try`, and the
    /// request goes on: no fatal is what it ends in, and its status is
    /// what an `exit` set or what the fatal's `php_error_cb` wrote, 255,
    /// unless the host bailed out of the log first
    /// ([`crate::api::abi::AfterLog::BailedOut`]). From a frame, what the
    /// callee left uncaught stays pending, as `zend_call_function` leaves
    /// `EG(exception)` with a frame executing.
    pub(crate) fn call_by_name(
        &mut self,
        under: Under,
        name: &[u8],
        arguments: &[crate::tier::Argument],
    ) -> Called {
        match under {
            Under::Frame(_) => self.call_by_name_here(name, arguments),
            Under::Nothing => self.nothing_executing(|req| {
                let ending = req.ending.take();
                let mut called = req.call_by_name_here(name, arguments);
                if called == Called::Threw {
                    req.exception_error(Level::Error).ok();
                    called = Called::Rendered;
                }
                let rendered = req.ending.is_some();
                req.ending = ending;
                if !req.diagnostics().bailing_out() {
                    return called;
                }
                req.catch_bailout();
                if rendered {
                    Called::Rendered
                } else {
                    Called::Exited
                }
            }),
        }
    }

    /// The call from wherever the request stands.
    fn call_by_name_here(&mut self, name: &[u8], arguments: &[crate::tier::Argument]) -> Called {
        let mut values = Vec::with_capacity(arguments.len());
        for argument in arguments {
            let value = match argument {
                crate::tier::Argument::Int(n) => Value::int(*n),
                crate::tier::Argument::Str(bytes) => match self.string(bytes) {
                    Ok(string) => Value::string(string),
                    Err(_) => return Called::Returned,
                },
            };
            values.push(value);
        }
        match self.call_named(name, &values) {
            Ok(returned) => {
                self.release(returned);
                Called::Returned
            }
            Err(thrown) => {
                if thrown.exception().is_none() {
                    return Called::Returned;
                }
                self.leave_pending(&thrown);
                Called::Threw
            }
        }
    }

    // ------------------------------------------------ the uncaught

    /// How the program is served.
    pub(crate) fn primary(&self) -> crate::engine::Primary {
        self.description.primary
    }

    /// How the host hands a wrapped program's file over.
    pub(crate) fn program_file(&self) -> crate::engine::ProgramFile {
        self.description.program_file
    }

    /// `zend_try_exception_handler`, then what is left kept for the host
    /// as phpdbg's `phpdbg_handle_exception` reads it
    /// (`Z/sapi/phpdbg/phpdbg_prompt.c:688`): its class, `file` and
    /// `line`, and what `__toString` answered, the status 255.
    pub(crate) fn keep_uncaught(&mut self) {
        // A handler that threw has rendered its throwable as the fatal
        // and left, and nothing is kept.
        if self.user_exception_handler().is_break() {
            return;
        }
        let Some(throwable) = self.take_pending_view() else {
            self.raised = None;
            return;
        };
        let class = self.symbols().class_name(throwable.class()).to_vec();
        let text = match crate::api::call::call_method_made(self, &throwable, b"__tostring", &[]) {
            Ok(returned) => {
                let text = returned
                    .deref()
                    .as_str_bytes()
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
                self.release(returned);
                text
            }
            Err(thrown) => {
                if let Some(inner) = thrown.exception() {
                    self.release(inner.as_value());
                }
                self.raised = None;
                Vec::new()
            }
        };
        self.inspect.kept_uncaught = Some(crate::engine::Uncaught {
            file: self.throwable_prop_bytes(throwable, b"file"),
            line: self.throwable_prop_int(throwable, b"line"),
            class,
            text,
        });
        self.release(throwable.as_value());
        self.exit_status = 255;
    }

    /// What [`Request::keep_uncaught`] kept, taken.
    pub(crate) fn take_kept_uncaught(&mut self) -> Option<crate::engine::Uncaught> {
        self.inspect.kept_uncaught.take()
    }
}
