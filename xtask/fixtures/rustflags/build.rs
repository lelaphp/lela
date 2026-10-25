//! php_engine's `build.rs` rule on the LLVM option, run as this crate's.

#[path = "../../../crates/php_engine/build/rustflags.rs"]
mod rustflags;

fn main() {
    rustflags::hold();
}
