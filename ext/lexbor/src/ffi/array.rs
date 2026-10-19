//! lexbor's array of structs (`core/array_obj.h`), which php's C uses as
//! a stack of its own structs on lexbor's allocator: the bridge's work
//! list in `html5_parser.c`. So the stack is the request's memory as
//! php's is, grown as lexbor grows it, 128 entries at a time and
//! `sizeof(uint8_t *)` bytes for each byte of an entry.

use std::marker::PhantomData;
use std::ptr;

use super::{Lent, Owned, sys};

/// `lexbor_array_obj_t` of `T`s, on the request's heap: pushed under a
/// lend, since a push may grow it, and given back ([`Owned`]) with
/// `lexbor_array_obj_destroy`. A `T` is copied in and out as bytes, so
/// it is `Copy`, with no drop of its own.
pub struct ArrayObj<T: Copy> {
    array: sys::lexbor_array_obj_t,
    _entries: PhantomData<T>,
}

// SAFETY: the list is this value's alone, on the heap of the request that
// made it, and reached on that request's thread; what a `T` points at is
// the `T`'s to answer for.
unsafe impl<T: Copy + Send> Send for ArrayObj<T> {}

impl<T: Copy> ArrayObj<T> {
    /// `lexbor_array_obj_init(&array, size, sizeof(T))`: room for `size`
    /// entries; `None` where lexbor could not allocate them.
    #[must_use]
    pub fn new(_: &Lent, size: usize) -> Option<ArrayObj<T>> {
        let mut made = ArrayObj {
            array: sys::lexbor_array_obj_t {
                list: ptr::null_mut(),
                size: 0,
                length: 0,
                struct_size: 0,
            },
            _entries: PhantomData,
        };
        // SAFETY: a zeroed array this owns, laid once.
        let status =
            unsafe { sys::lexbor_array_obj_init(&raw mut made.array, size, size_of::<T>().max(1)) };
        (status == sys::LXB_STATUS_OK).then_some(made)
    }

    /// `lexbor_array_obj_push_wo_cls`, `entry` written into the slot;
    /// `false` where lexbor could not grow the list, which is left as it
    /// stood.
    #[must_use]
    pub fn push(&mut self, _: &Lent, entry: T) -> bool {
        // SAFETY: the live array this owns.
        let slot = unsafe { sys::lexbor_array_obj_push_wo_cls(&raw mut self.array) };
        if slot.is_null() {
            return false;
        }
        // SAFETY: a slot of `struct_size`, `size_of::<T>()`, bytes in the
        // list, written whatever its alignment.
        unsafe { ptr::write_unaligned(slot.cast::<T>(), entry) };
        true
    }

    /// `lexbor_array_obj_pop`: the last entry pushed, `None` where the list
    /// is empty. It allocates nothing.
    pub fn pop(&mut self) -> Option<T> {
        // SAFETY: the live array this owns.
        let slot = unsafe { sys::lexbor_array_obj_pop(&raw mut self.array) };
        if slot.is_null() {
            return None;
        }
        // SAFETY: a slot `push` wrote a `T` into, read whatever its
        // alignment; it is past the length now and not read again.
        Some(unsafe { ptr::read_unaligned(slot.cast::<T>()) })
    }
}

impl<T: Copy> Owned for ArrayObj<T> {
    /// `lexbor_array_obj_destroy(&array, false)`.
    fn give_back(mut self, _: &Lent) {
        // SAFETY: the array this owns, its list freed once, itself not.
        unsafe { sys::lexbor_array_obj_destroy(&raw mut self.array, false) };
    }
}
