//! `php_jit_arena`, set where the target has a `Pages` adapter that maps
//! code (`jit-design.md` §4.4): macOS on aarch64, one `MAP_JIT` mapping,
//! and Linux on x86-64 and aarch64, one sealed `memfd` mapped twice. This
//! is the one place the set is named; the adapters, the `Native` alias
//! and the tests that enter published code read it.
//!
//! Beside it, the cfg names only the bench's builds set, added to the
//! workspace's rustflags by a `--config` line, into target directories of
//! their own, registered so `unexpected_cfgs` holds in every other build:
//! `php_jit_stepping`, the stepping arm of the `step` bar (#2946,
//! `src/stepping.rs`), `php_jit_usage`, the G4 bar's line of what a
//! request's traces took (#2961, `JitEngine::request_ended`), and
//! `php_jit_untested_loops`, the `interrupt` bar's traces compiled without
//! the interrupt test, at a loop's header and before a link alike (#2960,
//! `src/lower/clif.rs`).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(php_jit_arena)");
    println!("cargo::rustc-check-cfg=cfg(php_jit_stepping)");
    println!("cargo::rustc-check-cfg=cfg(php_jit_usage)");
    println!("cargo::rustc-check-cfg=cfg(php_jit_untested_loops)");
    let target = |key: &str| std::env::var(key).unwrap_or_default();
    let (os, arch) = (
        target("CARGO_CFG_TARGET_OS"),
        target("CARGO_CFG_TARGET_ARCH"),
    );
    if matches!(
        (os.as_str(), arch.as_str()),
        ("macos", "aarch64") | ("linux", "x86_64" | "aarch64")
    ) {
        println!("cargo::rustc-cfg=php_jit_arena");
    }
}
