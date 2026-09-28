//! The process's effective user id, `geteuid(2)`, which `ps_files_open`
//! compares a session file's owner with (`mod_files.c`) and which the
//! standard library does not read (design §7.2, `libc`, thin;
//! `unsafe.homes`' `ext/*/src/ffi/`). The real one is the engine's
//! (`php_ext::process`).
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

/// `geteuid()`.
pub(super) fn euid() -> u32 {
    // SAFETY: `geteuid` takes nothing and cannot fail.
    unsafe { libc::geteuid() }
}
