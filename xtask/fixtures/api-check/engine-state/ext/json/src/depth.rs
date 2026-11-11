//! The default depth, a module of the crate that `build.rs` also reaches
//! by `#[path]`. Its doc, a comment and a derive name `EngineState`, and
//! no string does, so the build's closure is not failed by it.

// What the build writes is `DEPTH`, never an EngineState impl.
#[derive(EngineState)]
pub struct Depth(pub u32);

pub const DEPTH: u32 = 512;
