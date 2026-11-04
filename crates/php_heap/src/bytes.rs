//! Bytes a declaration is named by (design §4.7, §5.14): the binary's
//! own, borrowed for the life of the process, where a native's entry
//! spells them, or a box of their own, where a compile or a reader made
//! them.
//!
//! php names a native by the `const char *` of its
//! `zend_function_entry` and a parameter by its `zend_internal_arg_info`'s,
//! static data the declaration points at, and opcache's
//! `accel_copy_permanent_strings` (`Z/ext/opcache/ZendAccelerator.c:646`)
//! re-points every name startup made at the one copy that lasts, so that
//! no name is a `pemalloc` block of its own. A [`Bytes`] borrowed from a
//! native's entry is that pointer: the name costs the process no
//! allocation. One a compile made is a box, freed with whatever holds
//! it, as a user function's name goes with its op array. One a
//! published unit names is laid in the unit's block
//! ([`crate::Persist::bytes`]), as opcache stores a cached file's names in
//! its memory (`zend_accel_store_interned_string`), and holds a share of
//! the block.

use std::ptr::NonNull;

/// The top bit of [`Bytes`]'s length: set where the bytes are the
/// binary's. No slice's length reaches it, `isize::MAX` bounding every
/// allocation and every static.
const BORROWED: usize = 1 << (usize::BITS - 1);

/// The bit below [`BORROWED`]: set where the bytes are laid in a
/// published unit's block, and then the length is the low half of the
/// rest and the bytes' offset in the block the high half. No owned
/// box's length reaches it on a 64-bit target, the only one bytes are
/// laid on.
const LAID: usize = 1 << (usize::BITS - 2);

/// Where a laid value's offset starts in its tag, past its length.
const OFFSET_SHIFT: u32 = usize::BITS / 2;

/// Immutable bytes, either borrowed from the binary for the life of the
/// process ([`Bytes::borrowed`]), owned in a box of their own (`From`
/// a box, a vector or a slice, which is copied), or laid in a published
/// unit's block ([`crate::Persist::bytes`]): the size of a `Box<[u8]>`,
/// and read, compared, hashed and printed as the slice it holds
/// whichever it is.
pub struct Bytes {
    at: NonNull<u8>,
    /// The length, with [`BORROWED`] set where the bytes are the binary's,
    /// or [`LAID`] and the offset where they are a block's.
    tagged: usize,
}

// SAFETY: the bytes are a `&'static [u8]`, a `Box<[u8]>` or a share of a
// block whose count is atomic, each `Send`, and nothing writes them once
// the value is made.
unsafe impl Send for Bytes {}
// SAFETY: as for `Send`: a shared read of bytes nobody writes.
unsafe impl Sync for Bytes {}

impl Bytes {
    /// `bytes`, the binary's, pointed at and not copied: what a native's
    /// entry spells its name with.
    #[must_use]
    pub const fn borrowed(bytes: &'static [u8]) -> Bytes {
        Bytes {
            at: NonNull::from_ref(bytes).cast(),
            tagged: bytes.len() | BORROWED,
        }
    }

    fn is_borrowed(&self) -> bool {
        self.tagged & BORROWED != 0
    }

    /// The binary's bytes this value borrows, `None` where it owns them
    /// or they are a block's: what a `const` names the same bytes again
    /// by, as no `Clone` runs in one.
    #[must_use]
    pub const fn as_borrowed(&self) -> Option<&'static [u8]> {
        if self.tagged & BORROWED == 0 {
            return None;
        }
        // SAFETY: a borrowed value's pointer and length are the
        // `&'static [u8]` `borrowed` took them from.
        Some(unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.tagged & !BORROWED) })
    }

    /// Whether the bytes are laid in a block, which only a 64-bit target
    /// lays them in.
    fn is_laid(&self) -> bool {
        cfg!(target_pointer_width = "64") && self.tagged & (BORROWED | LAID) == LAID
    }

    /// Whether `len` bytes at `offset` in a block can be named by a laid
    /// value: each fits its half of the tag.
    pub(crate) fn layable(offset: usize, len: usize) -> bool {
        cfg!(target_pointer_width = "64")
            && len >> OFFSET_SHIFT == 0
            && offset >> (OFFSET_SHIFT - 2) == 0
    }

    /// The `len` bytes at `at`, `offset` into a block whose share the
    /// caller hands over, where [`Bytes::layable`] said they fit.
    pub(crate) fn laid(at: NonNull<u8>, len: usize, offset: usize) -> Bytes {
        Bytes {
            at,
            tagged: LAID | offset << OFFSET_SHIFT | len,
        }
    }

    /// The length, whichever the bytes are.
    fn len_of(&self) -> usize {
        if self.is_laid() {
            self.tagged & ((1 << OFFSET_SHIFT) - 1)
        } else {
            self.tagged & !BORROWED
        }
    }

    /// The block laid bytes are in, `offset` bytes before them.
    fn block(&self) -> Option<NonNull<u8>> {
        if !self.is_laid() {
            return None;
        }
        let offset = (self.tagged & !LAID) >> OFFSET_SHIFT;
        NonNull::new(self.at.as_ptr().wrapping_sub(offset))
    }
}

impl std::ops::Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let len = self.len_of();
        // SAFETY: `at` holds `len` initialised bytes, the binary's, the
        // box's this value owns or the block's it holds a share of, and
        // nothing writes them while it stands.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), len) }
    }
}

impl Drop for Bytes {
    /// The box back to the allocator, where the bytes are owned, or the
    /// block's share, where they are laid; the binary's stay where they
    /// are.
    fn drop(&mut self) {
        if self.is_borrowed() {
            return;
        }
        if let Some(block) = self.block() {
            crate::persist::release(block);
            return;
        }
        let owned = NonNull::slice_from_raw_parts(self.at, self.tagged);
        // SAFETY: an owned value's pointer and length are the box `From`
        // leaked, and this is the one value that names it.
        drop(unsafe { Box::from_raw(owned.as_ptr()) });
    }
}

impl From<Box<[u8]>> for Bytes {
    fn from(owned: Box<[u8]>) -> Bytes {
        let tagged = owned.len();
        Bytes {
            at: NonNull::from(Box::leak(owned)).cast(),
            tagged,
        }
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(owned: Vec<u8>) -> Bytes {
        Bytes::from(owned.into_boxed_slice())
    }
}

impl From<&[u8]> for Bytes {
    /// A copy of `bytes`, owned.
    fn from(bytes: &[u8]) -> Bytes {
        Bytes::from(Box::<[u8]>::from(bytes))
    }
}

impl From<&str> for Bytes {
    /// A copy of `s`'s bytes, owned.
    fn from(s: &str) -> Bytes {
        Bytes::from(s.as_bytes())
    }
}

impl From<String> for Bytes {
    fn from(s: String) -> Bytes {
        Bytes::from(s.into_bytes())
    }
}

impl From<Bytes> for Box<[u8]> {
    /// The box an owned value holds, handed back whole; a copy of the
    /// binary's bytes where they are borrowed.
    fn from(bytes: Bytes) -> Box<[u8]> {
        if bytes.is_borrowed() || bytes.is_laid() {
            return Box::from(&*bytes);
        }
        let bytes = std::mem::ManuallyDrop::new(bytes);
        let owned = NonNull::slice_from_raw_parts(bytes.at, bytes.tagged);
        // SAFETY: an owned value's pointer and length are the box `From`
        // leaked, and the value is forgotten, so the box has one owner.
        unsafe { Box::from_raw(owned.as_ptr()) }
    }
}

impl Clone for Bytes {
    /// The same bytes where they are the binary's or a block's, a box of
    /// their own where they are owned.
    fn clone(&self) -> Bytes {
        if let Some(block) = self.block() {
            crate::persist::retain(block);
        }
        if self.is_borrowed() || self.is_laid() {
            Bytes {
                at: self.at,
                tagged: self.tagged,
            }
        } else {
            Bytes::from(&**self)
        }
    }
}

impl Default for Bytes {
    fn default() -> Bytes {
        Bytes::borrowed(&[])
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl std::borrow::Borrow<[u8]> for Bytes {
    fn borrow(&self) -> &[u8] {
        self
    }
}

impl std::fmt::Debug for Bytes {
    /// As the slice prints, whichever the bytes are.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <[u8] as std::fmt::Debug>::fmt(self, f)
    }
}

impl PartialEq for Bytes {
    fn eq(&self, other: &Bytes) -> bool {
        **self == **other
    }
}

impl Eq for Bytes {}

impl PartialOrd for Bytes {
    fn partial_cmp(&self, other: &Bytes) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bytes {
    fn cmp(&self, other: &Bytes) -> std::cmp::Ordering {
        (**self).cmp(&**other)
    }
}

impl std::hash::Hash for Bytes {
    /// As the slice hashes, so a borrowed and an owned value of the same
    /// bytes are one key.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

#[cfg(test)]
mod tests {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};

    use super::Bytes;

    const SPELLED: &[u8] = b"array_key_first";

    /// A borrowed value is the binary's bytes themselves, and its clone
    /// too; an owned one is a copy that lives and dies with its value;
    /// either is the slice it holds to compare, hash and print; and
    /// either is a box's size.
    #[test]
    fn borrowed_bytes_are_the_binarys_and_owned_ones_a_copy() {
        let borrowed = Bytes::borrowed(SPELLED);
        assert_eq!(borrowed.as_ptr(), SPELLED.as_ptr());
        assert_eq!(borrowed.clone().as_ptr(), SPELLED.as_ptr());
        assert_eq!(
            borrowed.as_borrowed().map(<[u8]>::as_ptr),
            Some(SPELLED.as_ptr())
        );
        let owned = Bytes::from(SPELLED);
        assert_eq!(owned.as_borrowed(), None);
        assert_ne!(owned.as_ptr(), SPELLED.as_ptr());
        let copy = owned.clone();
        assert_ne!(copy.as_ptr(), owned.as_ptr());
        drop(owned);
        assert_eq!(&*copy, SPELLED);
        let at = copy.as_ptr();
        let copy = Bytes::from(Box::<[u8]>::from(copy));
        assert_eq!(copy.as_ptr(), at, "an owned box handed back whole");
        assert_eq!(copy, borrowed);
        let hashed = BuildHasherDefault::<DefaultHasher>::default();
        assert_eq!(hashed.hash_one(&copy), hashed.hash_one(SPELLED));
        assert_eq!(hashed.hash_one(&borrowed), hashed.hash_one(SPELLED));
        assert_eq!(format!("{copy:?}"), format!("{SPELLED:?}"));
        assert_eq!(&*Bytes::from(Vec::new()), b"");
        assert_eq!(&*Bytes::default(), b"");
        assert!(Bytes::from("a") < Bytes::borrowed(b"b"));
        assert_eq!(size_of::<Bytes>(), size_of::<Box<[u8]>>());
    }
}
