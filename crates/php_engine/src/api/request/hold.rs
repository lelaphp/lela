//! One more owner of a value for the span of a call that may reach PHP
//! code: php's `GC_ADDREF` before a diagnostic, an accessor or a
//! conversion that can reach a user handler, and the `GC_DELREF`,
//! `OBJ_RELEASE` or `zend_string_release` after it. A handler may give
//! up every other owner of the array a write is landing in, the object
//! an accessor is served on or the string a conversion is reading;
//! under a [`Hold`] what it names stands until the hold is given back,
//! and what is [`Left`] then says whether the handler freed it or took
//! an owner of its own, which is the test php's handler makes after the
//! call.
//!
//! The owner is a type and not a pair of calls: [`Request::take_hold`] is the
//! one place a hold is taken and [`Request::let_go`] the one place it is
//! given up, and a `Hold` is neither `Copy` nor `Clone`, so one taken is
//! one given back.

use php_heap::Heap;

use super::{Hooks, Request};
use crate::api::values::Value;

/// One owner of a value, held across a call that may reach PHP code.
#[must_use = "a hold is an owner of the value, given back through Request::let_go"]
#[derive(Debug)]
pub(crate) struct Hold(Value);

/// What is left of a held value once the hold is given back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Left {
    /// Nothing was held: a scalar, an interned string or an immutable
    /// array, which no handler frees (php's `IS_ARRAY_IMMUTABLE` test
    /// before its `GC_ADDREF`).
    Uncounted,
    /// The owners besides the hold as they stand after it; none is the
    /// value freed as the hold was given back.
    Owners(u32),
}

impl Left {
    /// The hold was the last owner, and the value is gone:
    /// `GC_DELREF(ht) == 0`.
    pub(crate) fn freed(self) -> bool {
        self == Left::Owners(0)
    }

    /// Anything but the one owner the write started from is left, so
    /// the write lands nowhere: `GC_DELREF(ht) != 1` in
    /// `slow_index_convert_w` and `zend_undefined_index_write`.
    pub(crate) fn not_alone(self) -> bool {
        matches!(self, Left::Owners(owners) if owners != 1)
    }
}

impl Request {
    /// One more owner of `value` until [`Request::let_go`]:
    /// `GC_ADDREF`, or `GC_TRY_ADDREF` over what may be immutable.
    pub(crate) fn take_hold(&self, value: Value) -> Hold {
        self.addref(&value);
        Hold(value)
    }

    /// The hold given back and what is left of the value. An object is
    /// `OBJ_RELEASE`, its destructor served at the last owner and a
    /// possible root of the cycle collector where owners are left; a
    /// string, an array or a reference is `GC_DELREF` with the value
    /// destroyed at the last, and no root (design §5.12).
    // Taking the hold is the point: it is the owner given up here, and
    // one given back cannot be given back again.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn let_go(&mut self, hold: Hold) -> Left {
        let Hold(value) = hold;
        let zval = value.to_zval();
        if !zval.is_refcounted() {
            return Left::Uncounted;
        }
        let owners = value.refcount().saturating_sub(1);
        if value.as_object().is_some() {
            Heap::release(&mut Hooks(self), zval);
        } else {
            Heap::release_nogc(&mut Hooks(self), zval);
        }
        Left::Owners(owners)
    }
}
