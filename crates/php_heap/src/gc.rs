//! The refcounted header every PHP-visible heap value carries (design
//! §5.2–§5.5): php's `zend_refcounted_h`, eight bytes, a refcount and one
//! word that packs the type byte, the flags and the collector's colour
//! exactly as `Zend/zend_types.h:768-775` packs them.
//!
//! The header is shared: the string of §5.2 is the first value to carry
//! one, and the array, object, reference and resource carry the same one
//! when they land. The counts are `Cell`s because php mutates a refcount
//! through a shared pointer and so does every kernel above this crate.

use std::cell::Cell;

use crate::ZvalType;

/// A debug build's freed-use guard, asked before a refcount moves and
/// before a string's length or bytes are read:
/// whether the refcounted block at `at` is one the heap has freed
/// ([`crate::mm::freed`]). One that is is recorded in its heap's sensed
/// record ([`crate::sensed`]), with the operation and the line that
/// asked for it, and the operation is refused, so nothing is written
/// over the block's first word, the poison its bin's quarantine checks
/// as the block leaves.
#[cfg(debug_assertions)]
pub(crate) fn refuses_freed(
    at: std::ptr::NonNull<u8>,
    op: crate::sensed::Use,
    site: &'static std::panic::Location<'static>,
) -> bool {
    if !crate::mm::freed::is_freed(at) {
        return false;
    }
    crate::mm::freed::record(
        at,
        crate::sensed::Sensed::FreedUse {
            at: at.as_ptr().addr(),
            op,
            site,
        },
    );
    true
}

/// `zend_refcounted_h`: the first eight bytes of every refcounted value.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct Refcounted {
    /// php's count in the low thirty-one bits, and above them the pin
    /// ([`PINNED`]), which php's header has no room for elsewhere.
    refcount: Cell<u32>,
    type_info: Cell<u32>,
}

/// Where in a counted value's header its count is, php's `GC_REFCOUNT`:
/// the word with the pin above the count.
pub(crate) const REFCOUNT_AT: usize = std::mem::offset_of!(Refcounted, refcount);

/// Where in a counted value's header its type, flags and collector bits
/// are, php's `GC_TYPE_INFO`: the word [`LEAK_BITS`] is tested in.
pub(crate) const TYPE_INFO_AT: usize = std::mem::offset_of!(Refcounted, type_info);

/// The first of the walks' pins over a block other than an object, whose
/// pins are counted ([`crate::Obj::pin`]) (design §4.4, #2197, #2230,
/// #2291), the pins past the first counted beside it ([`Nested`]): the
/// top bit of the count's word, which no count reaches, since two to the thirty-one
/// owners would be thirty-two gigabytes of zvals naming one block. php's
/// header has no bit to spare beside it: each of the six bits
/// `GC_FLAGS_MASK` spans is taken by some kind (`IS_STR_VALID_UTF8`,
/// `IS_OBJ_FREE_CALLED`, `GC_PROTECTED`), and the collector's
/// twenty-two are all its own once an address is compressed. The bit is no count: [`Refcounted::refcount`]
/// and [`Counted::delref`] read the count beneath it, so the pin shows
/// in no count php's would differ from, and a release that takes the
/// count to zero asks it once there, from the word it has in hand.
const PINNED: u32 = 1 << 31;
/// The count's thirty-one bits beneath the pin.
pub(crate) const COUNT: u32 = !PINNED;

/// What one owner given up left of a block's count
/// ([`Counted::release`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Release {
    /// Owners are left.
    Owned,
    /// The last owner went: the block is destroyed.
    Last,
    /// There was no owner to give up.
    Ownerless,
}

/// `GC_FLAGS_MASK` (`Z/Zend/zend_types.h:769`); the type byte is the low
/// four bits below it and the collector's colour the bits above.
const FLAGS_MASK: u32 = 0x0000_03f0;

/// `GC_NOT_COLLECTABLE`: a string is never part of a cycle.
pub(crate) const NOT_COLLECTABLE: u32 = 1 << 4;
/// `GC_PROTECTED`: a walk is inside this value, php's recursion guard.
pub(crate) const PROTECTED: u32 = 1 << 5;
/// `GC_IMMUTABLE`, which strings spell `IS_STR_INTERNED`.
pub(crate) const IMMUTABLE: u32 = 1 << 6;
/// `GC_PERSISTENT`, which strings spell `IS_STR_PERSISTENT`: allocated
/// outside the request's heap, so the request's end does not free it.
pub(crate) const PERSISTENT: u32 = 1 << 7;
/// `IS_STR_PERMANENT`: relives the request boundary.
pub(crate) const PERMANENT: u32 = 1 << 8;
/// `IS_STR_VALID_UTF8`: the bytes were checked once and are valid UTF-8.
pub(crate) const VALID_UTF8: u32 = 1 << 9;
/// `IS_OBJ_WEAKLY_REFERENCED`, which is `GC_PERSISTENT` on an object
/// (`Z/Zend/zend_types.h:859`): a `WeakReference` or a `WeakMap` names
/// the object, so its free tells the engine before the members go.
pub(crate) const OBJ_WEAKLY_REFERENCED: u32 = PERSISTENT;
/// `IS_OBJ_DESTRUCTOR_CALLED`: `zend_objects_store_del` served the
/// destructor, and serves it no second time.
pub(crate) const OBJ_DESTRUCTOR_CALLED: u32 = 1 << 8;
/// `IS_OBJ_FREE_CALLED`: `free_obj` ran over the members, once.
pub(crate) const OBJ_FREE_CALLED: u32 = 1 << 9;
/// `IS_OBJ_LAZY_UNINITIALIZED`: a bit of the object's `extra_flags`,
/// where php keeps it (`Z/Zend/zend_types.h:867-870`), and not of this
/// word, whose top twenty-two bits are the collector's.
pub(crate) const OBJ_LAZY_UNINITIALIZED: u32 = 1 << 31;
/// `IS_OBJ_LAZY_PROXY`: the other bit of `extra_flags`, standing on a
/// proxy before its factory ran and after.
pub(crate) const OBJ_LAZY_PROXY: u32 = 1 << 30;
/// The thirty bits of `extra_flags` below php's two lazy ones, which
/// php leaves unused: how many frames name the object with no owner of
/// it ([`crate::Obj::pin`]).
pub(crate) const OBJ_PINS: u32 = (1 << 30) - 1;

/// `GC_TYPE_MASK`: the type byte's four bits at the bottom of the word.
const TYPE_MASK: u32 = 0x0000_000f;
/// `GC_INFO_SHIFT`: the collector's twenty-two bits start here.
const INFO_SHIFT: u32 = 10;
/// `GC_INFO_MASK | (GC_NOT_COLLECTABLE << GC_FLAGS_SHIFT)`: the bits of
/// `GC_TYPE_INFO` of which a value `gc_possible_root` may take has none,
/// php's `GC_MAY_LEAK` (`Z/Zend/zend_gc.h:110`), the one place the rule
/// is written ([`Refcounted::may_leak`], [`crate::Zval::LEAK_BITS`]).
pub(crate) const LEAK_BITS: u32 = (!0 << INFO_SHIFT) | NOT_COLLECTABLE;
/// `GC_ADDRESS` (`Z/Zend/zend_gc.c:90`): the low twenty bits of the
/// info, the value's index in the root buffer, compressed past 2^19.
const GC_ADDRESS: u32 = 0x000f_ffff;
/// `GC_COLOR`: the two bits above the address.
const GC_COLOR: u32 = 0x0030_0000;

/// A value's colour to the cycle collector (`Z/Zend/zend_gc.c:24-37`),
/// php's two `GC_COLOR` bits decoded; the header keeps them where php
/// keeps them and nothing reads the bits but [`Refcounted::colour`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Colour {
    /// `GC_BLACK`: in use or free, and zero, so a value the collector
    /// never touched reads black.
    Black,
    /// `GC_WHITE`: a member of a garbage cycle.
    White,
    /// `GC_GREY`: a possible member of a cycle.
    Grey,
    /// `GC_PURPLE`: a possible root of a cycle.
    Purple,
}

impl Colour {
    /// The colour's bits in the info.
    const fn bits(self) -> u32 {
        match self {
            Colour::Black => 0,
            Colour::White => 0x0010_0000,
            Colour::Grey => 0x0020_0000,
            Colour::Purple => 0x0030_0000,
        }
    }

    /// The colour the info's two bits say.
    const fn of(info: u32) -> Colour {
        match info & GC_COLOR {
            0x0010_0000 => Colour::White,
            0x0020_0000 => Colour::Grey,
            0x0030_0000 => Colour::Purple,
            _ => Colour::Black,
        }
    }
}

impl Refcounted {
    /// A header with one owner, of `kind`, with `flags` set.
    pub(crate) fn new(kind: ZvalType, flags: u32) -> Refcounted {
        Refcounted {
            refcount: Cell::new(1),
            type_info: Cell::new(kind as u32 | (flags & FLAGS_MASK)),
        }
    }

    /// `GC_REFCOUNT`: the count, beneath a pin.
    pub(crate) fn refcount(&self) -> u32 {
        self.refcount.get() & COUNT
    }

    /// `GC_SET_REFCOUNT`: the count as `zend_objects_store_del` holds it
    /// at one across a destructor, a pin kept.
    pub(crate) fn set_refcount(&self, count: u32) {
        let word = self.refcount.get();
        self.refcount.set((word & PINNED) | (count & COUNT));
    }

    /// Whether a walk pins the block ([`PINNED`]): what the end of its
    /// destruction asks before it gives the bytes back
    /// ([`crate::Heap::give_back`]).
    pub(crate) fn pinned(&self) -> bool {
        self.refcount.get() & PINNED != 0
    }

    /// `GC_FLAGS`.
    pub(crate) const fn flags(&self) -> u32 {
        self.type_info.get() & FLAGS_MASK
    }

    /// `GC_ADD_FLAGS`.
    pub(crate) fn add_flags(&self, flags: u32) {
        self.type_info
            .set(self.type_info.get() | (flags & FLAGS_MASK));
    }

    /// `GC_DEL_FLAGS`.
    pub(crate) fn del_flags(&self, flags: u32) {
        self.type_info
            .set(self.type_info.get() & !(flags & FLAGS_MASK));
    }

    /// `GC_TYPE`: the type byte, which the collector's free phase turns
    /// to `IS_NULL` so that a release reaching zero on a value it is
    /// already destroying does nothing (`GC_NULL`).
    pub(crate) fn kind(&self) -> u32 {
        self.type_info.get() & TYPE_MASK
    }

    /// `GC_TYPE_INFO(p) = kind | (GC_TYPE_INFO(p) & ~GC_TYPE_MASK)`: the
    /// type byte `kind`, the flags and the info kept. `IS_NULL` is
    /// php's `GC_NULL`, a value being destroyed, so that a release
    /// reaching zero on it does nothing; a destroyed block's own type
    /// again is a block a pin keeps standing ([`crate::Heap::give_back`]).
    pub(crate) fn set_kind(&self, kind: ZvalType) {
        self.type_info
            .set((self.type_info.get() & !TYPE_MASK) | kind as u32);
    }

    /// `GC_INFO`: the collector's twenty-two bits, colour and address.
    fn info(&self) -> u32 {
        self.type_info.get() >> INFO_SHIFT
    }

    /// `GC_REF_SET_INFO(ref, address | colour)`: where the value stands
    /// in the root buffer, its index compressed as `gc_compress` keeps
    /// it (none for a value not there), and its colour; the type and the
    /// flags kept.
    pub(crate) fn set_place(&self, address: Option<u32>, colour: Colour) {
        let info = address.map_or(0, |at| at & GC_ADDRESS) | colour.bits();
        self.type_info
            .set((self.type_info.get() & (TYPE_MASK | FLAGS_MASK)) | (info << INFO_SHIFT));
    }

    /// `GC_REF_ADDRESS`: the value's compressed index in the root
    /// buffer, none for one that is not there.
    pub(crate) fn address(&self) -> Option<u32> {
        match self.info() & GC_ADDRESS {
            0 => None,
            at => Some(at),
        }
    }

    /// `GC_REF_COLOR`.
    pub(crate) fn colour(&self) -> Colour {
        Colour::of(self.info())
    }

    /// `GC_REF_SET_COLOR`, which `GC_REF_SET_BLACK` and
    /// `GC_REF_SET_PURPLE` are for the two colours at either end.
    pub(crate) fn set_colour(&self, colour: Colour) {
        self.type_info.set(
            (self.type_info.get() & !(GC_COLOR << INFO_SHIFT)) | (colour.bits() << INFO_SHIFT),
        );
    }

    /// `GC_INFO(ref) == 0`: black and in no entry of the buffer, which
    /// is a value the collector holds nothing of.
    pub(crate) fn is_unplaced(&self) -> bool {
        self.info() == 0
    }

    /// `GC_MAY_LEAK` (`Z/Zend/zend_gc.h:110`): not in the root buffer,
    /// no colour, and collectable, which is what makes a value one
    /// `gc_possible_root` may take.
    pub(crate) fn may_leak(&self) -> bool {
        self.type_info.get() & LEAK_BITS == 0
    }
}

/// php's `zend_refcounted *`: the header of a refcounted block, named
/// by the block's own pointer. Every refcount move goes through it, so
/// the debug build's freed-use guard ([`refuses_freed`]) reads the
/// block's mark, past the header's eight bytes, with the provenance of
/// the whole block, which a `&Refcounted` does not carry (#2414). It
/// reads as the header it points at for everything else.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Counted(std::ptr::NonNull<Refcounted>);

impl Counted {
    /// The header of the block at `block`.
    ///
    /// # Safety
    ///
    /// `block` is where a refcounted block of this heap or of the
    /// process's allocator starts, its first eight bytes its header, and
    /// it stands while the `Counted` is used: the contract php's
    /// `zend_refcounted *` carries.
    pub(crate) const unsafe fn new(block: std::ptr::NonNull<u8>) -> Counted {
        Counted(block.cast())
    }

    /// Where the block starts, for the heap that frees it.
    pub(crate) const fn block(self) -> std::ptr::NonNull<u8> {
        self.0.cast()
    }

    /// `GC_ADDREF`: one more owner. This and [`Counted::delref`] are
    /// the chokepoint every refcount change reaches, whichever type's or
    /// the heap's own addref asked for it, so a debug build's freed-use
    /// guard ([`refuses_freed`]) sits here: an addref of a block the heap has
    /// freed is recorded and does nothing.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn addref(self) {
        #[cfg(debug_assertions)]
        if self.refuses(crate::sensed::Use::Addref) {
            return;
        }
        // The count saturates beneath the pin's bit ([`PINNED`]).
        let word = self.refcount.get();
        self.refcount.set(word + u32::from(word & COUNT != COUNT));
    }

    /// `GC_DELREF`: one owner fewer, and the count that is left. A count
    /// already at zero stays there, so a double release cannot wrap. A
    /// debug build refuses a decrement of a block the heap has freed as
    /// it refuses the increment, and answers one owner left, so no
    /// caller frees the block a second time.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn delref(self) -> u32 {
        #[cfg(debug_assertions)]
        if self.refuses(crate::sensed::Use::Release) {
            return 1;
        }
        let word = self.refcount.get();
        let count = (word & COUNT).saturating_sub(1);
        self.refcount.set((word & PINNED) | count);
        count
    }

    /// One owner given up as a release gives it up, `GC_DELREF`: what it
    /// left of the count. A count already at zero has no owner to give
    /// up: the block's destruction is under way, or done under a pin, and
    /// a place that still names it, as a release in place leaves one
    /// (#2245), is released again; it is left as it stands, so nothing
    /// destroys it twice, where php's count wraps and does the same.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn release(self) -> Release {
        if self.refcount() == 0 {
            Release::Ownerless
        } else if self.delref() == 0 {
            Release::Last
        } else {
            Release::Owned
        }
    }

    /// The pin set; whether it was set already, a pin the heap then
    /// counts beside it ([`Nested`]). A debug build's freed-use guard
    /// refuses a pin of a block the heap has freed, as it refuses an
    /// addref, sets nothing and answers not pinned, so nothing is
    /// counted beside it either.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn pin(self) -> bool {
        #[cfg(debug_assertions)]
        if self.refuses(crate::sensed::Use::Addref) {
            return false;
        }
        let word = self.refcount.get();
        self.refcount.set(word | PINNED);
        word & PINNED != 0
    }

    /// The last pin let go, the bit cleared; the count beneath it, which
    /// is zero where the block's last owner went while it stood. A debug
    /// build's freed-use guard refuses it over a block the heap has
    /// freed, as it refuses a release, and answers one owner left, so
    /// nothing is freed twice.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn unpin(self) -> u32 {
        #[cfg(debug_assertions)]
        if self.refuses(crate::sensed::Use::Release) {
            return 1;
        }
        let count = self.refcount.get() & COUNT;
        self.refcount.set(count);
        count
    }

    /// The freed-use guard over this header's block, for the line that
    /// asked: handed the block's own pointer, whose provenance covers
    /// the mark the guard reads past the header's eight bytes, as php's
    /// guard reads it through a `zend_refcounted *` (#2414).
    #[cfg(debug_assertions)]
    #[track_caller]
    fn refuses(self, op: crate::sensed::Use) -> bool {
        refuses_freed(self.block(), op, std::panic::Location::caller())
    }
}

/// The walks' pins over blocks other than objects past the first, which
/// the header's bit stands for ([`PINNED`]): for each block pinned more
/// than once, its address and how many pins past the first stand over
/// it (#2291). The pins over one block are let go in any order, which a
/// call stack's walks do not need and a fiber's do: a walk's step in one
/// execution and a walk's step in a fiber suspended inside it pin one
/// row, and whichever is let go first, the other keeps the row's bytes.
/// So the bit is cleared, and a block destroyed under it freed, by the
/// last pin let go, never by the first. The header has no room for the
/// count, where an object's `extra_flags` has ([`crate::Obj::pin`]); a
/// block is pinned more than once only where walks nest over one row,
/// so the list is short, and empty for every pin and let-go that does
/// not nest, which then reads its length alone. An address is a key,
/// never read through: a block stands while a pin does.
#[derive(Debug, Default)]
pub(crate) struct Nested(Vec<(usize, std::num::NonZeroU32)>);

impl Nested {
    /// One more pin over the block at `block`, pinned already.
    pub(crate) fn add(&mut self, block: std::ptr::NonNull<u8>) {
        let at = block.as_ptr().addr();
        match self.0.iter_mut().find(|(pinned, _)| *pinned == at) {
            Some((_, pins)) => *pins = pins.saturating_add(1),
            None => self.0.push((at, std::num::NonZeroU32::MIN)),
        }
    }

    /// One pin past the first over the block at `block` let go; whether
    /// there was one, where the bit's own pin is the last left.
    pub(crate) fn let_go(&mut self, block: std::ptr::NonNull<u8>) -> bool {
        if self.0.is_empty() {
            return false;
        }
        let at = block.as_ptr().addr();
        let found = self
            .0
            .iter_mut()
            .enumerate()
            .find(|(_, (pinned, _))| *pinned == at);
        let Some((index, (_, pins))) = found else {
            return false;
        };
        match std::num::NonZeroU32::new(pins.get() - 1) {
            Some(left) => *pins = left,
            None => {
                self.0.swap_remove(index);
            }
        }
        true
    }
}

impl std::ops::Deref for Counted {
    type Target = Refcounted;

    fn deref(&self) -> &Refcounted {
        // SAFETY: `Counted::new`'s contract: the block stands, and its
        // first eight bytes are the header.
        unsafe { self.0.as_ref() }
    }
}
