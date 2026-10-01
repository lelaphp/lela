//! The raising group (design §4.4): how a native reports what php
//! reports. The engine knows the active native's name and parameters
//! before the body, so the argument raisers take the position and the
//! tail of the message and the engine renders `fn(): Argument #N ($name)
//! must ...` itself. [`Thrown`] is the error every native returns. In
//! this skeleton nothing is rendered or dispatched to a user handler: a
//! diagnostic is kept on the request as its last error, and a `Thrown`
//! carries the throwable it would have made pending.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::api::request::Request;
use crate::api::values::{PhpObject, PhpString, Value};

/// php's error levels, `E_*`: the unit's, since a compile-time
/// diagnostic is recorded on the unit at one of them and raised here
/// (design §6.1).
pub use php_unit::Level;

/// What a native returns when it does not return a value: a throwable is
/// pending on the request, or the request is ending in a fatal or an
/// `exit`. A `Thrown` in hand *is* the pending state; there is no second
/// channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Thrown {
    pending: Option<PhpObject>,
}

impl Thrown {
    pub(crate) const fn of(pending: Option<PhpObject>) -> Thrown {
        Thrown { pending }
    }

    /// The throwable that is pending, if one is: `None` for a fatal or
    /// an `exit`.
    #[must_use]
    pub fn exception(&self) -> Option<&PhpObject> {
        self.pending.as_ref()
    }

    /// Which of the three ways out this is: a throwable, the `exit` a
    /// callback unwinds (`zend_is_unwind_exit`), which php's C code goes
    /// on past with the unwind exit pending, or a fatal, which
    /// `zend_bailout` leaves a native by and a `zend_catch` sees. A
    /// native that cleans up after a fatal raised under it, as
    /// `php_session_decode` does in its `zend_catch`
    /// (`Z/ext/session/session.c:289-302`), matches on
    /// [`Cause::Fatal`] and hands the `Thrown` on. The session's decode
    /// is the witness, in `ext/session`.
    #[must_use]
    pub fn cause(&self, req: &Request) -> Cause<'_> {
        match &self.pending {
            Some(throwable) => Cause::Throwable(throwable),
            None if req.unwind_exit() => Cause::Exit,
            None => Cause::Fatal,
        }
    }

    /// `zend_throw_exception_internal` with a throwable already in
    /// `EG(exception)`: `later`, raised while this one was pending, is
    /// what is pending from here, with this one chained onto the end of
    /// its `previous` chain (`zend_exception_set_previous`,
    /// `Z/Zend/zend_exceptions.c:103`). A fatal or an `exit` is not
    /// chained: where `later` is one, this throwable is given up and
    /// `later` is what leaves; where this is one, `later` could not have
    /// been raised past it and is given up. A dump of a `DOMNode` no
    /// constructor made is the witness, in `ext/dom`: each property's
    /// reader fails in turn and the throwables leave as one chain.
    #[must_use]
    pub fn chain(self, req: &mut Request, later: Thrown) -> Thrown {
        match (self.pending, later.pending) {
            (Some(earlier), Some(now)) => {
                req.set_previous(now, earlier);
                later
            }
            (Some(earlier), None) => {
                req.release(earlier.as_value());
                later
            }
            (None, now) => {
                if let Some(now) = now {
                    req.release(now.as_value());
                }
                self
            }
        }
    }
}

/// What a [`Thrown`] carries, [`Thrown::cause`]: the throwable pending,
/// the unwind exit an `exit` throws, or a fatal leaving by
/// `zend_bailout`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cause<'a> {
    Throwable(&'a PhpObject),
    Exit,
    Fatal,
}

/// Raise a diagnostic at `level` with php's message text.
pub fn raise(req: &mut Request, level: Level, message: &str) {
    req.note_error(level, message);
}

/// `zend_error_at`: the same diagnostic at a file and line of its own
/// rather than the site the request has reached, which is what php's
/// scanner and compiler raise with while a file is being compiled, the
/// compiled name and `CG(zend_lineno)`. What `token_get_all()` under
/// `TOKEN_PARSE` raises for an octal escape overflow is the witness, in
/// `ext/tokenizer`: the file is the empty one `tokenize_parse` scans
/// under, so php prints `in  on line 1`.
pub fn raise_at(req: &mut Request, level: Level, message: &str, file: &[u8], line: u32) {
    req.note_error_in(level, message, file, line);
}

/// `zend_exception_error(ex, severity)` over the throwable `thrown`
/// carries: rendered "Uncaught ..." at `severity` with its own site, as
/// `assert()` under `assert.bail` renders what its callback threw at
/// `E_WARNING`, the throwable given up. At `E_ERROR` it is the uncaught
/// fatal and the request ends, which is the `Err`; a `thrown` that
/// carries a fatal or an exit is handed on as it is.
///
/// ```php
/// ini_set("assert.bail", 1);
/// ini_set("assert.exception", 0);
/// function cb() { throw new Exception("from the callback"); }
/// ini_set("assert.callback", "cb");
/// assert(false);
/// echo "not reached\n";
/// ```
/// ```expect
/// Deprecated: ini_set(): assert.bail INI setting is deprecated in %s on line 2
///
/// Deprecated: ini_set(): assert.exception INI setting is deprecated in %s on line 3
///
/// Deprecated: ini_set(): assert.callback INI setting is deprecated in %s on line 5
///
/// Warning: assert(): assert(false) failed in %s on line 6
///
/// Warning: Uncaught Exception: from the callback in %s:4
/// Stack trace:
/// #0 [internal function]: cb('%s', 6, NULL, 'assert(false)')
/// #1 %s(6): assert(false, 'assert(false)')
/// #2 {main}
///   thrown in %s on line 4
/// ```
pub fn uncaught_at(req: &mut Request, thrown: Thrown, severity: Level) -> Result<(), Thrown> {
    if thrown.exception().is_none() {
        return Err(thrown);
    }
    req.leave_pending(&thrown);
    req.exception_error(severity)
        .or_else(|fatal| req.fatal_of(Err(fatal)))
}

/// `zend_error` with `EG(exception)` set: the diagnostic raised while
/// the throwable `pending` carries is pending, as php raises one after a
/// call that threw, where the engine's slot would otherwise stand empty
/// because the throwable is in the native's hands. No user error handler
/// is handed it, since php's `zend_call_function` returns at once while
/// a throwable is pending, and the built-in handler renders it where
/// none is set; a `pending` that carries a fatal or an exit raises
/// nothing. `pending` is the native's still, to hand on. The overloaded
/// element's notice after a throwing `offsetGet` is the witness, in
/// `ext/spl`.
pub fn raise_while_pending(req: &mut Request, pending: &Thrown, level: Level, message: &str) {
    req.while_pending(pending, |req| raise(req, level, message));
}

/// php's body carrying on past a throwable it left in `EG(exception)`:
/// `step` is taken while the throwable `pending` carries stands as the
/// request's pending one, so what php skips or keeps quiet while a
/// throwable is pending is skipped and kept quiet here, a diagnostic
/// raised as [`raise_while_pending`] raises one and a stream's `Failed
/// to open stream` among what `php_stream_display_wrapper_errors` keeps
/// quiet. `None`, with nothing taken, where `pending` carries a fatal or
/// an exit, which unwind at once. `pending` is the native's still, to
/// hand on once the step is done. `hash_update_file`, which opens and
/// hashes its file after `zend_fetch_resource_ex` threw over its
/// `$stream_context`, is the witness, in `ext/hash`.
pub fn while_pending<R>(
    req: &mut Request,
    pending: &Thrown,
    step: impl FnOnce(&mut Request) -> R,
) -> Option<R> {
    req.while_pending(pending, step)
}

/// `zend_error` while php starts, before any request (`zend_startup`,
/// `php_module_startup`, the `ZEND_INI_STAGE_STARTUP` rules of the ini
/// entries): the message as given, at `Unknown` on line 0, and displayed
/// past the `php_output` layer, which is not active yet and writes
/// straight to the host (`php_output_direct`), so it sends no header and
/// no buffer takes it. An extension's startup rule over the host's ini
/// values raises through this from the request's `RINIT`, since this
/// engine raises nothing while it is built; `php_error_docref`'s own
/// origin there, `PHP Startup: `, is the caller's to write. The session's
/// `session.use_only_cookies=0` at startup is the witness, in
/// `ext/session`.
pub fn startup(req: &mut Request, level: Level, message: &str) {
    req.note_startup_error(level, message);
}

/// `E_WARNING`.
pub fn warning(req: &mut Request, message: &str) {
    raise(req, Level::Warning, message);
}

/// `E_NOTICE`.
pub fn notice(req: &mut Request, message: &str) {
    raise(req, Level::Notice, message);
}

/// `E_DEPRECATED`.
pub fn deprecated(req: &mut Request, message: &str) {
    raise(req, Level::Deprecated, message);
}

/// `zend_deprecated_constant` (`Z/Zend/zend_execute.c:1994`), where the
/// constant the table holds under `key` carries `CONST_DEPRECATED`:
/// `Constant K is deprecated`, named by that key, with the `Deprecated`
/// object's suffix ([`crate::attributes::deprecation_suffix`]), at
/// `E_USER_DEPRECATED` for a constant a program defined and
/// `E_DEPRECATED` for an extension's. Every other constant raises
/// nothing. What a handler threw is the `Err`.
///
/// ```php
/// #[\Deprecated("use NEW", since: "2.0")]
/// const OLD = 1;
/// set_error_handler(function ($errno, $errstr) { var_dump($errno === E_USER_DEPRECATED); echo $errstr, "\n"; });
/// echo OLD, "\n";
/// ```
/// ```expect
/// bool(true)
/// Constant OLD is deprecated since 2.0, use NEW
/// 1
/// ```
///
/// The constant is held while its deprecation is worded
/// (`CONST_PROTECT_RECURSION`), so an attribute naming it reads it
/// without raising twice:
///
/// ```php
/// #[\Deprecated(TEST)]
/// const TEST = "from itself";
/// #[\Deprecated]
/// const TEST2 = "from another";
/// #[\Deprecated(TEST2)]
/// const TEST3 = 1;
/// TEST;
/// TEST3;
/// ```
/// ```expect
/// Deprecated: Constant TEST is deprecated, from itself in %s on line 8
///
/// Deprecated: Constant TEST2 is deprecated in %s on line 9
///
/// Deprecated: Constant TEST3 is deprecated, from another in %s on line 9
/// ```
pub(crate) fn deprecated_constant(req: &mut Request, key: &[u8]) -> Result<(), Thrown> {
    let Some((true, user)) = req.constant_deprecation(key) else {
        return Ok(());
    };
    if !req.protect_recursion(None, key) {
        return Ok(());
    }
    let attributes = req.constant_attributes(key).to_vec();
    let suffix = crate::attributes::deprecation_suffix(req, &attributes, None);
    req.unprotect_recursion();
    let suffix = suffix?;
    let level = if user {
        Level::UserDeprecated
    } else {
        Level::Deprecated
    };
    let name = String::from_utf8_lossy(key).into_owned();
    raise(
        req,
        level,
        &format!("Constant {name} is deprecated{suffix}"),
    );
    if req.is_pending() {
        return Err(Thrown::of(None));
    }
    Ok(())
}

/// `php_error_docref(NULL, level, ...)`: a diagnostic from inside a
/// native, its message behind the native's declared name and `(): `,
/// which is how `ext/standard`'s natives raise most of what they raise
/// (`chr(): Providing a value not in-between 0 and 255 is deprecated,
/// ...`). The docref itself, php's link into the manual, is off in the
/// CLI's ini and is not rendered here.
///
/// Where no native is active, `php_verror` names the code being served,
/// as `get_active_function_name` and `get_active_class_name` name it,
/// `main` for a file's own code, which is where an object's handler
/// raises from; and where none is served either, the request's phase
/// (`Z/main/main.c:1086-1129`): `PHP Request Startup` in an extension's
/// `RINIT`, `PHP Request Shutdown` in its `RSHUTDOWN`, and `Unknown`
/// between them, each with no `()`. The session's write at request end
/// is the witness of the phase, in `ext/session`.
///
/// ```php
/// $o = unserialize('O:1:"X":0:{}');
/// function f($o) { return $o->a; }
/// class C { function m($o) { return isset($o->a); } }
/// f($o); (new C)->m($o); $o->a;
/// ```
/// ```expect
///
/// Warning: f(): The script tried to access a property on an incomplete object. Please ensure that the class definition "X" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition in %s on line 3
///
/// Warning: C::m(): The script tried to access a property on an incomplete object. Please ensure that the class definition "X" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition in %s on line 4
///
/// Warning: main(): The script tried to access a property on an incomplete object. Please ensure that the class definition "X" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition in %s on line 5
/// ```
pub fn docref(req: &mut Request, level: Level, message: &str) {
    // `php_verror`'s origin while modules start (`Z/main/main.c:1087`):
    // `PHP Startup`, no function and so no `()`.
    // A fatal in flight is no native served: `_zend_bailout` empties
    // `EG(current_execute_data)` before it jumps (`Z/Zend/zend.c:1267`),
    // so what a `zend_catch` raises names no function.
    let text = if req.ini_stage() == crate::api::abi::IniStage::Startup {
        format!("PHP Startup: {message}")
    } else if (req.vm_serving_native() || req.serving_include()) && !req.bailing_out() {
        let function = req.active_native();
        format!("{}: {message}", origin(req, &function, ""))
    } else {
        let origin = match req.phase() {
            crate::api::request::Phase::Startup => String::from("PHP Request Startup"),
            // The code being served names it, as
            // `get_active_function_name` does when a handler raises it.
            crate::api::request::Phase::Running if req.bailing_out() => String::from("Unknown"),
            crate::api::request::Phase::Running => req
                .active_user_function()
                .map_or_else(|| String::from("Unknown"), |name| format!("{name}()")),
            crate::api::request::Phase::Shutdown | crate::api::request::Phase::ResourceShutdown => {
                String::from("PHP Request Shutdown")
            }
        };
        format!("{origin}: {message}")
    };
    raise(req, level, &text);
}

/// `php_verror`'s origin for a function's diagnostic, `function(params)`,
/// and its link to the manual (`Z/main/main.c:1150-1212`) under
/// `html_errors` with a `docref_root`:
/// the function's page, `function.name` or `class.method` with its
/// leading underscores dropped, `_` made `-` and lowered, `docref_ext`
/// after it, and a `#target` kept past the extension; none otherwise.
///
/// ```php
/// ini_set('html_errors', 1); ini_set('docref_root', '/'); ini_set('docref_ext', '.html');
/// @fopen('nope.txt', 'r');
/// echo strip_tags(error_get_last()['message']), "\n";
/// ```
/// ```expect
/// fopen(nope.txt) [function.fopen.html]: Failed to open stream: No such file or directory
/// ```
pub(crate) fn origin(req: &Request, function: &str, params: &str) -> String {
    match docref_link(req, function) {
        Some(link) => format!("{function}({params}) {link}"),
        None => format!("{function}({params})"),
    }
}

/// [`origin`]'s link, where there is one.
fn docref_link(req: &Request, function: &str) -> Option<String> {
    if !req.ini_flag(b"html_errors") {
        return None;
    }
    let root = String::from_utf8_lossy(req.ini_get(b"docref_root")?).into_owned();
    if root.is_empty() {
        return None;
    }
    let page = match function.split_once("::") {
        Some((class, method)) => format!("{class}.{}", method.trim_start_matches('_')),
        None => format!("function.{}", function.trim_start_matches('_')),
    };
    let mut page = page.replace('_', "-").to_ascii_lowercase();
    let target = page
        .rfind('#')
        .map(|at| page.split_off(at))
        .unwrap_or_default();
    if let Some(ext) = req.ini_get(b"docref_ext").filter(|ext| !ext.is_empty()) {
        page.push_str(&String::from_utf8_lossy(ext));
    }
    Some(format!("[<a href='{root}{page}{target}'>{page}</a>]"))
}

/// `zend_call_stack_overflowed(EG(stack_limit))` with
/// `zend_call_stack_size_error`: php's `Error` where the thread has come
/// down to its stack limit, what a native that recurses over a value
/// asks at each level, as `php_var_serialize_intern` does, so a value
/// nested past what the stack holds is refused and not a crash.
///
/// ```php
/// $o = $first = new stdClass;
/// for ($i = 0; $i < 100000; $i++) { $o->next = new stdClass; $o = $o->next; }
/// try { serialize($first); } catch (Error $e) { echo preg_replace('/\d+ bytes/', 'N bytes', $e->getMessage()), "\n"; }
/// while ($next = $first->next ?? null) { $first->next = $next->next ?? null; }
/// ```
/// ```expect
/// Maximum call stack size of N bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?
/// ```
///
/// Under the small-stack regime the size is the ini's, 256K less
/// `zend.reserved_stack_size`, whose least is the platform's
/// (`php_compile::StackLimit`), so the number is masked; the chain is
/// a quarter of the one above, which 256K refuses in both engines.
///
/// ```php
/// $o = $first = new stdClass;
/// for ($i = 0; $i < 25000; $i++) { $o->next = new stdClass; $o = $o->next; }
/// try { serialize($first); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// while ($next = $first->next ?? null) { $first->next = $next->next ?? null; }
/// ```
/// ```expect regime=small-stack
/// Maximum call stack size of %d bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?
/// ```
pub fn call_stack_limit(req: &mut Request) -> Result<(), Thrown> {
    let stack = req.stack_limit();
    if !stack.overflowed() {
        return Ok(());
    }
    Err(call_stack_size_error(req, stack.max_stack_size()))
}

/// The same `Error` at the thread's floor rather than its limit
/// (`php_compile::StackLimit::exhausted`): what a walk asks at each
/// level where php asks no limit of it. The walk is refused one reserve
/// short of the thread's end, with the size it had, so a nesting php
/// takes whole under a small `zend.max_allowed_stack_size` is taken
/// whole here too, as the small-stack witness beside `compare_arrays`
/// in the kernels shows.
pub fn call_stack_end(req: &mut Request) -> Result<(), Thrown> {
    let stack = req.stack_limit();
    if !stack.exhausted() {
        return Ok(());
    }
    Err(call_stack_size_error(req, stack.floor_size()))
}

/// `zend_call_stack_size_error`: php's sentence over `size`.
fn call_stack_size_error(req: &mut Request, size: usize) -> Thrown {
    throw(
        req,
        "Error",
        &format!(
            "Maximum call stack size of {size} bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?"
        ),
    )
}

/// `E_ERROR`: `php_error_cb` renders it where the request stands and the
/// request ends after this; the `Thrown` says so.
pub fn fatal(req: &mut Request, message: &str) -> Thrown {
    req.end_in_fatal(message);
    Thrown::of(None)
}

/// Throw a new instance of `class` with `message`, `zend_throw_error`.
/// A throwable the memory limit refuses to build is the fatal that ends
/// the request, already recorded, and the `Thrown` says so. The
/// instance's one owner is the `Thrown`'s, as `zend_throw_exception`
/// hands the object it made to `EG(exception)`, so once the program
/// lets go of what it caught the object is freed:
///
/// ```php
/// try { intdiv(1, 0); } catch (Error $e) { $w = WeakReference::create($e); }
/// unset($e);
/// var_dump($w->get(), new stdClass);
/// ```
/// ```expect
/// NULL
/// object(stdClass)#1 (0) {
/// }
/// ```
pub fn throw(req: &mut Request, class: &str, message: &str) -> Thrown {
    match new_throwable(req, class, message) {
        Ok(throwable) => {
            let thrown = throw_value(req, &throwable);
            req.release(throwable);
            thrown
        }
        Err(fatal) => fatal,
    }
}

/// A throwable of `class` made at a site of its own, the `file` and
/// `line` `zend_default_exception_new` records for a `ParseError` or a
/// `CompileError` while a file is being compiled: the compiled name and
/// `CG(zend_lineno)`, never the executed site. The one thrower here is
/// `token_get_all()` under `TOKEN_PARSE`, whose parse runs under the
/// empty file name, and its witness in `ext/tokenizer` pins the
/// rendering; `a_parse_error_is_rendered_as_its_level` below holds it
/// without the extension.
pub fn throw_at(req: &mut Request, class: &str, message: &str, file: &[u8], line: u32) -> Thrown {
    match throwable_at(req, class, message, file, line) {
        Ok(object) => Thrown::of(Some(object)),
        Err(fatal) => fatal,
    }
}

/// The instance behind [`throw_at`], its strings the request's; the
/// `Err` is the memory limit refusing them.
fn throwable_at(
    req: &mut Request,
    class: &str,
    message: &str,
    file: &[u8],
    line: u32,
) -> Result<PhpObject, Thrown> {
    let object = req.new_object_of(class)?;
    let message = Value::string(PhpString::new(req, message.as_bytes())?);
    let file = Value::string(PhpString::new(req, file)?);
    let line = Value::int(i64::from(line));
    for (name, value) in [(&b"message"[..], message), (b"file", file), (b"line", line)] {
        let name = PhpString::new(req, name)?;
        object.set_prop(req, name, value)?;
    }
    Ok(object)
}

/// `zend_throw_exception` with `EG(exception)` set: a new throwable of
/// `class` with `message`, the one `previous` holds chained as its
/// `previous` (`zend_throw_exception_internal`), as
/// `spl_multiple_iterator_get_all` wraps what a sub-iterator threw. A
/// fatal or an `exit` is handed on as it is.
///
/// ```php
/// class It extends ArrayIterator { function current(): mixed { throw new Exception("inner"); } }
/// $m = new MultipleIterator; $m->attachIterator(new It([1]));
/// try { $m->current(); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), " / ", $e->getPrevious()?->getMessage(), "\n"; }
/// ```
/// ```expect
/// RuntimeException: Failed to call sub iterator method / inner
/// ```
pub fn throw_chained(req: &mut Request, previous: Thrown, class: &str, message: &str) -> Thrown {
    let Some(earlier) = previous.exception().copied() else {
        return previous;
    };
    match new_throwable(req, class, message) {
        Ok(throwable) => {
            if let Some(object) = throwable.as_object().copied() {
                req.set_previous(object, earlier);
            }
            let thrown = throw_value(req, &throwable);
            req.release(throwable);
            thrown
        }
        Err(refused) => {
            req.release(earlier.as_value());
            refused
        }
    }
}

/// `zend_throw_error` where `EG(exception)` may already be set, as after
/// a resolution a deprecation's handler threw through: the new
/// throwable with the pending one taken off the request and chained as
/// its `previous` (`zend_throw_exception_internal`), or alone where
/// none is pending. An `exit` or a fatal standing is handed on as it is.
pub(crate) fn throw_over_pending(req: &mut Request, class: &str, message: &str) -> Thrown {
    match req.take_pending() {
        Some(earlier) => throw_chained(req, Thrown::of(Some(earlier)), class, message),
        None if req.is_pending() => Thrown::of(None),
        None => throw(req, class, message),
    }
}

/// Make a throwable pending, `zend_throw_exception_object`: one more
/// owner of the object, the pending slot's, as `ZEND_THROW` takes one
/// before it frees its operand; the caller keeps its own. A value that
/// is not an object becomes an `Error`.
pub fn throw_value(req: &mut Request, throwable: &Value) -> Thrown {
    if let Some(object) = throwable.as_object() {
        req.addref(throwable);
        return Thrown::of(Some(*object));
    }
    throw(
        req,
        "Error",
        "Cannot throw objects that do not implement Throwable",
    )
}

/// A new instance of the throwable class `class` with `message`, not yet
/// pending. `zend_default_exception_new` records where it was made, and
/// that is the site an uncaught one names. Its strings are the
/// request's, and the `Err` is the memory limit refusing them. The
/// message is the C string `zend_throw_exception` is handed, so it ends
/// at its first NUL, as a `%c` of zero ends `sscanf()`'s:
///
/// ```php
/// try { sscanf("x", "%"); } catch (ValueError $e) { var_dump($e->getMessage()); }
/// ```
/// ```expect
/// string(31) "Bad scan conversion character ""
/// ```
///
/// The site and its trace are recorded once, as the object is made
/// ([`Request::new_object`]), so the `SensitiveParameterValue` a trace
/// puts in place of a marked argument is made once, after the
/// throwable and before anything the program makes next:
///
/// ```php
/// function h(#[\SensitiveParameter] $x) { strlen(); }
/// try { h('hunter2'); } catch (ArgumentCountError $e) {
///     var_dump(spl_object_id($e), spl_object_id(new stdClass), $e->getTrace()[1]['args'][0]);
/// }
/// ```
/// ```expect
/// int(1)
/// int(3)
/// object(SensitiveParameterValue)#2 (0) {
/// }
/// ```
pub fn new_throwable(req: &mut Request, class: &str, message: &str) -> Result<Value, Thrown> {
    let object = req.new_object_of(class)?;
    let name = PhpString::new(req, b"message")?;
    let message = message.split('\0').next().unwrap_or_default();
    let message = PhpString::new(req, message.as_bytes())?;
    object.set_prop(req, name, Value::string(message))?;
    if !crate::classes::records_throw_site(req.handlers_of(object.class())) {
        req.note_throw_site(object)?;
    }
    Ok(object.as_value())
}

/// `zend_argument_type_error`: `TypeError` for the argument at
/// `position`, `{fn}(): Argument #N ($name) must {tail}`. The function's
/// name and the parameter's are the active native's declaration, as
/// `get_active_function_arg_name` reads them off `arg_info`, so the
/// caller's spelling of the call is not what the message says and a
/// native never repeats a name it declared. The trace under it prints
/// the argument off the call frame, as php prints what the frame holds.
///
/// ```php
/// Set_Time_Limit('x');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: set_time_limit(): Argument #1 ($seconds) must be of type int, string given in %s:2
/// Stack trace:
/// #0 %s(2): set_time_limit('x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub fn arg_type_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    let message = req.argument_message(position, tail);
    throw(req, "TypeError", &message)
}

/// `zend_argument_value_error`: `ValueError` for the argument at
/// `position`, the same sentence under the other class. No native this
/// engine declares raises one; `ext/standard`'s `str_repeat` does, and
/// its witness pins the rendering. The text is held here by
/// `a_value_error_names_the_parameter` below.
pub fn arg_value_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    let message = req.argument_message(position, tail);
    throw(req, "ValueError", &message)
}

/// `zend_argument_error`: a throwable of `class` for the argument at
/// `position`, the sentence the two above word, `fn(): Argument #N
/// ($name) <tail>` with the tail as it stands, which is what SPL throws
/// its `OutOfRangeException` for an index with.
///
/// ```php
/// $l = new SplDoublyLinkedList;
/// try { $l->offsetGet(3); } catch (OutOfRangeException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// SplDoublyLinkedList::offsetGet(): Argument #1 ($index) is out of range
/// ```
pub fn argument_error(req: &mut Request, class: &str, position: u32, tail: &str) -> Thrown {
    let message = req.argument_message(position, "");
    let message = format!("{}{tail}", message.trim_end_matches("must "));
    throw(req, class, &message)
}

/// `ArgumentCountError`: `fn() expects <tail>`.
pub fn arg_count_error(req: &mut Request, tail: &str) -> Thrown {
    let message = format!("{}() expects {tail}", req.active_native());
    throw(req, "ArgumentCountError", &message)
}

/// `zend_wrong_parameters_count_error` (`Z/Zend/zend_API.c:231`): the
/// call's argument count held to `min..=max`, `max` of `None` unbounded
/// as a `"+"` spec is, and outside it php's `ArgumentCountError` in its
/// three sentences, `exactly`, `at least` or `at most` by where the
/// count fell. A declared native's count is checked before its body; an
/// `arity = manual` native checks its own here, after the step php
/// takes first: `assert`'s active check, the `"+f"` set natives' split
/// of their trailing callbacks, `strtr`'s parse chosen by the count,
/// `stream_context_set_option`'s deprecation, the form
/// `session_set_save_handler` tells apart by the count, and the SPL
/// outer iterators' `spl_dual_it_construct`.
///
/// ```php
/// foreach ([fn () => assert(), fn () => array_udiff([1]), fn () => array_uintersect_uassoc([1], 'strcmp'), fn () => strtr("abc", "a", "b", "c"), fn () => stream_context_set_option()] as $f) {
///     try { $f(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// assert() expects at least 1 argument, 0 given
/// array_udiff() expects at least 2 arguments, 1 given
/// array_uintersect_uassoc() expects at least 3 arguments, 2 given
/// strtr() expects exactly 3 arguments, 4 given
/// stream_context_set_option() expects at least 2 arguments, 0 given
/// ```
///
/// ```php
/// foreach ([fn () => new IteratorIterator(), fn () => new LimitIterator(new ArrayIterator([]), 0, 1, 2), fn () => session_set_save_handler(), fn () => @session_set_save_handler(1, 2, 3)] as $f) {
///     try { $f(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// IteratorIterator::__construct() expects at least 1 argument, 0 given
/// LimitIterator::__construct() expects at most 3 arguments, 4 given
/// session_set_save_handler() expects at least 1 argument, 0 given
/// session_set_save_handler() expects at least 6 arguments, 3 given
/// ```
pub fn arity(req: &mut Request, min: usize, max: Option<usize>) -> Result<(), Thrown> {
    let given = req.argument_count();
    if given >= min && max.is_none_or(|max| given <= max) {
        return Ok(());
    }
    let (word, bound) = match max {
        Some(max) if max == min => ("exactly", min),
        _ if given < min => ("at least", min),
        Some(max) => ("at most", max),
        None => ("at least", min),
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {given} given"),
    ))
}

/// `ArgumentCountError` for a call with fewer arguments than the
/// declaration requires, `zend_wrong_parameters_count_error`.
pub fn too_few_arguments(req: &mut Request, expected: u32, given: u32) -> Thrown {
    let word = if expected == 1 {
        "argument"
    } else {
        "arguments"
    };
    let message = format!(
        "{}() expects at least {expected} {word}, {given} given",
        req.active_native()
    );
    throw(req, "ArgumentCountError", &message)
}

/// `zend_param_must_be_nullable`: the 8.1 deprecation for `null` passed
/// to a non-nullable scalar parameter of a native, one sentence,
/// `{fn}(): Passing null to parameter #N ($name) of type {type} is
/// deprecated`, the name read off the declaration as the type error's
/// is.
///
/// ```php
/// var_dump(ini_set(null, '1'));
/// ```
/// ```expect
/// Deprecated: ini_set(): Passing null to parameter #1 ($option) of type string is deprecated in %s on line 2
/// bool(false)
/// ```
pub fn arg_null_deprecated(req: &mut Request, position: u32, type_name: &str) {
    let text = req.null_parameter_message(position, type_name);
    deprecated(req, &text);
}

/// `ZEND_FUNCTION(exit)` past its parameter parsing
/// (`Z/Zend/zend_builtin_functions.c:118`): a `string` status is
/// written, and one that is not empty may reach a buffer's handler
/// that throws, which is then what the native returns with; an `int` status
/// is php's `EG(exit_status)`, the `int` it is in C, so a code past
/// 32 bits keeps its low ones. Then the unwind exit is thrown
/// ([`Request::throw_unwind_exit`]), which no `catch` matches and no
/// `finally` block is entered for, and the `Thrown` carries no
/// throwable. Every other value is the caller's to coerce or refuse
/// first, as the native `exit` does.
pub fn exit(req: &mut Request, status: &Value) -> Thrown {
    if let Some(text) = status.as_str_bytes() {
        if !text.is_empty() {
            req.write(text);
            if req.is_pending() {
                return Thrown::of(None);
            }
        }
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let code = status.as_int().unwrap_or(0) as i32;
        req.end_in_exit(code);
    }
    req.throw_unwind_exit();
    Thrown::of(None)
}

/// `zend_clear_exception`: the throwable a call left pending taken off
/// the request and given up, as SPL's `CATCH_GET_CHILD` swallows what a
/// child's `getChildren()` threw. A fatal or an `exit` is not a throwable
/// and cannot be cleared; it comes back as the `Err` to hand on.
///
/// ```php
/// class Bad extends RecursiveArrayIterator {
///     function getChildren(): ?RecursiveArrayIterator { throw new Exception("no children"); }
/// }
/// $it = new RecursiveIteratorIterator(new Bad([1, [2], 3]), RecursiveIteratorIterator::LEAVES_ONLY, RecursiveIteratorIterator::CATCH_GET_CHILD);
/// foreach ($it as $v) echo $v, ";";
/// ```
/// ```expect
/// 1;3;
/// ```
pub fn clear(req: &mut Request, thrown: Thrown) -> Result<(), Thrown> {
    let Some(exception) = thrown.exception().copied() else {
        return Err(thrown);
    };
    if req.pending() == Some(exception) {
        req.take_pending();
    }
    req.release(exception.as_value());
    Ok(())
}

/// `if (EG(exception))` after a step that may call PHP code and answers
/// nothing, such as a release whose destructor threw: what the step
/// left pending, taken off the request as the `Err`, as a call hands
/// back what its callee threw, and an `exit` unwinding as the `Err` too.
///
/// ```php
/// class D extends RecursiveArrayIterator { function __destruct() { throw new Exception("dtor " . count($this)); } }
/// try {
///     foreach (new RecursiveIteratorIterator(new D([[1]])) as $v) echo $v, "\n";
/// } catch (Exception $e) { do { echo $e->getMessage(), "\n"; } while ($e = $e->getPrevious()); }
/// ```
/// ```expect
/// 1
/// dtor 1
/// dtor 1
/// ```
pub fn pending(req: &mut Request) -> Result<(), Thrown> {
    match req.take_pending() {
        Some(pending) => Err(Thrown::of(Some(pending))),
        None if req.is_pending() => Err(Thrown::of(None)),
        None => Ok(()),
    }
}

/// `zend_replace_error_handling(EH_THROW, class)` around `f`: a warning
/// raised meanwhile is thrown as `class` with the warning's message and
/// shown nowhere, the first one only, and the handling `f` found is
/// restored after it, which is how SPL's file classes turn a stream's
/// warning into their `RuntimeException`. A throwable left pending by a
/// warning is the `Err`, whatever `f` answered; where `f` threw one of
/// its own after it, that one is the `Err` with the warning's chained as
/// its `previous`, as `zend_throw_exception_internal` chains a new
/// throwable onto `EG(exception)`.
///
/// ```php
/// try { new SplFileObject(__DIR__ . '/no-such-file'); }
/// catch (RuntimeException $e) { echo get_class($e), ': ', str_replace(__DIR__, '', $e->getMessage()), "\n"; }
/// ```
/// ```expect
/// RuntimeException: SplFileObject::__construct(/no-such-file): Failed to open stream: No such file or directory
/// ```
pub fn throwing_warnings<R>(
    req: &mut Request,
    class: &str,
    f: impl FnOnce(&mut Request) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let previous = req.replace_error_handling(Some(class.to_owned()));
    let answered = f(req);
    req.replace_error_handling(previous);
    match (answered, req.take_error_handling_thrown()) {
        (Err(later), Some(first)) => match (later.exception().copied(), first.exception().copied())
        {
            (Some(outer), Some(inner)) => {
                req.set_previous(outer, inner);
                Err(later)
            }
            // A fatal or an exit thrown after the warning ends it.
            (None, Some(inner)) => {
                req.release(inner.as_value());
                Err(later)
            }
            (Some(outer), None) => {
                req.release(outer.as_value());
                Err(first)
            }
            (None, None) => Err(first),
        },
        (_, Some(thrown)) => Err(thrown),
        (answered, None) => answered,
    }
}

#[cfg(test)]
mod tests {
    use php_unit::decl::{FunctionDecl, Name, Parameter, ParameterDefault, Type};

    use crate::api::abi::{Entry, ExtensionBuilder};
    use crate::api::request::Request;
    use crate::api::values::Value;
    use crate::engine::Engine;
    use crate::test::Capture;

    use super::{Thrown, arg_type_error, arg_value_error, new_throwable, throw, throw_at};

    fn parameter(name: &[u8], ty: Type, variadic: bool) -> Parameter {
        Parameter {
            name: Name(name.into()),
            ty: Some(ty),
            by_ref: false,
            prefer_ref: false,
            promoted: false,
            variadic,
            default: ParameterDefault::None,
            attributes: Vec::new(),
        }
    }

    fn declared(name: &[u8], parameters: Vec<Parameter>) -> FunctionDecl {
        FunctionDecl {
            name: Name(name.into()),
            parameters,
            return_type: None,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        }
    }

    /// `str_repeat(string $string, int $times)` as far as its one
    /// `ValueError` goes.
    fn str_repeat(req: &mut Request) -> Result<Value, Thrown> {
        Err(arg_value_error(req, 2, "be greater than or equal to 0"))
    }

    /// `array_merge(array ...$arrays)` as far as its `TypeError` goes.
    fn array_merge(req: &mut Request) -> Result<Value, Thrown> {
        Err(arg_type_error(req, 1, "be of type array, int given"))
    }

    /// What `token_get_all("<?php if (", TOKEN_PARSE)` throws: a
    /// `ParseError` at the compiled site, the empty file name and the
    /// scanner's line.
    fn refuse(req: &mut Request) -> Result<Value, Thrown> {
        Err(throw_at(req, "ParseError", "Unclosed '('", b"", 1))
    }

    /// A native whose body throws a class the bundle may not link, as
    /// `php_macros`' parity test met it (#970).
    fn unlinked(req: &mut Request) -> Result<Value, Thrown> {
        Err(throw(req, "LogicException", "never returns"))
    }

    /// The same class made and not thrown.
    fn unlinked_made(req: &mut Request) -> Result<Value, Thrown> {
        new_throwable(req, "LogicException", "never made")
    }

    /// The fatal a source comes to under an engine with the natives
    /// above declared beside the core's.
    fn fatal_of(source: &str) -> Option<String> {
        served(source).fatal
    }

    fn served(source: &str) -> crate::test::Served {
        let extension = ExtensionBuilder::default()
            .functions(vec![
                (
                    declared(
                        b"str_repeat",
                        vec![
                            parameter(b"string", Type::String, false),
                            parameter(b"times", Type::Int, false),
                        ],
                    ),
                    str_repeat as Entry,
                ),
                (
                    declared(
                        b"array_merge",
                        vec![parameter(b"arrays", Type::Array, true)],
                    ),
                    array_merge as Entry,
                ),
                (declared(b"refuse", Vec::new()), refuse as Entry),
                (declared(b"unlinked", Vec::new()), unlinked as Entry),
                (
                    declared(b"unlinked_made", Vec::new()),
                    unlinked_made as Entry,
                ),
            ])
            .build("raising");
        Engine::builder()
            .extensions(&[extension])
            .build()
            .serve_capture(source, Capture::default())
    }

    /// `zend_argument_type_error` under the pinned oracle, the name off
    /// the declaration whatever case the call spelled:
    ///
    /// ```text
    /// $ php -n -r 'Set_Time_Limit("x");'
    /// Fatal error: Uncaught TypeError: set_time_limit(): Argument #1 ($seconds) must be of type int, string given in Command line code:1
    /// ```
    #[test]
    fn a_type_error_names_the_parameter_off_the_declaration() {
        let fatal = fatal_of("<?php Set_Time_Limit('x');");
        assert_eq!(
            fatal.as_deref().map(|f| f.contains(
                "TypeError: set_time_limit(): Argument #1 ($seconds) must be of type int, string given"
            )),
            Some(true),
            "{fatal:?}"
        );
    }

    /// `zend_argument_value_error` under the pinned oracle:
    ///
    /// ```text
    /// $ php -n -r 'str_repeat("x", -1);'
    /// Fatal error: Uncaught ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0 in Command line code:1
    /// ```
    #[test]
    fn a_value_error_names_the_parameter() {
        let fatal = fatal_of("<?php str_repeat('x', -1);");
        assert_eq!(
            fatal.as_deref().map(|f| f.contains(
                "ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0"
            )),
            Some(true),
            "{fatal:?}"
        );
    }

    /// `get_function_arg_name` answers no name past `num_args`, which is
    /// where php keeps the variadic's `arg_info`, so an argument at a
    /// variadic slot is `Argument #N` alone. The pinned oracle:
    ///
    /// ```text
    /// $ php -n -r 'array_merge(1);'
    /// Fatal error: Uncaught TypeError: array_merge(): Argument #1 must be of type array, int given in Command line code:1
    /// ```
    #[test]
    fn an_argument_at_a_variadic_slot_has_no_name() {
        let fatal = fatal_of("<?php array_merge(1);");
        assert_eq!(
            fatal.as_deref().map(|f| f.contains(
                "TypeError: array_merge(): Argument #1 must be of type array, int given"
            )),
            Some(true),
            "{fatal:?}"
        );
    }

    /// `zend_exception_error` on an uncaught `ParseError`: the message as
    /// `E_PARSE` at the throwable's own site, no `Uncaught` and no trace,
    /// and the request ends at 255 with nothing after it. The pinned
    /// oracle:
    ///
    /// ```text
    /// $ php -n -r 'token_get_all("<?php if (", TOKEN_PARSE); echo "after";'
    /// Parse error: Unclosed '(' in  on line 1
    /// ```
    #[test]
    fn a_parse_error_is_rendered_as_its_level() {
        let served = served("<?php refuse(); echo 'after';");
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            "\nParse error: Unclosed '(' in  on line 1\n"
        );
        assert_eq!(served.exit_code, 255);
        assert_eq!(served.fatal, None);
    }

    /// A raiser naming a class the bundle does not link, `spl`'s
    /// `LogicException` under an engine that links only the natives
    /// above, ends the request in the `InternalError` fatal naming the
    /// class, where it threw an object of whatever class held id zero,
    /// its properties written as dynamic ones. php has no such call:
    /// `zend_throw_error` takes a class entry, so nothing throws a
    /// class nothing registered.
    #[test]
    fn a_raiser_naming_an_unlinked_class_is_the_internal_error_naming_it() {
        for native in ["unlinked", "unlinked_made"] {
            let served = served(&format!("<?php {native}(); echo 'after';"));
            let stdout = String::from_utf8_lossy(&served.stdout);
            assert_eq!(
                stdout,
                format!(
                    "\nFatal error: InternalError: the class LogicException is not linked into this engine in Standard input code on line 1\nStack trace:\n#0 Standard input code(1): {native}()\n#1 {{main}}\n"
                ),
                "{native}"
            );
            assert_eq!(served.exit_code, 255, "{native}");
        }
    }
}
