//! An extension whose bindings are its `ffi` module.

mod ffi;

pub fn one() -> u8 {
    ffi::one()
}
