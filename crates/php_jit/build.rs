//! The cfg names only the bench's builds set, through `RUSTFLAGS` into
//! target directories of their own, registered so `unexpected_cfgs` holds
//! in every other build: `php_jit_stepping`, the stepping arm of the `step`
//! bar (#2946, `src/stepping.rs`).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(php_jit_stepping)");
}
