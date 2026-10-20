//! What a request leaves pending: php's `EG(exception)`, a throwable or
//! one of php's two exits, raised, read, saved around a destructor or a
//! handler and restored, chained as `previous`, and taken by a `CATCH`;
//! the frame a throw left from; the uncaught throwable rendered as
//! `zend_exception_error` renders it; and what the entry frame of the
//! innermost serving returned. The pending slot and the returned value
//! stay on the request, since the tier face, the native face and the
//! loader read them.

use crate::api::call::{Function, write_prop};
use crate::api::raising::{Level, Thrown};
use crate::api::values::{ClassId, Owner, PhpObject, PhpString, Value};
use crate::engine::Known;
use php_unit::decl::Bare;

use super::Request;

/// Where an uncaught throwable ended the request: rendered as the fatal
/// it is (`zend_exception_error` at `E_ERROR`), or, where the handler
/// `set_exception_handler()` set or the throwable's `__toString` raised
/// in its place, what that raised rendered or its `exit` taken, and in each
/// case the request leaving by `zend_bailout` ([`Request::bailing_out`]).
/// The ending is written and nothing is left to render: what the caller
/// does next is unwind to the `zend_try` that catches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Leaving;

impl Request {
    /// What the entry frame of the innermost serving returned.
    pub(crate) fn set_returned(&mut self, value: Value) {
        self.returned = value;
    }

    /// What the entry frame last returned, taken, `null` left in its place.
    pub(crate) fn take_returned(&mut self) -> Value {
        std::mem::take(&mut self.returned)
    }

    /// Leave what a raise made pending where the loop reads it, php's
    /// `EG(exception)`. A `Thrown` that carries none is a fatal or an
    /// `exit` the raiser already recorded on the request. The
    /// throwable's one owner is the pending slot's from here; a second
    /// throwable raised while one is pending is given up, where php
    /// chains it as the first's `previous` (`zend_throw_exception_internal`),
    /// which arrives with `catch`. One raised while the unwind exit
    /// stands is given up too, which is php's own arm there: what is
    /// unwinding is not replaced by a different throwable
    /// (`Z/Zend/zend_exceptions.c:195-199`). The graceful exit is
    /// replaced, `zend_exception_set_previous` giving it up.
    pub(crate) fn leave_pending(&mut self, thrown: &Thrown) {
        let Some(object) = thrown.exception().copied() else {
            return;
        };
        match self.raised {
            None | Some(Raised::GracefulExit) if !self.bailout => {
                self.raised = Some(Raised::Throwable(object));
            }
            _ => self.release(object.as_value()),
        }
    }

    /// A step of php's body with `EG(exception)` set: `step` meets the
    /// throwable `pending` carries standing in php's slot, and the slot
    /// is given back after, so the `Thrown` stays the one owner. A user
    /// error handler is handed nothing then, since `zend_call_function`
    /// returns at once while a throwable is pending
    /// (`Z/Zend/zend_execute_API.c:822`), the built-in handler renders a
    /// diagnostic where no handler stands, and a stream's wrapper errors
    /// are kept quiet. A fatal or an exit runs nothing and answers
    /// `None`: php's `zend_bailout` left before the step. What a step's
    /// call answers is [`crate::api::raising::while_pending`]'s to say.
    pub(crate) fn while_pending<R>(
        &mut self,
        pending: &Thrown,
        step: impl FnOnce(&mut Request) -> R,
    ) -> Option<R> {
        let object = pending.exception().copied()?;
        if self.raised.is_some() || self.bailout {
            return Some(step(self));
        }
        self.raised = Some(Raised::Throwable(object));
        let outer = self.stepping_past.replace(object);
        let answer = step(self);
        self.stepping_past = outer;
        if self.raised == Some(Raised::Throwable(object)) {
            self.raised = None;
        }
        Some(answer)
    }

    /// Whether `object`, standing in `EG(exception)`, is the throwable a
    /// [`Request::while_pending`] step stands it in for, which the slot
    /// names with no owner of its own: what the step answers naming it
    /// takes none either.
    pub(crate) fn stepping_past(&self, object: PhpObject) -> bool {
        self.stepping_past == Some(object)
    }

    /// `zend_throw_exception_internal`'s chain
    /// (`Z/Zend/zend_exceptions.c:193-206`) for a handler php carries on
    /// past a diagnostic whose handler threw, as the dimension writes carry
    /// on past `ZVAL_UNDEFINED_OP1` and `zend_false_to_array_deprecated`:
    /// a throwable the handler raised after it takes the slot, and the
    /// one the diagnostic left pending, `ran_on`, is chained under it as
    /// its `previous`. Where anything else stands pending by then, a
    /// destructor's throwable the handler's own releases served, nothing
    /// is chained here. Everywhere else the loop stops at the first,
    /// which is where php tests `EG(exception)`, and
    /// [`Request::leave_pending`] gives a second up.
    ///
    /// ```php
    /// set_error_handler(function (y $y) {});
    /// $k = [];
    /// try { $y[$k]++; } catch (TypeError $e) {
    ///     echo $e->getMessage(), "\n", $e->getPrevious()::class, "\n";
    /// }
    /// ```
    /// ```expect
    /// Cannot access offset of type array on array
    /// TypeError
    /// ```
    pub(crate) fn raised_over(&mut self, thrown: Thrown, ran_on: Option<PhpObject>) -> Thrown {
        let (Some(new), Some(ran_on)) = (thrown.exception().copied(), ran_on) else {
            return thrown;
        };
        if let Some(Raised::Throwable(old)) = self.raised
            && !self.bailout
            && old.index() == ran_on.index()
            && old.index() != new.index()
        {
            self.raised = None;
            self.set_previous(new, old);
        }
        thrown
    }

    /// A release php makes with `EG(exception)` set, `OBJ_RELEASE` or
    /// `FREE_OP1` on a handler's way out: `free` served while `thrown`
    /// stands pending, so a destructor it serves sees the throwable
    /// pending and chains it as its own's `previous`, as
    /// `zend_objects_destroy_object` does, and what is pending after it
    /// is handed on. A fatal or an `exit` is freed past.
    pub(crate) fn free_pending(&mut self, thrown: Thrown, free: impl FnOnce(&mut Self)) -> Thrown {
        if thrown.exception().is_none() {
            free(self);
            return thrown;
        }
        self.leave_pending(&thrown);
        free(self);
        Thrown::of(self.take_pending_view())
    }

    /// The pending throwable at the end of a serving, php's
    /// `zend_exception_error` (design §5.9): the throwable's own
    /// `__toString` behind `Uncaught `, with `\n  thrown` after it, given
    /// to `php_error_cb` as `E_ERROR` at the site the throwable recorded
    /// rather than the site the loop has reached. The `in ... on line ...`
    /// that closes php's last line is that call's own tail, which is why
    /// the site appears twice and in two spellings.
    ///
    /// Nothing pending means the request is ending in the fatal or the
    /// `exit` the raiser recorded, and the loop has nothing to add.
    ///
    /// ```php
    /// nope();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// The rows are the frame chain as it stands at the raise, and
    /// `{main}` is the line every trace ends on whether or not the main
    /// frame still does. A destructor the shutdown walk serves stands
    /// with that frame already popped, so it is the outermost row and
    /// php gives it no file: `[internal function]`, the site of a frame
    /// no opline called.
    ///
    /// ```php
    /// class A { function __destruct() { nope(); } }
    /// $a = new A;
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A `ParseError` or a `CompileError`, the class itself and not a
    /// child, is the branch before that one: its message alone, as
    /// `E_PARSE` or `E_COMPILE_ERROR` at the site the throwable recorded,
    /// with no `Uncaught` and no trace, and the request ends as the
    /// diagnostic ends it. `token_get_all()` under `TOKEN_PARSE` is what
    /// throws one here, and its witness in `ext/tokenizer` pins the
    /// rendering; `a_parse_error_is_rendered_as_its_level` in
    /// `api::raising` holds it without the extension.
    pub(crate) fn uncaught(&mut self) -> Result<(), Leaving> {
        // `zend_execute_scripts` and `zend_throw_exception_internal` with
        // no frame standing: a user exception handler takes the
        // throwable first, and what it leaves is rendered. A handler
        // that threw has rendered its own throwable and left by
        // `zend_bailout`, past everything after it here; that fatal is
        // already the request's ending, and nothing is added to it.
        if self.user_exception_handler().is_break() {
            return Err(Leaving);
        }
        self.exception_error(Level::Error)
    }

    /// `zend_throw_exception_internal` (`Z/Zend/zend_exceptions.c:180`)
    /// over a throwable raised where no handler hands it back to the
    /// loop, a step a release serves (a resource's destructor, or what a
    /// stream an object's free closed parked): chained over what is
    /// pending, as `zend_exception_set_previous` chains it; where nothing
    /// is pending and no frame stands, as when `zend_close_rsrc_list`
    /// serves the destructor at the request's end, handed to the handler
    /// `set_exception_handler()` set, or rendered as the uncaught fatal
    /// and left by `zend_bailout` ([`Request::thrown_without_frame`]);
    /// pending for the frame that stands otherwise. A fatal or an exit
    /// the raiser recorded itself is left as it stands. Where the unwind
    /// exit is what is pending, the throwable is given up, since php does
    /// not put a different throwable in the place of what is unwinding
    /// (`Z/Zend/zend_exceptions.c:195-199`); [`Request::leave_pending`]
    /// gives it up so.
    pub(crate) fn throw_internal(&mut self, thrown: &Thrown) {
        if thrown.exception().is_none() {
            return;
        }
        if self.unwind_exit() {
            self.leave_pending(thrown);
            return;
        }
        match self.take_raised() {
            Some(before) => {
                self.leave_pending(thrown);
                self.raise_under(before);
            }
            None if !self.bailout && !self.executing() => {
                if let Ok(undef) = self.thrown_without_frame(thrown) {
                    self.release(undef);
                }
            }
            None => self.leave_pending(thrown),
        }
    }

    /// `zend_exception_error` (`Z/Zend/zend_exceptions.c:936`): what is
    /// pending rendered "Uncaught" at `severity`, with no handler asked,
    /// and nothing rendered for either exit. At `E_ERROR` it is the
    /// uncaught fatal and the request ends, [`Leaving`]; below it, php's
    /// `severity | E_DONT_BAIL` as `assert.bail` asks, it is a diagnostic
    /// like any other, and the throwable is given up. At every severity
    /// `EG(last_fatal_error_backtrace)` is cleared first
    /// (`Z/Zend/zend_exceptions.c:944`), so the shell's warning after an
    /// earlier fatal carries the throwable's trace alone (#2641).
    pub(crate) fn exception_error(&mut self, severity: Level) -> Result<(), Leaving> {
        let Some(throwable) = self.take_pending_view() else {
            // `zend_exception_error` on either exit: "We successfully
            // unwound, nothing more to do", and the object is released
            // (`Z/Zend/zend_exceptions.c:1005`); a fatal's bailout is
            // caught where the serving is left.
            self.raised = None;
            self.catch_bailout();
            return Ok(());
        };
        self.clear_fatal_error_backtrace();
        let class = self.class_name_of(throwable);
        let compile_time = match class.as_str() {
            "ParseError" => Some(Level::Parse),
            "CompileError" => Some(Level::CompileError),
            _ => None,
        };
        let message = if compile_time.is_some() {
            self.throwable_prop_bytes(throwable, b"message")
        } else {
            // The class's own `__toString`, which a user class may
            // override; a string it answers is kept in the base class's
            // `string` property, and a throwable it raises has no frame
            // to take it, so it is rendered uncaught in its turn and the
            // request leaves (`zend_call_function`'s tail,
            // [`Request::thrown_without_frame`]).
            match crate::api::call::call_method_made(self, &throwable, b"__tostring", &[]) {
                Ok(returned) => {
                    let text = returned.deref();
                    if text.as_str_bytes().is_some() {
                        let text = Owner::take(self, text);
                        write_prop(self, &throwable, b"string", text).ok();
                    } else {
                        crate::api::raising::warning(
                            self,
                            &format!("{class}::__toString() must return a string"),
                        );
                    }
                    self.release(returned);
                }
                Err(inner) => {
                    self.release(throwable.as_value());
                    if let Ok(undef) = self.thrown_without_frame(&inner) {
                        self.release(undef);
                    }
                    return Err(Leaving);
                }
            }
            [
                b"Uncaught ".as_slice(),
                &self.throwable_prop_bytes(throwable, b"string"),
                b"\n  thrown",
            ]
            .concat()
        };
        let (file, line) = self.throw_site(throwable);
        // `zend_error_va` with no file for a throwable that recorded an
        // empty one, which `php_error_cb` names `Unknown`; a
        // `ParseError`'s is handed on as it stands.
        let file = if file.is_empty() && compile_time.is_none() {
            b"Unknown".to_vec()
        } else {
            file
        };
        if compile_time.is_none() && severity != Level::Error {
            self.serve_uncaught(severity, &message, &file, line);
            self.release(throwable.as_value());
            return Ok(());
        }
        self.end_in_uncaught(compile_time.unwrap_or(Level::Error), &message, &file, line);
        // `zend_exception_error` ends with `OBJ_RELEASE(ex)`: the pending
        // slot's owner, given up once the rendering has read it.
        self.release(throwable.as_value());
        Err(Leaving)
    }

    /// The tail of `zend_call_function` where the callee left something
    /// pending and no frame stands to take it
    /// (`Z/Zend/zend_execute_API.c:1069`), as in an extension's
    /// `RSHUTDOWN` or a shutdown function: `zend_throw_exception_internal`
    /// with no frame hands a throwable to the handler `set_exception_handler` set,
    /// and the call is done where the handler took it, with the `UNDEF`
    /// the callee never replaced as what it returned; otherwise it is
    /// rendered as the uncaught fatal, and the unwind exit is dropped,
    /// and either leaves by `zend_bailout`, which is the `Thrown` with
    /// no throwable handed back. The session's write at request end is
    /// the witness, in `ext/session`.
    pub(crate) fn thrown_without_frame(&mut self, thrown: &Thrown) -> Result<Value, Thrown> {
        let threw = thrown.exception().is_some();
        self.leave_pending(thrown);
        // The handler took the throwable; or it was rendered, or an exit
        // was dropped, and the call leaves.
        if self.uncaught().is_ok() && threw {
            return Ok(Value::undef());
        }
        self.bail_out();
        Err(Thrown::of(None))
    }

    /// The declared name of a throwable's class, `Error` for one the
    /// tables do not know.
    fn class_name_of(&self, throwable: PhpObject) -> String {
        self.class_decl(throwable.class()).map_or_else(
            || String::from("Error"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        )
    }

    /// `Exception::__toString` (`Z/Zend/zend_exceptions.c:692`): each
    /// throwable of the `previous` chain rendered, the deepest last in
    /// the text php builds by putting what it has so far after `\n\nNext `
    /// behind the one it is rendering: the class, the message where
    /// there is one, the site behind a colon and the trace under `Stack
    /// trace:`. The message and the file are read as `zval_get_string`
    /// reads them, so an object there is its `__toString`, and the line
    /// as `zval_get_long`; a throwable raised on the way is handed back
    /// once the text is built, as php leaves it pending past the loop.
    /// The walk stops at a throwable it has already rendered, php's
    /// `Z_PROTECT_RECURSION_P`. The text is kept in the base class's
    /// private `string` property, which the uncaught rendering reads.
    ///
    /// ```php
    /// class M { function __toString(): string { return "late"; } }
    /// class E extends Exception { function __construct() { $this->message = new M; } }
    /// $e = new E;
    /// echo substr((string) $e, 0, 7), "\n";
    /// var_dump(str_starts_with(((array) $e)["\0Exception\0string"], "E: late in "));
    /// ```
    /// ```expect
    /// E: late
    /// bool(true)
    /// ```
    pub(crate) fn exception_string(&mut self, this: PhpObject) -> Result<PhpString, Thrown> {
        let mut text: Vec<u8> = Vec::new();
        let mut seen: Vec<PhpObject> = Vec::new();
        let mut raised: Option<Thrown> = None;
        let trace_as_string = self.trace_as_string_of(this);
        let mut at = Some(this);
        while let Some(exception) = at {
            let Some(throwable) = self.class_named(Bare::resolved(b"Throwable")) else {
                break;
            };
            if !self.instance_of(exception.class(), throwable) {
                break;
            }
            let mut read = |req: &mut Request, name: &[u8]| -> Vec<u8> {
                let value = exception.prop(req, name).unwrap_or_else(Value::null);
                match crate::ops::to_string(req, &value) {
                    Ok(string) => {
                        let bytes = string.as_bytes().to_vec();
                        req.release(Value::string(string));
                        bytes
                    }
                    Err(thrown) => {
                        if raised.is_none() {
                            raised = Some(thrown);
                        } else {
                            req.leave_pending(&thrown);
                        }
                        Vec::new()
                    }
                }
            };
            let mut message = read(self, b"message");
            let file = read(self, b"file");
            let line = exception.prop(self, b"line").unwrap_or_else(Value::null);
            let line = crate::ops::get_long(self, &line);
            let class = self.class_name_of(exception);
            // `Z/Zend/zend_exceptions.c:740`: a `TypeError` or an
            // `ArgumentCountError` whose message names the call site is
            // closed with ` and defined`, the class itself and not what
            // descends from it.
            if (class == "TypeError" || class == "ArgumentCountError")
                && message.windows(12).any(|window| window == b", called in ")
            {
                message.extend_from_slice(b" and defined");
            }
            let trace = self.served_trace(trace_as_string, exception, &mut raised);
            let mut rendered = class.into_bytes();
            if !message.is_empty() {
                rendered.extend_from_slice(b": ");
                rendered.extend_from_slice(&message);
            }
            rendered.extend_from_slice(b" in ");
            rendered.extend_from_slice(&file);
            rendered.extend_from_slice(format!(":{line}\nStack trace:\n").as_bytes());
            rendered.extend_from_slice(&trace);
            if !text.is_empty() {
                rendered.extend_from_slice(b"\n\nNext ");
                rendered.extend_from_slice(&text);
            }
            text = rendered;
            seen.push(exception);
            at = self
                .previous_of(exception)
                .filter(|previous| !seen.iter().any(|done| done.index() == previous.index()));
        }
        let string = self.string(&text)?;
        let kept = Owner::take(self, Value::string(string));
        if let Err(thrown) = write_prop(self, &this, b"string", kept) {
            self.release(Value::string(string));
            return Err(thrown);
        }
        match raised {
            Some(thrown) => {
                self.release(Value::string(string));
                Err(thrown)
            }
            None => Ok(string),
        }
    }

    /// `Exception::__toString`'s callee, found once as php finds it
    /// (`Z/Zend/zend_exceptions.c:705-706`): the final
    /// `getTraceAsString` of `this`'s base class, `Exception` or
    /// `Error` (`i_get_exception_base`), which is also the scope it is
    /// called in.
    fn trace_as_string_of(&self, this: PhpObject) -> Option<(Function, ClassId)> {
        let exception = self.class_named(Bare::resolved(b"Exception"))?;
        let base = if self.instance_of(this.class(), exception) {
            exception
        } else {
            self.class_named(Bare::resolved(b"Error"))?
        };
        Some((self.method_named(base, b"gettraceasstring")?, base))
    }

    /// One throwable's trace as `Exception::__toString` reads it: its
    /// `getTraceAsString` served through `zend_call_function`
    /// (`Z/Zend/zend_exceptions.c:731-733`), the call path a native
    /// calling a native takes, so an observer sees it begun and ended
    /// inside `__toString`. What is not a non-empty string, a call that
    /// threw and a call never made because something thrown on the way
    /// stands, as `zend_call_function` returns at once while
    /// `EG(exception)` holds one, is `#0 {main}` and a newline.
    fn served_trace(
        &mut self,
        trace_as_string: Option<(Function, ClassId)>,
        exception: PhpObject,
        raised: &mut Option<Thrown>,
    ) -> Vec<u8> {
        const MAIN: &[u8] = b"#0 {main}\n";
        let Some((function, base)) = trace_as_string.filter(|_| raised.is_none()) else {
            return MAIN.to_vec();
        };
        match self.serve_method_made(function, Some(exception), base, &[], &[]) {
            Ok(trace) => {
                let bytes = trace
                    .as_str_bytes()
                    .filter(|bytes| !bytes.is_empty())
                    .map_or_else(|| MAIN.to_vec(), <[u8]>::to_vec);
                self.release(trace);
                bytes
            }
            Err(thrown) => {
                *raised = Some(thrown);
                MAIN.to_vec()
            }
        }
    }

    /// The `file` and `line` a throwable recorded where it was made.
    fn throw_site(&self, throwable: PhpObject) -> (Vec<u8>, u32) {
        let file = self.throwable_prop_bytes(throwable, b"file");
        let line = u32::try_from(self.throwable_prop_int(throwable, b"line")).unwrap_or(0);
        (file, line)
    }

    /// `zend_default_exception_new`: a throwable records where it was
    /// made, `zend_get_executed_filename` and `zend_get_executed_lineno`,
    /// and the backtrace `zend_fetch_debug_backtrace` builds there, the
    /// array `getTrace` answers, written into the declared `trace` slot;
    /// `getTraceAsString`, `__toString` and the uncaught fatal render it
    /// through [`Request::trace_string_of`] when they are asked.
    pub(crate) fn note_throw_site(&mut self, throwable: PhpObject) -> Result<(), Thrown> {
        // `zend_get_executed_filename`, which names no script where no
        // user frame stands, as while the uncaught rendering serves a
        // throwable's `__toString` after the main frame has returned.
        let file = match self.overridden_site() {
            Some((file, _)) => file.to_vec(),
            None => self
                .innermost_frame()
                .and_then(crate::exec::FrameAt::code)
                .map_or_else(
                    || b"[no active file]".to_vec(),
                    |code| code.filename.0.to_vec(),
                ),
        };
        let trace = self.debug_backtrace()?;
        let trace = Owner::adopt(trace);
        let file = match PhpString::new(self, &file) {
            Ok(file) => file,
            Err(thrown) => {
                trace.give_up(self);
                return Err(thrown);
            }
        };
        if let Err(thrown) = self.write_known(throwable, Known::File, file.into_value()) {
            trace.give_up(self);
            return Err(thrown);
        }
        let line = Owner::int(i64::from(self.current_line()));
        if let Err(thrown) = self.write_known(throwable, Known::Line, line) {
            trace.give_up(self);
            return Err(thrown);
        }
        self.write_known(throwable, Known::Trace, trace.into_value())
    }

    /// `zend_update_property_ex(base_ce, object, ZSTR_KNOWN(..), value)`:
    /// one of the throwable's own properties written under its interned
    /// name, which costs the write no string of its own.
    fn write_known(
        &mut self,
        throwable: PhpObject,
        name: Known,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let name = match self.known(name) {
            Ok(name) => name,
            Err(thrown) => {
                value.give_up(self);
                return Err(thrown);
            }
        };
        throwable.set_prop(self, name, value)
    }

    /// The first half of `zend_objects_destroy_object`'s protection of
    /// a destructor from a throwable raised before it
    /// (`Z/Zend/zend_objects.c:148-168`): the pending slot is emptied
    /// for the call, and what it held is handed back for
    /// [`Request::restore_pending`]. A destructor served on the pending
    /// throwable itself is php's one `E_CORE_ERROR`, and the `None`
    /// here is that: the request is ending and nothing is served.
    ///
    /// No PHP snippet reaches that fatal, in this engine or in php,
    /// whose own corpus has no test naming it (harness §8). It would
    /// take the pending throwable's last owner released while the slot
    /// still holds it, and php releases one in that state in two places
    /// only: `zend_clear_exception` empties the slot before
    /// `OBJ_RELEASE`, under php's own comment that the throwable may
    /// have a destructor (`Z/Zend/zend_exceptions.c:251-254`), and
    /// `zend_user_exception_handler` releases `EG(exception)` before
    /// emptying it (`Z/Zend/zend.c:1949-1953`) but is reached only with
    /// `EG(current_execute_data)` null, where `zend_call_function`'s
    /// tail renders the handler's own throwable uncaught and
    /// `zend_bailout`s first. `zend_exception_set_previous`, called from
    /// `zend_throw_exception_internal` before the slot is reassigned
    /// (`Z/Zend/zend_exceptions.c:201-202`), gives up the slot's object
    /// only where a `previous` chain or the throw itself holds it a
    /// second time. This engine's slot is the one owner of what stands
    /// in it and nothing releases that while it stands, so the state is
    /// unreachable here for its own reason as well. The unit test beside
    /// this builds what the release cannot, and #458 is the question
    /// that leaves for a person.
    pub(super) fn save_pending(&mut self, object: PhpObject) -> Saved {
        match self.raised {
            Some(Raised::Throwable(pending)) if pending.index() == object.index() => {
                self.end_in_core_error("Attempt to destruct pending exception");
                Saved::TheObject
            }
            _ => Saved::Raised(self.raised.take()),
        }
    }

    /// The second half: the throwable the destructor was protected from
    /// is pending again, and one the destructor itself raised keeps the
    /// slot with the old one chained onto it as its `previous`
    /// (`zend_exception_set_previous`), which is what the rendering
    /// prints under `Next`.
    ///
    /// The array here gives its two elements up in order, and the first
    /// destructor's raise is pending while the second is served: it is
    /// saved, the second destructor writes its own line and raises in
    /// its turn, and the two are one uncaught throwable with the first
    /// under `Next`.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = [new A(1), new A(2)];
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect exit=255
    /// d1
    /// d2
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 %s(8): A->__destruct()
    /// #1 {main}
    ///
    /// Next Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 %s(8): A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    pub(super) fn restore_pending(&mut self, saved: Saved) {
        if let Saved::Raised(Some(old)) = saved {
            self.raise_under(old);
        }
    }

    /// The restore php writes after each step it protects from what was
    /// pending before it, a destructor, a generator's `finally` on
    /// destruction and a fiber's graceful unwind: `old` is pending again
    /// where nothing is, and where something is,
    /// `zend_exception_set_previous(EG(exception), old)` chains a
    /// throwable under the one pending and gives up an exit
    /// (`Z/Zend/zend_exceptions.c:113-116`), so what the step raised is
    /// what is unwound from here and an `exit`'s status stands only
    /// until that throwable is rendered. An exit pending keeps the slot,
    /// and a throwable under it is given up with it.
    pub(crate) fn raise_under(&mut self, old: Raised) {
        match (self.raised, old) {
            (None, old) if !self.bailout => self.raised = Some(old),
            (Some(Raised::Throwable(new)), Raised::Throwable(old)) => self.set_previous(new, old),
            (_, old) => self.release_raised(old),
        }
    }

    /// What was pending out of php's slot, any of the three, its owner
    /// the caller's: `EG(exception) = NULL` beside the object taken.
    pub(crate) fn take_raised(&mut self) -> Option<Raised> {
        self.raised.take()
    }

    /// `EG(exception) = Z_OBJ_P(fast_call)`: what a `finally` block
    /// delayed standing in the slot again, which `FAST_RET` writes.
    pub(crate) fn set_raised(&mut self, raised: Option<Raised>) {
        self.raised = raised;
    }

    /// What was pending, a throwable's owner given up; an exit holds no
    /// object here.
    pub(crate) fn release_raised(&mut self, raised: Raised) {
        if let Raised::Throwable(object) = raised {
            self.release(object.as_value());
        }
    }

    /// `zend_exception_set_previous` (`Z/Zend/zend_exceptions.c:103`):
    /// `previous` is chained onto the end of the chain `throwable`
    /// already carries, its one owner the property's from here. php
    /// refuses to make a cycle, and walks the two chains together to
    /// find one: at each throwable of `throwable`'s chain, where that
    /// throwable is already among `previous`'s own ancestors, or where
    /// `previous` is `throwable` itself or already in its chain,
    /// `previous` takes its owner back and nothing is chained. A chain
    /// is a path through live objects, so the store's count is a length
    /// no walk of it passes.
    ///
    /// A `finally` that rethrows the previous of the throwable it
    /// delayed chains nothing, and the one it rethrows is what leaves:
    ///
    /// ```php
    /// try {
    ///     $e = new Exception("Foo");
    ///     try { throw new Exception("Bar", 0, $e); } finally { throw $e; }
    /// } catch (Exception $e) {
    ///     echo $e->getMessage(), " ", var_export($e->getPrevious(), true), "\n";
    /// }
    /// ```
    /// ```expect
    /// Foo NULL
    /// ```
    pub(crate) fn set_previous(&mut self, throwable: PhpObject, previous: PhpObject) {
        let bound = self.heap.objects();
        let mut at = throwable;
        for _ in 0..=bound {
            if at.index() == previous.index() {
                break;
            }
            let mut ancestor = self.previous_of(previous);
            for _ in 0..=bound {
                let Some(held) = ancestor else {
                    break;
                };
                if held.index() == at.index() {
                    self.release(previous.as_value());
                    return;
                }
                ancestor = self.previous_of(held);
            }
            if let Some(next) = self.previous_of(at) {
                at = next;
                continue;
            }
            let previous = Owner::adopt(previous.as_value());
            // `write_prop` gives the owner up where it fails.
            write_prop(self, &at, b"previous", previous).ok();
            return;
        }
        self.release(previous.as_value());
    }

    /// The `previous` property of a throwable, when it holds one.
    fn previous_of(&self, throwable: PhpObject) -> Option<PhpObject> {
        throwable
            .prop(self, b"previous")
            .and_then(|value| value.deref().as_object())
    }

    /// Whether php's `EG(exception)` holds a throwable or an exit, or the
    /// request is leaving for the `zend_try` that catches a fatal: the
    /// loop's `Threw` edge, and nothing more is served until it is
    /// handled.
    pub(crate) fn is_pending(&self) -> bool {
        self.raised.is_some() || self.bailout
    }

    /// What stands in php's slot, `EG(exception)`, a view, no owner
    /// taken: what an observer's end handler reads where the frame it is
    /// handed is left by a throwable or an exit, as `observer.c`'s
    /// `observer_end` names its class.
    #[must_use]
    pub fn raised(&self) -> Option<Raised> {
        self.raised
    }

    /// The pending throwable as a view, no owner taken: what `CATCH`
    /// tests its class against and what the unwinder chains onto. None
    /// while an exit stands.
    pub(crate) fn pending(&self) -> Option<PhpObject> {
        match self.raised {
            Some(Raised::Throwable(object)) => Some(object),
            _ => None,
        }
    }

    /// The pending throwable out, the slot left empty and its one owner
    /// the caller's: php's `EG(exception) = NULL` beside the object it
    /// took, which is what a matching `CATCH` does with it. An exit
    /// standing is left where it stands.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn take_pending(&mut self) -> Option<Owner<PhpObject>> {
        let object = self.take_pending_view()?;
        Some(Owner::adopt(object))
    }

    /// What [`take_pending`](Self::take_pending) takes, its one owner not
    /// yet named.
    pub(crate) fn take_pending_view(&mut self) -> Option<PhpObject> {
        let object = self.pending()?;
        self.raised = None;
        Some(object)
    }
}

/// What `zend_objects_destroy_object` found in the pending slot before
/// it served a destructor ([`Request::save_pending`]): nothing, one
/// throwable held there for the duration of the call, the unwind exit
/// held the same way, or the throwable that is the object being
/// destroyed, which is php's one `E_CORE_ERROR` and no call at all.
#[derive(Clone, Copy)]
pub(super) enum Saved {
    Raised(Option<Raised>),
    TheObject,
}

/// What stands in php's `EG(exception)`: a throwable, or one of the two
/// exits php unwinds as objects of internal classes no `catch` names
/// and `zend_exception_error` renders nothing for, `UnwindExit` and
/// `GracefulExit`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Raised {
    /// A throwable, whose one owner is the slot's.
    Throwable(PhpObject),
    /// `zend_ce_unwind_exit`, what `exit` throws: no `finally` block is
    /// served for it, every throwable it meets is given up, and none
    /// raised while it stands replaces it.
    UnwindExit,
    /// `zend_ce_graceful_exit`, what `zend_fiber_object_destroy` resumes
    /// a suspended fiber with: the fiber's `finally` blocks are served
    /// for it, a throwable one of them delayed is given up, and one
    /// raised while it stands replaces it.
    GracefulExit,
}
