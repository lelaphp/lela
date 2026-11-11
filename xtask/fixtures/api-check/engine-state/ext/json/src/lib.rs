//! What the sensor passes: a C library's immutable object made engine
//! state by hand in the extension's `src/ffi/`, a home `unsafe.homes`
//! names; the derive; a macro whose impl names its trait, and one whose
//! impl is inherent on a metavariable; a call that hands the trait to a
//! macro the crate defines; and a file included from `OUT_DIR`, written by
//! a `build.rs` whose closure, `src/depth.rs` among it, holds no string
//! naming the trait, though that module's doc, comment and derive do.

mod depth;
mod ffi;

use php_ext::abi::EngineState;

#[derive(EngineState)]
pub(crate) struct Settings {
    depth: u32,
}

macro_rules! shown {
    ($t:ty) => {
        impl core::fmt::Display for $t {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("json")
            }
        }
        impl $t {
            fn name() -> &'static str {
                "json"
            }
        }
    };
}

shown!(Settings);

macro_rules! bounded {
    ($t:path) => {
        const _: fn() = || {
            fn held<T: $t>() {}
            held::<Settings>();
        };
    };
}

bounded!(EngineState);

include!(concat!(env!("OUT_DIR"), "/depth.rs"));
