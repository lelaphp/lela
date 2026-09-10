//! The facade (ADR-0004; design §4.1, §7.1): a page of re-exports and
//! nothing defined. An extension depends on this crate and on no other
//! engine crate, so the "api only" rule is crate visibility, not a lint
//! over the bodies. The seven groups of `php_engine::api`, `tier::Nop`
//! for the bundle's alias, the four attributes under `attr`, and, behind
//! the `test` feature, the `Capture` sink and `Engine::serve_capture`
//! for an extension's own tests. `api.lock` lists every public item here.
#![forbid(unsafe_code)]

pub use php_engine::api::{abi, call, coerce, decl, raising, request, values};
pub use php_engine::tier::Nop;

/// The native attributes.
pub mod attr {
    pub use php_macros::{php_class, php_const, php_function, tentative};
}

/// The test face (design §4.3 seam 4, ADR-0002): a fresh engine with one
/// extension and the `Capture` sink.
#[cfg(feature = "test")]
pub mod test {
    pub use php_engine::test::{Capture, Engine};
}
