//! The zval (design §5.1): sixteen bytes, `Copy`, no `Drop`. Zend's type
//! numbering is kept so that the all-zero value is `IS_UNDEF`.

use std::ptr::NonNull;

use crate::Str;
use crate::array::Arr;
use crate::object::Obj;
use crate::reference::Ref;
use crate::resource::Res;

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
    /// `IS_INDIRECT`: a pointer to a zval that stands elsewhere, which is
    /// what a `FETCH_*_W` leaves in its `VAR` for the `ASSIGN*` after it
    /// to write through (`ZVAL_INDIRECT`, `Z_INDIRECT`). Never
    /// refcounted, never a value a program can hold.
    Indirect = 12,
}

/// A zval in place, php's `zval *` at the sites a handler keeps one from
/// one opline to the next: a frame slot ([`crate::Frame::slot_ptr`]) or a
/// declared property slot ([`Obj::slot_ref`]). `Copy`, owning nothing,
/// and what an `Indirect` zval carries. Naming a slot does not keep it:
/// the frame or the object it is in is kept by whoever holds the zval,
/// as a `zval *` php holds across `FETCH_OBJ_W` and `ASSIGN_OBJ` is kept
/// by the CV under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ZvalRef(NonNull<Zval>);

impl ZvalRef {
    pub(crate) const fn new(at: NonNull<Zval>) -> ZvalRef {
        ZvalRef(at)
    }

    /// The zval in place, copied: `ZVAL_COPY_VALUE` off a `zval *`, no
    /// owner taken.
    #[must_use]
    pub fn read(self) -> Zval {
        // SAFETY: the pointer was taken from a live frame slot or object
        // slot by `Frame::slot_ptr` or `Obj::slot_ref`, and a name kept
        // past the slot's life is the same mistake in php.
        unsafe { self.0.as_ptr().read() }
    }

    /// Write `z` in place, what stood there overwritten and no owner
    /// touched: the caller has read it and gives it up, as
    /// `zend_assign_to_variable` gives up `garbage` after the write.
    pub fn write(self, z: Zval) {
        // SAFETY: as `read`; the slot is sixteen aligned bytes the frame
        // or the object laid down.
        unsafe { self.0.as_ptr().write(z) };
    }

    fn payload(self) -> u64 {
        self.0.as_ptr().expose_provenance() as u64
    }

    fn from_payload(value: u64) -> Option<ZvalRef> {
        let at: *mut Zval =
            std::ptr::with_exposed_provenance_mut(usize::try_from(value).unwrap_or(0));
        NonNull::new(at).map(ZvalRef)
    }
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

    /// `ZVAL_OBJ`: the object, refcounted and collectable, as every
    /// object is (design §5.4).
    #[must_use]
    pub fn object(o: Obj) -> Zval {
        Zval {
            value: o.payload(),
            kind: ZvalType::Object,
            type_flags: Zval::TYPE_REFCOUNTED | Zval::TYPE_COLLECTABLE,
            _pad: 0,
            u2: 0,
        }
    }

    /// `ZVAL_RES`: the resource, refcounted and never collectable
    /// (`IS_RESOURCE_EX`, design §5.5).
    #[must_use]
    pub fn resource(r: Res) -> Zval {
        Zval {
            value: r.payload(),
            kind: ZvalType::Resource,
            type_flags: Zval::TYPE_REFCOUNTED,
            _pad: 0,
            u2: 0,
        }
    }

    /// `ZVAL_REF`: the reference, refcounted and collectable
    /// (`IS_REFERENCE_EX`, `Z/Zend/zend_types.h:843`), since what it
    /// holds may close a cycle.
    #[must_use]
    pub fn reference(r: Ref) -> Zval {
        Zval {
            value: r.payload(),
            kind: ZvalType::Reference,
            type_flags: Zval::TYPE_REFCOUNTED | Zval::TYPE_COLLECTABLE,
            _pad: 0,
            u2: 0,
        }
    }

    /// `ZVAL_INDIRECT`: the zval that says where another stands.
    #[must_use]
    pub fn indirect(at: ZvalRef) -> Zval {
        Zval::scalar(ZvalType::Indirect, at.payload())
    }

    /// `Z_INDIRECT` of an `Indirect`, else `None`.
    #[must_use]
    pub fn as_indirect(self) -> Option<ZvalRef> {
        match self.kind {
            ZvalType::Indirect => ZvalRef::from_payload(self.value),
            _ => None,
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

    /// `Z_FE_POS`: the same `u2` word read as a `foreach`'s position in
    /// the array the zval holds, which `FE_RESET_R` leaves at zero in
    /// the `VAR` it fills and `FE_FETCH_R` moves one past each element it
    /// fetched. Zero on every zval built here, so a copied value starts
    /// a loop at the beginning.
    #[must_use]
    pub const fn fe_pos(self) -> u32 {
        self.u2
    }

    /// Write `Z_FE_POS`.
    pub const fn set_fe_pos(&mut self, pos: u32) {
        self.u2 = pos;
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

    /// The object of an `Object`, else `None`. Naming it does not take
    /// an owner, as php's `Z_OBJ` does not.
    #[must_use]
    pub fn as_obj(self) -> Option<Obj> {
        match self.kind {
            // SAFETY: the payload of an `Object` was written by
            // `Zval::object` from a live object, and a zval that names a
            // freed object is the same mistake in php.
            ZvalType::Object => unsafe { Obj::from_payload(self.value) },
            _ => None,
        }
    }

    /// The resource of a `Resource`, else `None`. Naming it does not
    /// take an owner, as php's `Z_RES` does not.
    #[must_use]
    pub fn as_resource(self) -> Option<Res> {
        match self.kind {
            // SAFETY: the payload of a `Resource` was written by
            // `Zval::resource` from a live resource, and a zval that
            // names a freed resource is the same mistake in php.
            ZvalType::Resource => unsafe { Res::from_payload(self.value) },
            _ => None,
        }
    }

    /// The reference of a `Reference`, else `None`, and not followed:
    /// `Z_REF`, where `ZVAL_DEREF` is the caller's. Naming it does not
    /// take an owner.
    #[must_use]
    pub fn as_reference(self) -> Option<Ref> {
        match self.kind {
            // SAFETY: the payload of a `Reference` was written by
            // `Zval::reference` from a live reference, and a zval that
            // names a freed reference is the same mistake in php.
            ZvalType::Reference => unsafe { Ref::from_payload(self.value) },
            _ => None,
        }
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
}

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

    #[test]
    fn an_indirect_zval_names_a_slot_in_place() {
        let mut frame = crate::Frame::new(2);
        let at = frame.slot_ptr(crate::Slot(1));
        assert!(at.is_some(), "a frame of two slots has a second");
        let Some(at) = at else {
            return;
        };
        let indirect = Zval::indirect(at);
        assert_eq!(indirect.kind(), ZvalType::Indirect);
        assert!(!indirect.is_refcounted());
        assert_eq!(indirect.as_indirect(), Some(at));
        assert_eq!(Zval::long(1).as_indirect(), None);
        at.write(Zval::long(4));
        assert_eq!(frame.slot(crate::Slot(1)), Zval::long(4));
        assert_eq!(at.read(), Zval::long(4));
        assert_eq!(frame.slot_ptr(crate::Slot(2)), None);
    }
}
