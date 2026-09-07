//! The negative control for `vocab.avoid`: a crate that names a public item
//! by an `_Avoid_` word and writes one in rustdoc prose. It is scanned by
//! `xtask vocab`, never compiled.

/// Serve one request and hand back the result of the run.
pub fn run() {}

/// What a native hands back: a `Result` whose error is a fatal.
pub type Result<T> = core::result::Result<T, ()>;

/// The value a native hands back to the engine.
pub struct Return;
