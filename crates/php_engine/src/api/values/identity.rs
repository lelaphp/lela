//! A counted value told apart by its place, as php compares
//! `zend_refcounted *`: what a debugger's watch list keeps of a value it
//! copied, to say whether the value it reads after a step is the one it
//! copied from. The face compares strings by their bytes, so the
//! place is a type of its own, holding no owner and never read through.

use php_heap::{Arr, Obj, Ref, Str};

use super::Value;

/// What a counted value points to: the string, array, object or
/// reference by its place, as php compares `zend_refcounted *`. It
/// holds no owner, so a place freed and taken again reads as the same,
/// as php's pointer does.
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
