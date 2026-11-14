//! What an entry of `state.engine`'s `allow` does not admit (#4647), in a
//! crate no seam names, each module named by an entry of the test's: a
//! child module's file and a file under the module's directory that a
//! `#[path]` makes another module, beside the one hand impl of
//! `one/mod.rs`, which passes; an impl a macro writes; an impl in an
//! inline `mod`; two hand impls in one file; and a file a `#[path]` or an
//! `include!` reaches beside its own `mod`.

mod included;
mod macroed;
mod nested;
mod one;
mod pathed_to;
mod twice;

#[path = "one/pathed.rs"]
mod pathed;

#[path = "pathed_to.rs"]
mod again;

include!("included.rs");
