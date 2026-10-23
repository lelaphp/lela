//! The cfg names only the bench's builds set, through `RUSTFLAGS` into
//! target directories of their own, registered so `unexpected_cfgs` holds
//! in every other build: `php_jit_stepping`, the stepping arm of the `step`
//! bar (#2946, `src/stepping.rs`), `php_jit_usage`, the G4 bar's line of
//! what a request's traces took (#2961, `JitEngine::request_ended`), and
//! `php_jit_untested_loops`, the `interrupt` bar's traces compiled without
//! the interrupt test, at a loop's header and before a link alike (#2960,
//! `src/lower/clif.rs`).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(php_jit_stepping)");
    println!("cargo::rustc-check-cfg=cfg(php_jit_usage)");
    println!("cargo::rustc-check-cfg=cfg(php_jit_untested_loops)");
}
