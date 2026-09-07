//! An extension with bindings: `deny` at the root, the allowance on `ffi` alone.
#![deny(unsafe_code)]
#[allow(unsafe_code)]
pub mod ffi;
