//! The facade (ADR-0004; design §4.1, §7.1): a page of re-exports and
//! nothing defined. An extension depends on this crate and on no other
//! engine crate, so the "api only" rule is crate visibility, not a lint
//! over the bodies. The ten groups of `php_engine::api`, the request
//! group with the streams facet `php_streams` adds to it, `tier::Nop`
//! for the bundle's alias, the three attributes under `attr` and
//! `__private` for what they expand to, and, behind the `test` feature,
//! the `Capture` sink and `Engine::serve_capture` for an extension's own
//! tests. `api.lock` lists every public item here.
#![forbid(unsafe_code)]

pub use php_engine::api::{abi, call, coerce, decl, net, process, raising, syntax, values};

/// The request group: the engine's, and the streams facet `php_streams`
/// adds to it (design §4.3 seam 5), one face counted in `api.lock`.
pub mod request {
    pub use php_engine::api::request::*;
    pub use php_streams::api::request::*;
}

/// What only a macro expansion names (design §4.4): listed in `api.lock`
/// under a cap of its own, and named by no extension's source.
#[doc(hidden)]
pub use php_engine::api::__private;
pub use php_engine::tier::Nop;

/// The native attributes.
pub mod attr {
    pub use php_macros::{php_class, php_const, php_function};
}

/// The test face (design §4.3 seam 4, ADR-0002): a fresh engine with one
/// extension and the `Capture` sink.
#[cfg(feature = "test")]
pub mod test {
    /// A native a traced request entered, for the witness runner
    /// (harness §4.5 condition 2, revision 58).
    #[cfg(feature = "witness-trace")]
    pub use php_engine::test::Entered;
    pub use php_engine::test::{Capture, Engine};
}
