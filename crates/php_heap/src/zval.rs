//! The zval (design §5.1): sixteen bytes, `Copy`, no `Drop`. Zend's type
//! numbering is kept so that the all-zero value is `IS_UNDEF`.
// The `expect` fence's `regime=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ptr::NonNull;

use crate::Str;
use crate::array::Arr;
use crate::gc::Counted;
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
    /// `IS_CONSTANT_AST`: a constant expression in the constant table of
    /// a compiled program, where the compiler left a name unevaluated,
    /// which is what `zend_compile_params` leaves for a parameter
    /// default naming a constant. The payload is the index of the
    /// expression the op array keeps beside that table, and not a
    /// pointer, since the expression is the unit's and outlives every
    /// request that reads it. Never refcounted, and never a value a
    /// program holds: the opline that copies it evaluates it first
    /// (`zval_update_constant_ex`).
    ConstExpr = 11,
    /// `IS_INDIRECT`: a pointer to a zval that stands elsewhere, which is
    /// what a `FETCH_*_W` leaves in its `VAR` for the `ASSIGN*` after it
    /// to write through (`ZVAL_INDIRECT`, `Z_INDIRECT`). Never
    /// refcounted, never a value a program can hold.
    Indirect = 12,
    /// `IS_PTR`: an index a compiled program keeps into one of its
    /// unit's own tables, where php keeps a pointer, which is what the
    /// `OP_DATA` after a `DECLARE_ATTRIBUTED_CONST` carries: the
    /// declaration of the constant, whose attributes the opline hangs on
    /// it. Never refcounted, and never a value a program holds.
    Ptr = 13,
}

/// A bit of `Z_PROP_FLAG_P` (`Z/Zend/zend_types.h:1600-1604`), the flags a
/// declared property slot carries in its `u2` word, which no copy of the
/// slot's value takes along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropBit {
    /// `IS_PROP_UNINIT`: nothing has written the slot since the object was
    /// made, so a read, a write, `isset` and `unset` pass the magic
    /// methods by; an `unset` clears it.
    Uninit,
    /// `IS_PROP_REINITABLE`: a readonly slot a clone's `__clone`, or a
    /// `clone with`, may write once more; set on each slot of the clone
    /// before the method runs and cleared after it, and by a write.
    Reinitable,
    /// The property table's row under the slot's name is a plain value,
    /// as `zend_hash_update` over that key leaves php's where the row
    /// was an `IS_INDIRECT` to the slot: the slot holds the row's value,
    /// read by every reader of the table as before, and written by
    /// `unserialize()` with no declared type to hold it to. php has no
    /// bit for it, since its table says it by the row's type, and this
    /// table holds no row for a slot; it sits past `IS_PROP_LAZY`. A
    /// write of the slot clears it and the row stands for the slot again,
    /// where php's row would keep the plain value apart from the slot:
    /// the one class that makes such a row, `ZipArchive`, refuses every
    /// write of those properties but its own.
    PlainRow,
}

impl PropBit {
    const fn bit(self) -> u32 {
        match self {
            PropBit::Uninit => 1,
            PropBit::Reinitable => 1 << 1,
            PropBit::PlainRow => 1 << 3,
        }
    }
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
        // SAFETY: the pointer was taken from a live slot by the one
        // place that lays it down: a frame slot (`Frame::slot_ptr`), an
        // object slot (`Obj::slot_ref`), an array's bucket
        // (`Arr::value_ref`, `Arr::str_value_ref`) or a reference's value
        // (`Ref::val_ref`); a name kept past the slot's life, or past a
        // resize of the table whose bucket it is, is the same mistake in
        // php.
        unsafe { self.0.as_ptr().read() }
    }

    /// Write `z` in place as `ZVAL_COPY_VALUE` writes it, what stood
    /// there overwritten and no owner touched: the caller has read it and
    /// gives it up, as `zend_assign_to_variable` gives up `garbage` after
    /// the write.
    ///
    /// The payload and the type info, and never the `u2` word. php's
    /// `ZVAL_COPY_VALUE_EX` (`Z/Zend/zend_types.h:1000-1013`) assigns
    /// `Z_COUNTED_P` and `Z_TYPE_INFO_P` and leaves `u2` where it stands,
    /// because `u2` is not the value's: it belongs to whatever holds the
    /// slot. Inside an array's bucket it is `Z_NEXT`, the next bucket of
    /// the hash chain the bucket is linked into, and a value write that
    /// carried a zero there left a chain pointing at bucket zero. A
    /// single-bucket chain then named itself and `zend_hash_find_bucket`
    /// walked it forever, which is what hung
    /// `Zend/tests/ArrayAccess/bug63217.phpt` past the recipe's limit
    /// with nothing printed (#491): `$a[123] = 1` wrote the value through
    /// this slot after `link` had set the bucket's next, and `$a['abc']`
    /// hashes to the same chain.
    ///
    /// ```php
    /// $a = [];
    /// $a[123] = 1;
    /// $a['abc'] = 3;
    /// var_dump($a);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [123]=>
    ///   int(1)
    ///   ["abc"]=>
    ///   int(3)
    /// }
    /// ```
    ///
    /// The `u2` word a slot does carry is written by
    /// [`set_fe_pos`](ZvalRef::set_fe_pos) and by nothing else.
    pub fn write(self, z: Zval) {
        let z = z.with_u2_of(self.read());
        // SAFETY: as `read`; the slot is sixteen aligned bytes the one
        // that laid it down laid.
        unsafe { self.0.as_ptr().write(z) };
    }

    /// `Z_FE_POS_P(z) = pos`: the slot's `u2` word alone, the value it
    /// holds untouched. What `FE_RESET_R` sets to zero on the VAR it
    /// fills after copying the array in, and what `FE_FETCH_R` moves one
    /// past each element it fetched, which php writes through this word
    /// and never through a value assignment.
    pub fn set_fe_pos(self, pos: u32) {
        let mut z = self.read();
        z.set_fe_pos(pos);
        // SAFETY: as `read`.
        unsafe { self.0.as_ptr().write(z) };
    }

    /// One of `Z_PROP_FLAG_P(slot)`'s bits set or cleared on a declared
    /// property slot, its `u2` word alone, the value it holds untouched.
    pub fn set_prop_bit(self, flag: PropBit, on: bool) {
        let mut z = self.read();
        let u2 = z.u2();
        z.set_u2(if on {
            u2 | flag.bit()
        } else {
            u2 & !flag.bit()
        });
        // SAFETY: as `read`.
        unsafe { self.0.as_ptr().write(z) };
    }

    fn payload(self) -> Word {
        Word::pointer(self.0)
    }

    /// The slot an `Indirect` zval's word names.
    ///
    /// # Safety
    ///
    /// The word came from [`ZvalRef::payload`].
    unsafe fn from_payload(word: Word) -> Option<ZvalRef> {
        // SAFETY: a pointer word, by the contract.
        unsafe { word.as_pointer() }.map(ZvalRef)
    }
}

/// Which of the two walks that reach a `WeakMap` entry the cycle
/// collector's grey walk has already taken its owner off by: from the
/// key object (`GC_FROM_WEAKMAP_KEY`) and from the map
/// (`GC_FROM_WEAKMAP`). An entry is owned by the map and kept alive by
/// the key, so the walk takes one owner off it, whichever of the two it
/// came by first, and the marks say which. Cleared again before the
/// collection ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WeakMarks {
    pub(crate) from_key: bool,
    pub(crate) from_map: bool,
}

/// A zval's payload word, `zend_value` on the 64-bit targets the design
/// names: the bits of a long, a double or one of a unit's indexes, or the
/// pointer to a refcounted value or to a slot in place. A pointer is kept
/// as a pointer, its provenance with it, so a value read back out of a
/// zval reaches its block through the pointer that was stored and never
/// through an integer (#2528, #2441). Which of the two a word holds is
/// the zval's type byte's to say; the word itself does not know.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) union Word {
    bits: u64,
    pointer: *mut u8,
}

impl Word {
    /// A word of `bits`, no pointer among them.
    pub(crate) const fn bits(bits: u64) -> Word {
        Word { bits }
    }

    /// A word holding `at`, provenance and all.
    pub(crate) const fn pointer<T>(at: NonNull<T>) -> Word {
        Word {
            pointer: at.as_ptr().cast(),
        }
    }

    /// The word's bits: a scalar's payload, or a pointer's address with
    /// its provenance left behind.
    pub(crate) const fn as_bits(self) -> u64 {
        // SAFETY: every bit pattern of the eight bytes is a `u64`, and a
        // pointer's bytes read as one are its address alone, as `addr`
        // reads it.
        unsafe { self.bits }
    }

    /// The pointer [`Word::pointer`] stored, none for a null word.
    ///
    /// # Safety
    ///
    /// The word was made by [`Word::pointer`] from a pointer to a `T`.
    pub(crate) unsafe fn as_pointer<T>(self) -> Option<NonNull<T>> {
        // SAFETY: the word holds a pointer, by the contract.
        NonNull::new(unsafe { self.pointer }.cast())
    }
}

/// Two words are equal when their bits are: a pointer is compared by its
/// address, as php compares two `zend_value`s.
impl PartialEq for Word {
    fn eq(&self, other: &Word) -> bool {
        self.as_bits() == other.as_bits()
    }
}

impl std::fmt::Debug for Word {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#x}", self.as_bits())
    }
}

/// One value slot as two words: the payload word, then php's
/// `u1.type_info` and `u2` in one, the type byte at the zval's byte 8,
/// the type flags at 9, the extra bits at 10 and 11 and `u2` at 12 to
/// 15 (design §5.1), so that a copy is one pair of loads and one pair
/// of stores and a fresh number one pair of stores, as php's
/// `ZVAL_COPY_VALUE` and `ZVAL_DOUBLE` are. Each part is placed by its
/// bytes in memory order, whatever the target's byte order. Copying a
/// zval is a bitwise copy and touches no refcount; [`Zval::addref`] and
/// [`crate::Heap::release`] are explicit.
#[derive(Clone, Copy, PartialEq)]
#[repr(C)]
pub struct Zval {
    value: Word,
    /// The type byte, the type flags, the extra bits of php's
    /// `Z_TYPE_INFO` above them, where the cycle collector marks a
    /// `WeakMap` entry ([`WeakMarks`]) and nothing else writes, and the
    /// `u2` word. The type byte holds one of [`ZvalType`]'s
    /// discriminants and nothing else: [`Zval::typed`] is the one place
    /// it is written.
    info: u64,
}

// SAFETY: a zval is the sixteen bytes php's is, and what its word points
// at is the claim `Str`, `Arr`, `Obj`, `Ref` and `Res` make for the value
// it names: a request's value moves with the request that owns it, whole,
// to the thread that serves it, and an immutable one, an interned string
// or a constant array of a published unit, is shared by every thread and
// touched by none (design §3.4, §5.14). The integer word this replaced
// let the compiler infer both; the pointer word states them.
unsafe impl Send for Zval {}
// SAFETY: as `Send`: a zval shared across threads is one inside a
// published unit, whose values are immutable and never refcounted.
unsafe impl Sync for Zval {}

// The layout the design names: sixteen bytes, the word first, and the
// type byte, the flags, the extra bits and `u2` at bytes 8, 9, 10 and 12.
const _: () = assert!(std::mem::size_of::<Zval>() == 16);
const _: () = assert!(std::mem::offset_of!(Zval, value) == 0);
const _: () = assert!(std::mem::offset_of!(Zval, info) == 8);
const _: () = assert!(std::mem::size_of::<Word>() == 8);
const _: () = {
    let z = Zval::typed(ZvalType::Double, Zval::TYPE_REFCOUNTED, Word::bits(0)).with_weak_marks(
        WeakMarks {
            from_key: true,
            from_map: false,
        },
    );
    let mut with_u2 = z;
    with_u2.set_u2(0x0403_0201);
    let b = with_u2.info.to_ne_bytes();
    let extra = 1u16.to_ne_bytes();
    let u2 = 0x0403_0201u32.to_ne_bytes();
    assert!(b[0] == ZvalType::Double as u8 && b[1] == Zval::TYPE_REFCOUNTED);
    assert!(b[2] == extra[0] && b[3] == extra[1]);
    assert!(b[4] == u2[0] && b[5] == u2[1] && b[6] == u2[2] && b[7] == u2[3]);
};

impl std::fmt::Debug for Zval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Zval")
            .field("value", &self.value)
            .field("kind", &self.kind())
            .field("type_flags", &self.info.to_ne_bytes()[1])
            .field("extra", &self.extra())
            .field("u2", &self.u2())
            .finish()
    }
}

impl Zval {
    /// Where php's `Z_TYPE_INFO` starts, the type byte (`Z_TYPE`), the
    /// flags byte after it and the extra bits above them, one 32-bit word
    /// on the targets compiled code runs on, all little-endian: what the
    /// compiled tier loads and stores a slot's type through, on the frame
    /// layout the interpreter's (`jit-design.md` §4.3).
    pub const TYPE_AT: usize = std::mem::offset_of!(Zval, info);
    /// Where php's `Z_TYPE_FLAGS` is, the byte that says whether the
    /// value is refcounted ([`Zval::TYPE_REFCOUNTED`]).
    pub const FLAGS_AT: usize = std::mem::offset_of!(Zval, info) + 1;

    /// The all-zero zval, `IS_UNDEF`.
    pub const UNDEF: Zval = Zval {
        value: Word::bits(0),
        info: 0,
    };

    /// The bytes of the info word that hold the type byte, the zval's
    /// byte 8.
    const INFO_TYPE: u64 = u64::from_ne_bytes([0xff, 0, 0, 0, 0, 0, 0, 0]);
    /// The bytes of the info word that hold `u2`, the zval's 12 to 15.
    const INFO_U2: u64 = u64::from_ne_bytes([0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);

    /// The zval of `kind` over `value` with the type flags `flags`, no
    /// extra bits and a `u2` of zero: the one place a type byte is
    /// written.
    const fn typed(kind: ZvalType, flags: u8, value: Word) -> Zval {
        Zval {
            value,
            info: u64::from_ne_bytes([kind as u8, flags, 0, 0, 0, 0, 0, 0]),
        }
    }

    /// The `u2` word.
    const fn u2(self) -> u32 {
        let b = self.info.to_ne_bytes();
        u32::from_ne_bytes([b[4], b[5], b[6], b[7]])
    }

    /// The `u2` word written, the value and its type untouched.
    const fn set_u2(&mut self, u2: u32) {
        let u = u2.to_ne_bytes();
        self.info =
            (self.info & !Zval::INFO_U2) | u64::from_ne_bytes([0, 0, 0, 0, u[0], u[1], u[2], u[3]]);
    }

    /// This value with `slot`'s `u2` word in place of its own, what a
    /// write into a slot that keeps its `u2` writes ([`ZvalRef::write`]).
    const fn with_u2_of(self, slot: Zval) -> Zval {
        Zval {
            value: self.value,
            info: (self.info & !Zval::INFO_U2) | (slot.info & Zval::INFO_U2),
        }
    }

    /// The extra bits, the two bytes past the type flags.
    const fn extra(self) -> u16 {
        let b = self.info.to_ne_bytes();
        u16::from_ne_bytes([b[2], b[3]])
    }

    /// The extra bits written, everything else untouched.
    const fn set_extra(&mut self, extra: u16) {
        let mut b = self.info.to_ne_bytes();
        let e = extra.to_ne_bytes();
        b[2] = e[0];
        b[3] = e[1];
        self.info = u64::from_ne_bytes(b);
    }

    /// Whether `bit` of the type flags is set: a test of one bit of the
    /// info word.
    const fn has_flag(self, bit: u8) -> bool {
        self.info & u64::from_ne_bytes([0, bit, 0, 0, 0, 0, 0, 0]) != 0
    }

    const fn scalar(kind: ZvalType, value: u64) -> Zval {
        Zval::typed(kind, 0, Word::bits(value))
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
        let flags = if s.is_interned() {
            0
        } else {
            Zval::TYPE_REFCOUNTED
        };
        Zval::typed(ZvalType::String, flags, s.payload())
    }

    /// `ZVAL_ARR`: the array, refcounted unless it is immutable, which is
    /// what a compiled program's constant array is (design §5.3).
    #[must_use]
    pub fn array(a: Arr) -> Zval {
        let flags = if a.is_immutable() {
            0
        } else {
            Zval::TYPE_REFCOUNTED | Zval::TYPE_COLLECTABLE
        };
        Zval::typed(ZvalType::Array, flags, a.payload())
    }

    /// `ZVAL_OBJ`: the object, refcounted and collectable, as every
    /// object is (design §5.4).
    #[must_use]
    pub fn object(o: Obj) -> Zval {
        Zval::typed(
            ZvalType::Object,
            Zval::TYPE_REFCOUNTED | Zval::TYPE_COLLECTABLE,
            o.payload(),
        )
    }

    /// `ZVAL_RES`: the resource, refcounted and never collectable
    /// (`IS_RESOURCE_EX`, design §5.5).
    #[must_use]
    pub fn resource(r: Res) -> Zval {
        Zval::typed(ZvalType::Resource, Zval::TYPE_REFCOUNTED, r.payload())
    }

    /// `ZVAL_REF`: the reference, refcounted and collectable
    /// (`IS_REFERENCE_EX`, `Z/Zend/zend_types.h:843`), since what it
    /// holds may close a cycle.
    #[must_use]
    pub fn reference(r: Ref) -> Zval {
        Zval::typed(
            ZvalType::Reference,
            Zval::TYPE_REFCOUNTED | Zval::TYPE_COLLECTABLE,
            r.payload(),
        )
    }

    /// `ZVAL_AST`: the constant expression at `at` of the op array's own
    /// table of them, which is what a constant of this kind names.
    #[must_use]
    pub const fn const_expr(at: u32) -> Zval {
        Zval::scalar(ZvalType::ConstExpr, at as u64)
    }

    /// `Z_ASTVAL` of an `IS_CONSTANT_AST`, else `None`.
    #[must_use]
    pub fn as_const_expr(self) -> Option<u32> {
        match self.kind() {
            ZvalType::ConstExpr => u32::try_from(self.value.as_bits()).ok(),
            _ => None,
        }
    }

    /// `ZVAL_PTR`: the entry at `at` of a table the unit keeps.
    #[must_use]
    pub const fn ptr(at: u32) -> Zval {
        Zval::scalar(ZvalType::Ptr, at as u64)
    }

    /// `Z_PTR` of an `IS_PTR`, else `None`.
    #[must_use]
    pub fn as_ptr(self) -> Option<u32> {
        match self.kind() {
            ZvalType::Ptr => u32::try_from(self.value.as_bits()).ok(),
            _ => None,
        }
    }

    /// `ZVAL_INDIRECT`: the zval that says where another stands.
    #[must_use]
    pub fn indirect(at: ZvalRef) -> Zval {
        Zval::typed(ZvalType::Indirect, 0, at.payload())
    }

    /// `Z_INDIRECT` of an `Indirect`, else `None`.
    #[must_use]
    pub fn as_indirect(self) -> Option<ZvalRef> {
        match self.kind() {
            // SAFETY: the word of an `Indirect` was written by
            // `Zval::indirect` from a `ZvalRef`.
            ZvalType::Indirect => unsafe { ZvalRef::from_payload(self.value) },
            _ => None,
        }
    }

    /// The type byte.
    #[must_use]
    pub const fn kind(self) -> ZvalType {
        let byte = self.info.to_ne_bytes()[0];
        // SAFETY: the type byte is written by `Zval::typed` alone, from a
        // `ZvalType`, or is the zero of `Zval::UNDEF`, so it holds one of
        // the enum's discriminants; every other writer of the info word
        // writes its other bytes and keeps this one as it stood.
        unsafe { std::mem::transmute::<u8, ZvalType>(byte) }
    }

    /// Whether `other` is the same value of the same type: the identity
    /// of a slot's content, the type and, for a type that carries one,
    /// the payload word, whatever the `u2` word carries (a bucket's is
    /// its hash chain); a double's bits rather than its number, so a NaN
    /// is itself.
    #[must_use]
    pub fn is_same_value(self, other: Zval) -> bool {
        self.kind() == other.kind()
            && match self.kind() {
                ZvalType::Undef | ZvalType::Null | ZvalType::False | ZvalType::True => true,
                ZvalType::Long
                | ZvalType::Double
                | ZvalType::String
                | ZvalType::Array
                | ZvalType::Object
                | ZvalType::Resource
                | ZvalType::Reference
                | ZvalType::ConstExpr
                | ZvalType::Indirect
                | ZvalType::Ptr => self.value == other.value,
            }
    }

    /// `Z_ISUNDEF`.
    #[must_use]
    pub const fn is_undef(self) -> bool {
        self.info & Zval::INFO_TYPE == 0
    }

    /// `Z_NEXT`: the `u2` word, which inside an array's bucket is the
    /// next bucket of the hash chain.
    pub(crate) const fn next(self) -> u32 {
        self.u2()
    }

    /// Write `Z_NEXT`.
    pub(crate) const fn set_next(&mut self, next: u32) {
        self.set_u2(next);
    }

    /// `Z_FE_POS`: the same `u2` word read as a `foreach`'s position in
    /// the array the zval holds, which `FE_RESET_R` leaves at zero in
    /// the `VAR` it fills and `FE_FETCH_R` moves one past each element it
    /// fetched. Zero on every zval built here, so a copied value starts
    /// a loop at the beginning.
    #[must_use]
    pub const fn fe_pos(self) -> u32 {
        self.u2()
    }

    /// Write `Z_FE_POS`.
    pub const fn set_fe_pos(&mut self, pos: u32) {
        self.set_u2(pos);
    }

    /// Whether one of `Z_PROP_FLAG_P(z)`'s bits is set on a declared
    /// property slot ([`ZvalRef::set_prop_bit`]).
    #[must_use]
    pub const fn prop_bit(self, flag: PropBit) -> bool {
        self.u2() & flag.bit() != 0
    }

    /// The payload of a `Long`, else `None`.
    #[must_use]
    pub const fn as_long(self) -> Option<i64> {
        match self.kind() {
            ZvalType::Long => Some(self.value.as_bits().cast_signed()),
            _ => None,
        }
    }

    /// The payload of a `Double`, else `None`.
    #[must_use]
    pub const fn as_double(self) -> Option<f64> {
        match self.kind() {
            ZvalType::Double => Some(f64::from_bits(self.value.as_bits())),
            _ => None,
        }
    }

    /// The string of a `String`, else `None`. Naming it does not take an
    /// owner, as php's `Z_STR` does not.
    #[must_use]
    pub fn as_str(self) -> Option<Str> {
        match self.kind() {
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
        match self.kind() {
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
        match self.kind() {
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
        match self.kind() {
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
        match self.kind() {
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
        match self.kind() {
            ZvalType::True => Some(true),
            ZvalType::False => Some(false),
            _ => None,
        }
    }

    /// `Z_COUNTED`: the refcounted header of the block a refcounted value
    /// names, at the block's start for every counted kind, so no kind is
    /// asked; none for a value that carries no count, which a scalar, an
    /// interned string and an immutable array are, by their type flags
    /// ([`Zval::string`], [`Zval::array`]).
    #[inline(always)] // a bit test and the word, on every addref and release
    #[allow(clippy::inline_always)]
    pub(crate) fn counted(self) -> Option<Counted> {
        if !self.is_refcounted() {
            return None;
        }
        // SAFETY: a refcounted zval's word is the pointer its value's
        // `payload` wrote.
        let block = unsafe { self.value.as_pointer::<u8>() }?;
        // SAFETY: a counted kind's block starts with its header, which
        // each kind's header asserts, and stands while a zval names it, as
        // a zval naming a freed block is the same mistake in php.
        Some(unsafe { Counted::new(block) })
    }

    /// `Z_TRY_ADDREF`: one more owner of what this zval holds, php's
    /// `Z_OPT_REFCOUNTED` test of the type flags and `GC_ADDREF` on the
    /// header at the block's start, and nothing for a scalar, an interned
    /// string or an immutable array, which carry no `IS_TYPE_REFCOUNTED`
    /// bit. Needs
    /// no request and no heap: an inline path that copies a value takes
    /// its owner where it holds the slots. A debug build's freed-use guard
    /// is the header's, and names the caller.
    ///
    /// ```php
    /// $i = "interned"; $l = [1, 2]; $c = str_repeat("c", mt_rand(3, 3));
    /// $x = $i; $y = $l; $z = $c;
    /// debug_zval_dump($x, $y, $z);
    /// ```
    /// ```expect regime=immutable-literals
    /// string(8) "interned" interned
    /// array(2) packed interned {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// string(3) "ccc" refcount(3)
    /// ```
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn addref(self) {
        if let Some(header) = self.counted() {
            header.addref();
        }
    }

    /// Whether the value carries a refcounted header (`Z_REFCOUNTED`): a
    /// bit test, never a type-range test.
    #[must_use]
    pub const fn is_refcounted(self) -> bool {
        self.has_flag(Zval::TYPE_REFCOUNTED)
    }

    /// `Z_COLLECTABLE`: a mutable array, an object or a reference, the
    /// values a cycle can pass through.
    #[must_use]
    pub(crate) const fn is_collectable(self) -> bool {
        self.has_flag(Zval::TYPE_COLLECTABLE)
    }

    /// The marks the cycle collector left on this zval, a `WeakMap`
    /// entry's.
    pub(crate) const fn weak_marks(self) -> WeakMarks {
        WeakMarks {
            from_key: self.extra() & Zval::FROM_WEAKMAP_KEY != 0,
            from_map: self.extra() & Zval::FROM_WEAKMAP != 0,
        }
    }

    /// The zval with `marks` in place of the ones it carried.
    pub(crate) const fn with_weak_marks(mut self, marks: WeakMarks) -> Zval {
        let mut extra = self.extra() & !(Zval::FROM_WEAKMAP_KEY | Zval::FROM_WEAKMAP);
        if marks.from_key {
            extra |= Zval::FROM_WEAKMAP_KEY;
        }
        if marks.from_map {
            extra |= Zval::FROM_WEAKMAP;
        }
        self.set_extra(extra);
        self
    }

    /// `Z_FROM_WEAKMAP_KEY` (`Z/Zend/zend_gc.c:200`), in the extra bits.
    const FROM_WEAKMAP_KEY: u16 = 1 << 0;
    /// `Z_FROM_WEAKMAP`.
    const FROM_WEAKMAP: u16 = 1 << 1;

    /// `IS_TYPE_REFCOUNTED`, a bit of the flags byte at
    /// [`Zval::FLAGS_AT`].
    pub const TYPE_REFCOUNTED: u8 = 1;
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
        let _alone = crate::serial::alone();
        let mut heap = crate::Heap::new();
        let mut stack = crate::VmStack::<()>::new();
        let pushed = stack.push_frame(&mut heap, 2, 0, ());
        assert!(pushed.is_ok(), "a frame of two slots fits the first page");
        let Some(frame) = pushed.ok().and_then(|f| stack.frame_mut(f)) else {
            return;
        };
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
        drop(stack.close(&mut heap));
    }
}
