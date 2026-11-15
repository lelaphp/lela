//! The libFuzzer smoke lane's pure part (ruled 2026-10-31): its own crate
//! with its own ceiling, so the lines ruled for it cannot be spent by
//! other harness work. Nothing here runs a process or reads the store;
//! the adapter under `xtask/src` does, and calls this crate. It depends
//! on no workspace crate. The lane is not built yet, and nothing is
//! written here.

#![forbid(unsafe_code)]
