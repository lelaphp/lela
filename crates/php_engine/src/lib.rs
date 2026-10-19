//! The engine (design §4.1, §5): the one loop over `php_unit`, the native
//! face [`api`], the tier seam [`tier`], the linker, and `serve`. Four
//! faces, each locked: the host's (`Engine`, `serve`, `sapi.lock`), the
//! extension's (`api`, measured on `php_ext` as `api.lock`), the
//! tier's (`tier.lock`) and the stream layer's ([`layer`], `layer.lock`). The loop over `php_unit::Opcode` is [`tier`], its
//! dispatch generated from `ops.toml` in `build.rs`, its handlers in
//! `handlers` and its semantics in `ops`; the rest of the kernels and the
//! natives are Lane B and C of design §11. The interrupt the loop tests
//! at its safepoints, and the execution timer that sets it, are
//! [`interrupt`] (design §5.8). The one home for `fork`, which pcntl asks
//! through the request and a host through [`fork()`], is `fork` (#1807).
//! [`getopt`] is `main/getopt.c`, which every host reads its command line
//! with and `getopt()` reads a program's `argv` with (#889). [`layer`] is
//! the face `php_streams` alone reaches, which no extension can name
//! (#1959).
#![forbid(unsafe_code)]
// Design §5.8: the dispatch threaded by tail calls is measured on nightly.
#![cfg_attr(
    dispatch = "tailcall",
    feature(explicit_tail_calls, rust_preserve_none_cc)
)]
#![cfg_attr(dispatch = "tailcall", allow(incomplete_features))]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

pub mod api;
mod arena;
mod attributes;
pub mod c_number;
mod classes;
mod command_line;
mod constants;
mod core;
mod engine;
mod enums;
mod exec;
mod fiber;
mod fork;
mod generator;
pub mod getopt;
mod handlers;
mod inheritance_cache;
mod ini_quantity;
mod ini_syntax;
mod interfaces;
mod interrupt;
pub mod layer;
mod multibyte;
mod natives;
mod observer;
mod open_basedir;
mod operand;
mod ops;
mod output;
mod preload;
#[cfg(test)]
mod probe;
mod process_scope;
pub mod readable;
mod rfc1867;
#[cfg(debug_assertions)]
mod served;
mod signal;
mod statement;
mod supplied;
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests;
// The test face is on for an extension's dev-dependency, and for this
// crate's own tests, which read a served request the same way.
#[cfg(any(feature = "test", test))]
pub mod test;
pub mod tier;
mod trace;
mod trampoline;
mod unit_cache;
mod virtual_cwd;
mod weakrefs;

pub use api::request::ini_files::IniFile;
pub use api::request::{BuildDefs, IniFiles, PHP_VERSION};
pub use command_line::{CommandLine, EachLine};
pub use constants::{
    PHP_CONFIG_FILE_PATH, PHP_CONFIG_FILE_SCAN_DIR, PHP_LOCALSTATEDIR, PHP_PREFIX, PHP_SYSCONFDIR,
};
#[cfg(debug_assertions)]
pub use engine::LeakSensor;
pub use engine::{
    Aborted, Body, Chdir, Dispatched, Engine, EngineBuilder, FileHandles, HostPage, HostServes,
    HostValue, IniOrigin, Method, NotAlone, Observables, OutputSink, Primary, ProcessTitle,
    ProgramFile, ProgramServed, RequestDescription, ResponseHead, Routed, Uncaught, WebRequest,
};
pub use fork::{Forked, HostThread, Pid, fork, host_thread};
pub use ini_syntax::{IniEvent, IniValue};
