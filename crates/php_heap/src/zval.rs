//! The zval (design §5.1): sixteen bytes, `Copy`, no `Drop`. Zend's type
//! numbering is kept so that the all-zero value is `IS_UNDEF`.

use std::any::Any;
use std::ptr::NonNull;

use crate::Str;
use crate::array::Arr;
use crate::boxed::Boxed;

/// The type byte of a zval, numbered as `Zend/zend_types.h` numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum ZvalType {
    /// `IS_UNDEF`: a slot nothing has written.
    #[default]
    Undef = 0,
    Null = 1,
    False = 2,
    True = 3,
    Long = 4,
    Double = 5,
    String = 6,
    Array = 7,
    Object = 8,
    Resource = 9,
    Reference = 10,
}

/// One value slot: a payload word, the type byte, the type flags and the
/// opline-specific `u2` word. Copying a zval is a bitwise copy and touches
/// no refcount; [`crate::Heap::addref`] and [`crate::Heap::release`] are
/// explicit.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct Zval {
    value: u64,
    kind: ZvalType,
    type_flags: u8,
    _pad: u16,
    u2: u32,
}

impl Zval {
    /// The all-zero zval, `IS_UNDEF`.
    pub const UNDEF: Zval = Zval {
        value: 0,
        kind: ZvalType::Undef,
        type_flags: 0,
        _pad: 0,
        u2: 0,
    };

    const fn scalar(kind: ZvalType, value: u64) -> Zval {
        Zval {
            value,
            kind,
            type_flags: 0,
            _pad: 0,
            u2: 0,
        }
    }

    /// `ZVAL_NULL`.
    #[must_use]
    pub const fn null() -> Zval {
        Zval::scalar(ZvalType::Null, 0)
    }

    /// `ZVAL_BOOL`: two distinct tags, as Zend keeps them.
    #[must_use]
    pub const fn bool(b: bool) -> Zval {
        Zval::scalar(if b { ZvalType::True } else { ZvalType::False }, 0)
    }

    /// `ZVAL_LONG`.
    #[must_use]
    pub const fn long(n: i64) -> Zval {
        Zval::scalar(ZvalType::Long, n.cast_unsigned())
    }

    /// `ZVAL_DOUBLE`.
    #[must_use]
    pub const fn double(d: f64) -> Zval {
        Zval::scalar(ZvalType::Double, d.to_bits())
    }

    /// `ZVAL_STR`: the string, refcounted unless it is one of the
    /// process's interned strings, which is the one test php makes here
    /// (`Z/Zend/zend_types.h:1093-1100`).
    #[must_use]
    pub fn string(s: Str) -> Zval {
        Zval {
            value: s.payload(),
            kind: ZvalType::String,
            type_flags: if s.is_interned() {
                0
            } else {
                Zval::TYPE_REFCOUNTED
            },
            _pad: 0,
            u2: 0,
        }
    }

    /// `ZVAL_ARR`: the array, refcounted unless it is immutable, which is
    /// what a compiled program's constant array is (design §5.3).
    #[must_use]
    pub fn array(a: Arr) -> Zval {
        Zval {
            value: a.payload(),
            kind: ZvalType::Array,
            type_flags: if a.is_immutable() {
                0
            } else {
                Zval::TYPE_REFCOUNTED | Zval::TYPE_COLLECTABLE
            },
            _pad: 0,
            u2: 0,
        }
    }

    /// A zval of `kind` naming a boxed value the heap counts: the
    /// stand-in for an object, a resource or a reference until each
    /// stands on the heap as its own (design §11, days 3–7). The payload
    /// is the box, and the private bit in the flags says so, so that no
    /// reader takes it for the store entry it will be.
    pub(crate) fn boxed(kind: ZvalType, b: NonNull<Boxed>) -> Zval {
        Zval {
            value: b.as_ptr().expose_provenance() as u64,
            kind,
            type_flags: Zval::TYPE_REFCOUNTED | Zval::TYPE_BOXED,
            _pad: 0,
            u2: 0,
        }
    }

    /// The type byte.
    #[must_use]
    pub const fn kind(self) -> ZvalType {
        self.kind
    }

    /// `Z_ISUNDEF`.
    #[must_use]
    pub const fn is_undef(self) -> bool {
        matches!(self.kind, ZvalType::Undef)
    }

    /// `Z_NEXT`: the `u2` word, which inside an array's bucket is the
    /// next bucket of the hash chain.
    pub(crate) const fn next(self) -> u32 {
        self.u2
    }

    /// Write `Z_NEXT`.
    pub(crate) const fn set_next(&mut self, next: u32) {
        self.u2 = next;
    }

    /// The payload of a `Long`, else `None`.
    #[must_use]
    pub const fn as_long(self) -> Option<i64> {
        match self.kind {
            ZvalType::Long => Some(self.value.cast_signed()),
            _ => None,
        }
    }

    /// The payload of a `Double`, else `None`.
    #[must_use]
    pub const fn as_double(self) -> Option<f64> {
        match self.kind {
            ZvalType::Double => Some(f64::from_bits(self.value)),
            _ => None,
        }
    }

    /// The string of a `String`, else `None`. Naming it does not take an
    /// owner, as php's `Z_STR` does not.
    #[must_use]
    pub fn as_str(self) -> Option<Str> {
        match self.kind {
            // SAFETY: the payload of a `String` was written by
            // `Zval::string` from a live string, and a zval that names a
            // freed string is the same mistake in php.
            ZvalType::String => unsafe { Str::from_payload(self.value) },
            _ => None,
        }
    }

    /// The array of an `Array`, else `None`. Naming it does not take an
    /// owner, as php's `Z_ARR` does not.
    #[must_use]
    pub fn as_arr(self) -> Option<Arr> {
        match self.kind {
            // SAFETY: the payload of an `Array` was written by
            // `Zval::array` from a live array, and a zval that names a
            // freed array is the same mistake in php.
            ZvalType::Array => unsafe { Arr::from_payload(self.value) },
            _ => None,
        }
    }

    /// The boxed value a zval of an owned kind names, as `T`, and `None`
    /// for a zval that names none or one of another type. What is
    /// borrowed lives while the zval's owner holds it.
    #[must_use]
    pub fn as_boxed<T: Any>(&self) -> Option<&T> {
        self.boxed_ptr()
            // SAFETY: the payload was written by `Zval::boxed` from a live
            // box the heap counts, held by whoever holds this zval.
            .map(|b| unsafe { b.as_ref() })
            .and_then(|b| b.value().downcast_ref::<T>())
    }

    /// Whether the payload is a boxed stand-in.
    #[must_use]
    pub const fn is_boxed(self) -> bool {
        self.type_flags & Zval::TYPE_BOXED != 0
    }

    /// The box the payload names, for the heap's counting.
    pub(crate) fn boxed_ptr(self) -> Option<NonNull<Boxed>> {
        if !self.is_boxed() {
            return None;
        }
        let at: *mut Boxed =
            std::ptr::with_exposed_provenance_mut(usize::try_from(self.value).unwrap_or(0));
        NonNull::new(at)
    }

    /// `Some(true)` for `True`, `Some(false)` for `False`, else `None`.
    #[must_use]
    pub const fn as_bool(self) -> Option<bool> {
        match self.kind {
            ZvalType::True => Some(true),
            ZvalType::False => Some(false),
            _ => None,
        }
    }

    /// Whether the value carries a refcounted header (`Z_REFCOUNTED`): a
    /// bit test, never a type-range test.
    #[must_use]
    pub const fn is_refcounted(self) -> bool {
        self.type_flags & Zval::TYPE_REFCOUNTED != 0
    }

    /// `IS_TYPE_REFCOUNTED`.
    const TYPE_REFCOUNTED: u8 = 1;
    /// `IS_TYPE_COLLECTABLE`: may be part of a cycle.
    const TYPE_COLLECTABLE: u8 = 1 << 1;
    /// This engine's own bit: the payload is a boxed stand-in.
    const TYPE_BOXED: u8 = 1 << 7;
}

/// The identity of an object in the store (design §5.4): the index php
/// reports as `spl_object_id()`, and what weak references, `WeakMap`
/// keys and the garbage collector enumerate by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zval_is_sixteen_bytes_and_undef_is_all_zero() {
        assert_eq!(std::mem::size_of::<Zval>(), 16);
        assert_eq!(Zval::UNDEF.kind(), ZvalType::Undef);
        assert_eq!(Zval::long(-3).as_long(), Some(-3));
        assert_eq!(Zval::double(0.5).as_double(), Some(0.5));
        assert_eq!(Zval::bool(true).as_bool(), Some(true));
        let mut strings = crate::Strings::new();
        let php = crate::Intern::intern(&mut strings, b"php");
        assert!(php.is_ok(), "the engine's table would not intern");
        let Ok(php) = php else {
            return;
        };
        let interned = Zval::string(php);
        assert_eq!(
            interned.as_str().map(|s| s.bytes().to_vec()),
            Some(b"php".to_vec())
        );
        assert!(
            !interned.is_refcounted(),
            "a zval naming an interned string carries no refcount"
        );
        assert_eq!(Zval::long(1).as_str(), None);
        assert!(!Zval::null().is_refcounted());
    }
}
