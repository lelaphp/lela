//! The values a tier reads between the steps it drives: a variable of a
//! frame, the superglobals, the constants a program defined, an op
//! array's literals, the backtrace, each copied off the request as a
//! [`Shown`], which a debugger prints in its own formats and compares
//! from one step to the next. A copy names what it was copied from by
//! an [`Identity`], which tells two strings, arrays, objects or
//! references apart as php's pointers do, holding no owner of them.

use php_heap::{Arr, FrameRef, Obj, Ref, Str, Zval};

use crate::api::request::Request;
use crate::api::values::{Key as ValueKey, TypeTag, Value};

/// What a counted value was copied from: the string, array, object or
/// reference by its place, as php compares `zend_refcounted *`. It
/// holds no owner, so a place freed and taken again reads as the same.
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
    fn of(zval: Zval) -> Identity {
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

/// A key of an array, or a property's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Int(i64),
    Str(Vec<u8>),
}

/// A value copied off the request.
#[derive(Debug, Clone, PartialEq)]
pub enum Shown {
    Undef,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str {
        bytes: Vec<u8>,
        id: Identity,
        /// `Z_REFCOUNTED`: an interned string is not.
        counted: bool,
    },
    Array {
        id: Identity,
        counted: bool,
        len: usize,
        /// The elements, down to the depth asked; none past it.
        entries: Vec<(Key, Shown)>,
    },
    Object {
        id: Identity,
        class: Vec<u8>,
        handle: u32,
    },
    Resource {
        handle: i64,
    },
    Reference {
        id: Identity,
        value: Box<Shown>,
    },
}

impl Shown {
    /// `Z_REFCOUNTED`.
    #[must_use]
    pub fn counted(&self) -> bool {
        match self {
            Shown::Str { counted, .. } | Shown::Array { counted, .. } => *counted,
            Shown::Object { .. } | Shown::Resource { .. } | Shown::Reference { .. } => true,
            _ => false,
        }
    }
}

/// A named value and its owners, as `info vars` lists one.
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    pub name: Vec<u8>,
    /// `Z_REFCOUNT`, one for a value that counts none.
    pub refs: u32,
    pub value: Shown,
}

/// `value` copied, its arrays to `depth` levels.
pub(crate) fn shown(req: &mut Request, value: Value, depth: u32) -> Shown {
    let zval = value.to_zval();
    match value.tag() {
        TypeTag::Undef => Shown::Undef,
        TypeTag::Null | TypeTag::Number => Shown::Null,
        TypeTag::Bool => Shown::Bool(value.as_bool().unwrap_or(false)),
        TypeTag::Int => Shown::Int(value.as_int().unwrap_or(0)),
        TypeTag::Float => Shown::Float(value.as_float().unwrap_or(0.0)),
        TypeTag::String => Shown::Str {
            bytes: value.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default(),
            id: Identity::of(zval),
            counted: zval.is_refcounted(),
        },
        TypeTag::Array => {
            let Some(array) = value.as_array().copied() else {
                return Shown::Null;
            };
            let entries = if depth == 0 || array.is_recursive() {
                Vec::new()
            } else {
                array.protect();
                let rows = array
                    .snapshot()
                    .into_iter()
                    .map(|(key, value)| (key_of(&key), shown(req, value, depth - 1)))
                    .collect();
                array.unprotect();
                rows
            };
            Shown::Array {
                id: Identity::of(zval),
                counted: zval.is_refcounted(),
                len: array.len(),
                entries,
            }
        }
        TypeTag::Object => {
            let Some(object) = value.as_object().copied() else {
                return Shown::Null;
            };
            Shown::Object {
                id: Identity::of(zval),
                class: req.class_name(object.class()).into_bytes(),
                handle: object.index(),
            }
        }
        TypeTag::Resource => Shown::Resource {
            handle: value
                .as_resource()
                .map_or(0, crate::api::values::Resource::id),
        },
        TypeTag::Reference => Shown::Reference {
            id: Identity::of(zval),
            value: Box::new(shown(req, value.deref(), depth)),
        },
    }
}

fn key_of(key: &ValueKey) -> Key {
    match key {
        ValueKey::Int(n) => Key::Int(*n),
        ValueKey::Str(s) => Key::Str(s.as_bytes().to_vec()),
    }
}

/// The variable `name` of `frame`, copied to `depth`: what its symbol
/// table holds under the name, or the compiled variable of that name
/// where the frame has no table; `UNDEF` where the name is one and holds
/// nothing, none where the frame knows no such name.
pub fn variable(req: &mut Request, frame: FrameRef, name: &[u8], depth: u32) -> Option<Shown> {
    let value = req.frame_variable(frame, name)?;
    Some(shown(req, value, depth))
}

/// `zend_rebuild_symbol_table` over `frame`, and each name that holds a
/// value, in the table's order, with its owners and its value copied one
/// level deep: what phpdbg's `info vars` walks, the superglobals left
/// out.
pub fn variables(req: &mut Request, frame: FrameRef) -> Vec<Variable> {
    let rows = req.frame_variables(frame);
    rows.into_iter()
        .map(|(name, value)| Variable {
            refs: value.refcount(),
            value: shown(req, value, 1),
            name,
        })
        .collect()
}

/// The superglobals of `EG(symbol_table)`, each armed first, in the
/// table's order: what phpdbg's `info globals` walks.
pub fn superglobals(req: &mut Request) -> Vec<Variable> {
    let rows = req.superglobal_rows();
    rows.into_iter()
        .map(|(name, value)| Variable {
            refs: value.refcount(),
            value: shown(req, value, 1),
            name,
        })
        .collect()
}

/// The constants the program defined, `PHP_USER_CONSTANT`, in the
/// table's order: what phpdbg's `info constants` walks.
pub fn user_constants(req: &mut Request) -> Vec<Variable> {
    let rows: Vec<(Vec<u8>, Value)> = req
        .constant_table()
        .into_iter()
        .filter(|(module, _, _)| module.is_none())
        .map(|(_, name, value)| (name, value))
        .collect();
    rows.into_iter()
        .map(|(name, value)| Variable {
            refs: value.refcount(),
            value: shown(req, value, 0),
            name,
        })
        .collect()
}

/// The literals of the op array `frame` serves, `op_array->literals` in
/// order.
pub fn literals(req: &mut Request, frame: FrameRef) -> Vec<Shown> {
    let values = req.frame_literals(frame);
    values
        .into_iter()
        .map(|value| shown(req, value, 0))
        .collect()
}

/// `EG(included_files)` in its order.
#[must_use]
pub fn included_files(req: &Request) -> Vec<Vec<u8>> {
    req.included_files()
}

/// One row of `zend_fetch_debug_backtrace`, as phpdbg's `back` prints it.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceRow {
    pub function: Vec<u8>,
    /// The class and `->` or `::`, for a method.
    pub class: Option<(Vec<u8>, Vec<u8>)>,
    pub file: Option<Vec<u8>>,
    pub line: Option<i64>,
    pub args: Vec<Shown>,
}

/// `zend_fetch_debug_backtrace(&trace, 0, 0, limit)`: the rows of the
/// chain from the innermost frame down, `limit` of them where it is not
/// zero.
pub fn backtrace(req: &mut Request, limit: i64) -> Vec<TraceRow> {
    req.backtrace_rows(limit)
}

/// A value copied one level deep, for the rows a request reads back.
pub(crate) fn values_shown(req: &mut Request, value: Value) -> Shown {
    shown(req, value, 1)
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
    let _ = req.build_argv(argv);
}
