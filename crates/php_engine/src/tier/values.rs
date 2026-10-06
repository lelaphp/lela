//! The values a tier reads between the steps it drives: a variable of a
//! frame, the superglobals, the constants a program defined, an op
//! array's literals, the backtrace. Each is handed out as the [`Value`]
//! the request holds, a view that takes no owner, so what the debugging
//! host prints is the request's own refcount; the host copies it into
//! its own shape before the request runs code again, and keeps of it
//! only what [`Identity`] says it was copied from. How a debugger shows
//! a value is the host's, as php's phpdbg prints its own.

use php_heap::{Arr, FrameRef, Obj, Ref, Str};

use crate::api::request::Request;
use crate::api::values::Value;

/// What a counted value points to: the string, array, object or
/// reference by its place, as php compares `zend_refcounted *`. It
/// holds no owner, so a place freed and taken again reads as the same.
/// The face the extensions read compares strings by their bytes, so the
/// place is the tier's to tell.
#[derive(Debug, Clone, Copy, Default)]
pub struct Identity(Of);

#[derive(Debug, Clone, Copy, Default)]
enum Of {
    #[default]
    None,
    Str(Str),
    Arr(Arr),
    Obj(Obj),
    Ref(Ref),
}

impl PartialEq for Identity {
    fn eq(&self, other: &Identity) -> bool {
        match (self.0, other.0) {
            (Of::None, Of::None) => true,
            (Of::Str(a), Of::Str(b)) => a.is_same(b),
            (Of::Arr(a), Of::Arr(b)) => a.is_same(b),
            (Of::Obj(a), Of::Obj(b)) => a.is_same(b),
            (Of::Ref(a), Of::Ref(b)) => a.is_same(b),
            _ => false,
        }
    }
}

impl Identity {
    /// The place `value` points to; none for a value that points to
    /// nothing.
    #[must_use]
    pub fn of(value: Value) -> Identity {
        let zval = value.to_zval();
        if let Some(s) = zval.as_str() {
            return Identity(Of::Str(s));
        }
        if let Some(a) = zval.as_arr() {
            return Identity(Of::Arr(a));
        }
        if let Some(o) = zval.as_obj() {
            return Identity(Of::Obj(o));
        }
        if let Some(r) = zval.as_reference() {
            return Identity(Of::Ref(r));
        }
        Identity(Of::None)
    }
}

/// The name of an object's class as a message prints it, an anonymous
/// class's as `class@anonymous`; none for a value that is no object.
#[must_use]
pub fn class_of(req: &Request, value: Value) -> Option<Vec<u8>> {
    value
        .as_object()
        .map(|object| req.class_name(object.class()).into_bytes())
}

/// The variable `name` of `frame`: what its symbol table holds under the
/// name, or the compiled variable of that name where the frame has no
/// table; `UNDEF` where the name is one and holds nothing, none where
/// the frame knows no such name. A view.
pub fn variable(req: &mut Request, frame: FrameRef, name: &[u8]) -> Option<Value> {
    req.frame_variable(frame, name)
}

/// `zend_rebuild_symbol_table` over `frame`, and each name that holds a
/// value, in the table's order: what phpdbg's `info vars` walks, the
/// superglobals left out. Views.
pub fn variables(req: &mut Request, frame: FrameRef) -> Vec<(Vec<u8>, Value)> {
    req.frame_variables(frame)
}

/// The superglobals of `EG(symbol_table)`, each armed first, in the
/// table's order: what phpdbg's `info globals` walks. Views.
pub fn superglobals(req: &mut Request) -> Vec<(Vec<u8>, Value)> {
    req.superglobal_rows()
}

/// The constants the program defined, `PHP_USER_CONSTANT`, in the
/// table's order: what phpdbg's `info constants` walks. Views.
#[must_use]
pub fn user_constants(req: &Request) -> Vec<(Vec<u8>, Value)> {
    req.constant_table()
        .into_iter()
        .filter(|(module, _, _)| module.is_none())
        .map(|(_, name, value)| (name, value))
        .collect()
}

/// The literals of the op array `frame` serves, `op_array->literals` in
/// order. Views.
#[must_use]
pub fn literals(req: &Request, frame: FrameRef) -> Vec<Value> {
    req.frame_literals(frame)
}

/// `EG(included_files)` in its order.
#[must_use]
pub fn included_files(req: &Request) -> Vec<Vec<u8>> {
    req.included_files()
}

/// `zend_fetch_debug_backtrace(&trace, 0, 0, limit)`, the array of the
/// chain's rows from the innermost frame down, `limit` of them where it
/// is not zero, lent to `read` and released after it, as phpdbg's
/// `back` builds and destroys it; none where building it met the memory
/// limit.
pub fn backtrace<R>(
    req: &mut Request,
    limit: i64,
    read: impl FnOnce(&mut Request, Value) -> R,
) -> Option<R> {
    let trace = Value::array(req.fetch_debug_backtrace(false, 0, limit).ok()?);
    let answered = read(req, trace);
    req.release(trace);
    Some(answered)
}

/// The last error the request raised: its message, file and line.
#[must_use]
pub fn last_error(req: &Request) -> Option<(Vec<u8>, Vec<u8>, u32)> {
    req.last_error_row()
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
pub fn set_stdin(req: &mut Request, stdin: std::os::fd::OwnedFd) {
    req.reregister_file_handles(stdin).ok();
}
