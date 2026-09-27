//! The process's user ids, `getuid(2)` and `geteuid(2)`, which
//! `ps_files_open` compares a session file's owner with (`mod_files.c`)
//! and which the standard library does not read (design §7.2, `libc`,
//! thin; `unsafe.homes`' `ext/*/src/ffi/`).
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

/// The process's real and effective user ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Uids {
    pub(super) real: u32,
    pub(super) effective: u32,
}

/// `getuid()` and `geteuid()`.
pub(super) fn uids() -> Uids {
    // SAFETY: `getuid` takes nothing and cannot fail.
    let real = unsafe { libc::getuid() };
    // SAFETY: `geteuid` takes nothing and cannot fail.
    let effective = unsafe { libc::geteuid() };
    Uids { real, effective }
}
