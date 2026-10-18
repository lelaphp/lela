//! The owner of every published trace (`jit-design.md` §4.5), so far its
//! code arena alone, `code`, the one place a trace's bytes are written.
//! The table, its ids and the one mutex a publish takes land with the
//! first trace;
//! until then `code` is built for its tests (#2741).

#[allow(
    unsafe_code,
    reason = "seams.toml rule unsafe.homes: php_jit::table::code"
)]
mod code;
