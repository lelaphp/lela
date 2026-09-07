//! The negative control for `vocab.avoid`: a crate that names public items
//! by `_Avoid_` words and writes one in rustdoc prose. It is scanned by
//! `xtask vocab`, never compiled.

/// Serve one request and hand back the result of the run.
pub fn run() {}

/// What a native hands back: a `Result` whose error is a fatal. Result
/// is also the word the glossary marks as code, so it passes bare.
pub type Result<T> = core::result::Result<T, ()>;

/// The value a native hands back to the engine.
pub struct Return;

/// A linked table under the name the glossary avoids for it.
pub struct Registry {
    /// Named by a word the glossary avoids for engine state.
    pub globals: u8,
}

/// Its variants are public items too.
pub enum Kind {
    Serve,
    Handle,
}
