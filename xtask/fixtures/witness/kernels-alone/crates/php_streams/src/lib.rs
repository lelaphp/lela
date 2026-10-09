//! A crate outside `ext/` that declares no native: its functions are
//! neither kernels nor natives, and none is a subject.

pub mod ops;

/// No fence, and none owed.
pub fn open_memory() -> u8 {
    1
}

/// No fence, and none owed.
pub(crate) fn temporary_file() -> u8 {
    2
}
