//! The binding (design §7.2, revision 47; `unsafe.homes`'
//! `ext/*/src/ffi/`): the one home of this crate's `unsafe`, over what
//! `build.rs` compiles and links, each as a safe type:
//!
//! - [`mbfl`]: the vendored libmbfl, its encodings and languages and the
//!   decoder and encoder `mbstring.c` drives;
//! - [`onig`]: the platform's oniguruma, the regex engine of
//!   `php_mbregex.c`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

pub(crate) mod mbfl;
pub(crate) mod onig;
