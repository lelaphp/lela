//! The bindings to the compression libraries libzip links (design §7.2,
//! `unsafe.homes`' `ext/*/src/ffi/`), each the platform's library, and
//! the one home of this crate's `unsafe`: zlib's deflate ([`zlib`]),
//! liblzma for LZMA and XZ ([`lzma`]) and libzstd for ZSTD ([`zstd`]),
//! both ways, so an entry's bytes, and the points a close reports while
//! it compresses one, are those libzip makes through the same libraries.
//! Everything else the crate reads and writes stays pure
//! Rust.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

mod lzma;
mod zlib;
mod zstd;

pub(crate) use lzma::{Code as LzmaCode, Lzma};
pub(crate) use zlib::Deflate;
pub(crate) use zstd::{
    Refused as ZstdRefused, ZstdCompress, ZstdDecompress, ZstdError, compress_bound, levels,
};
