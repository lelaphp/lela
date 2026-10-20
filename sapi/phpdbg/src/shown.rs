//! What the debugger keeps of a value it reads between steps, and the
//! rows its `info` and `back` commands print (#1799): each value the
//! native face hands out is a view of what the request holds, copied here
//! into a [`Shown`] before the request runs code again, so the watch
//! list can compare it from one step to the next and `info vars` can
//! print its owners as php's `Z_REFCOUNT` counts them. php's phpdbg
//! reads the zvals themselves; this copy is what stands in for holding
//! one across a step.

use php_ext::request::Request;
use php_ext::values::walk::rows;
use php_ext::values::{Identity, Key as ValueKey, TypeTag, Value};
use php_sapi::tier::{self, FrameRef};

use crate::read::{self, Thrown};

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

/// What stands pending on the request as php's phpdbg reads it off the
/// throwable (`phpdbg_prompt.c:1690-1696`): its class, and its `file`,
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

/// `value` copied, its arrays to `depth` levels; an array that holds
/// itself is copied without its elements. Read under the borrowed
/// request ([`rows`]): nothing here runs code of the program.
fn shown(req: &Request, value: Value, depth: u32) -> Shown {
    match value.tag() {
        TypeTag::Undef => Shown::Undef,
        TypeTag::Null | TypeTag::Number => Shown::Null,
        TypeTag::Bool => Shown::Bool(value.as_bool().unwrap_or(false)),
        TypeTag::Int => Shown::Int(value.as_int().unwrap_or(0)),
        TypeTag::Float => Shown::Float(value.as_float().unwrap_or(0.0)),
        TypeTag::String => Shown::Str {
            bytes: value.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default(),
            id: Identity::of(value),
            counted: value.is_refcounted(),
        },
        TypeTag::Array => {
            let Some(array) = value.as_array() else {
                return Shown::Null;
            };
            let entries = if depth == 0 || array.is_recursive() {
                Vec::new()
            } else {
                array.protect();
                let copied = rows(req, array)
                    .map(|(key, value)| (key_copied(&key), shown(req, value.deref(), depth - 1)))
                    .collect();
                array.unprotect();
                copied
            };
            Shown::Array {
                id: Identity::of(value),
                counted: value.is_refcounted(),
                len: array.len(),
                entries,
            }
        }
        TypeTag::Object => {
            let Some(object) = value.as_object() else {
                return Shown::Null;
            };
            Shown::Object {
                id: Identity::of(value),
                class: read::class_of(req, value).unwrap_or_default(),
                handle: object.index(),
            }
        }
        TypeTag::Resource => {
            let Some(resource) = value.as_resource() else {
                return Shown::Null;
            };
            Shown::Resource {
                handle: resource.id(),
            }
        }
        TypeTag::Reference => Shown::Reference {
            id: Identity::of(value),
            value: Box::new(shown(req, value.deref(), depth)),
        },
    }
}

/// An array's key, copied.
fn key_copied(key: &ValueKey) -> Key {
    match key {
        ValueKey::Int(n) => Key::Int(*n),
        ValueKey::Str(s) => Key::Str(s.as_bytes().to_vec()),
    }
}

/// Each row with its owners, its value copied to `depth`.
fn variable_rows(req: &mut Request, read: Vec<(Vec<u8>, Value)>, depth: u32) -> Vec<Variable> {
    read.into_iter()
        .map(|(name, value)| Variable {
            refs: value.refcount(),
            value: shown(req, value, depth),
            name,
        })
        .collect()
}

/// php's auto-globals, `CG(auto_globals)` in the order php registers
/// them (`php_startup_auto_globals`, then ext/session's `_SESSION`):
/// what `zend_is_auto_global` knows, and the seven phpdbg's `info
/// globals` arms before it walks the global table.
const AUTO_GLOBALS: [&[u8]; 8] = [
    b"_GET",
    b"_POST",
    b"_COOKIE",
    b"_SERVER",
    b"_ENV",
    b"_REQUEST",
    b"_FILES",
    b"_SESSION",
];

/// The frame a step handed out, as the native face keeps one.
fn frame_id(req: &Request, frame: FrameRef) -> Option<php_ext::request::FrameId> {
    tier::frame(req, frame).map(|view| view.id())
}

/// The variable `name` of `frame`, copied to `depth`: what its symbol
/// table holds under the name, or the compiled variable of that name
/// where it has no table, so no table is built for the read.
pub fn variable(req: &mut Request, frame: FrameRef, name: &[u8], depth: u32) -> Option<Shown> {
    let id = frame_id(req, frame)?;
    let value = req.frames_mut().var(id, name)?;
    Some(shown(req, value, depth))
}

/// The variables of `frame` as phpdbg's `info vars` lists them,
/// `zend_rebuild_symbol_table`'s rows without the auto-globals, each one
/// level deep.
pub fn variables(req: &mut Request, frame: FrameRef) -> Vec<Variable> {
    let read = frame_id(req, frame)
        .and_then(|id| req.frames_mut().vars(id))
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, _)| !AUTO_GLOBALS.contains(&name.as_slice()))
        .collect();
    variable_rows(req, read, 1)
}

/// The superglobals as phpdbg's `info globals` lists them, each armed
/// first and read in the global table's order, one level deep.
pub fn superglobals(req: &mut Request) -> Vec<Variable> {
    for name in &AUTO_GLOBALS[..7] {
        req.variables_mut().superglobal(name);
    }
    let read = req
        .variables_mut()
        .global_names()
        .into_iter()
        .filter(|name| AUTO_GLOBALS.contains(&name.as_slice()))
        .filter_map(|name| {
            let value = req.variables_mut().global(&name)?;
            Some((name, value))
        })
        .collect();
    variable_rows(req, read, 1)
}

/// The constants the program defined, `PHP_USER_CONSTANT`, as phpdbg's
/// `info constants` lists them in the table's order.
pub fn user_constants(req: &mut Request) -> Vec<Variable> {
    let read = req
        .symbols()
        .constant_table()
        .into_iter()
        .filter(|(extension, _, _)| extension.is_none())
        .map(|(_, name, value)| (name.to_vec(), value))
        .collect();
    variable_rows(req, read, 0)
}

/// What stands pending on the request, read off it ([`read::pending`]).
pub fn pending(req: &Request) -> Option<Pending> {
    let object = match read::pending(req)? {
        Thrown::GracefulExit => {
            return Some(Pending {
                id: u32::MAX,
                class: b"GracefulExit".to_vec(),
                file: Vec::new(),
                line: 0,
                message: Vec::new(),
            });
        }
        Thrown::Object(object) => object,
    };
    let throwable = object.as_object()?;
    let prop = |name: &[u8]| throwable.prop(req, name);
    let bytes = |name: &[u8]| {
        prop(name)
            .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default()
    };
    Some(Pending {
        id: throwable.index(),
        class: read::class_of(req, object).unwrap_or_default(),
        file: bytes(b"file"),
        line: prop(b"line").and_then(|v| v.as_int()).unwrap_or(0),
        message: bytes(b"message"),
    })
}

/// The literals of the op array `frame` serves, `op_array->literals` in
/// order.
pub fn literals(req: &mut Request, frame: FrameRef) -> Vec<Shown> {
    let Some(code) = tier::frame(req, frame).and_then(|view| view.op_array()) else {
        return Vec::new();
    };
    (0..)
        .map_while(|at| code.constant(php_ext::request::ConstIdx(at)))
        .map(|value| shown(req, value, 0))
        .collect()
}

/// The rows of the chain from the innermost frame down, `limit` of them
/// where it is not zero, each read back out of the array
/// `zend_fetch_debug_backtrace(&trace, 0, 0, limit)` builds by its keys,
/// and the array given up after, as phpdbg's `back` builds and destroys
/// it; none where building it met the memory limit.
pub fn backtrace(req: &mut Request, limit: i64) -> Vec<TraceRow> {
    let Ok(trace) = req.frames_mut().backtrace_here(0, limit) else {
        return Vec::new();
    };
    let read = {
        let trace = *trace;
        let req: &Request = req;
        let mut read = Vec::new();
        for (_, row) in rows(req, trace) {
            let Some(row) = row.as_array() else {
                continue;
            };
            let field = |name: &[u8]| {
                rows(req, row).find_map(|(key, value)| match key {
                    ValueKey::Str(s) if s.as_bytes() == name => Some(value.deref()),
                    _ => None,
                })
            };
            let bytes =
                |value: Option<Value>| value.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
            let args = field(b"args")
                .and_then(|v| v.as_array())
                .map(|args| {
                    rows(req, args)
                        .map(|(_, v)| shown(req, v.deref(), 1))
                        .collect()
                })
                .unwrap_or_default();
            read.push(TraceRow {
                function: bytes(field(b"function")).unwrap_or_default(),
                class: bytes(field(b"class")).zip(bytes(field(b"type"))),
                file: bytes(field(b"file")),
                line: field(b"line").and_then(|v| v.as_int()),
                args,
            });
        }
        read
    };
    trace.give_up(req);
    read
}
