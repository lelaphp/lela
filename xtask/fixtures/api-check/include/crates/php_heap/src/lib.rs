mod generated {
    //! What `build.rs` writes; api-check reads no file of OUT_DIR.
    include!(concat!(env!("OUT_DIR"), "/kind.rs"));
}
mod kind;
mod plain;

pub use generated::Kind;
// A glob through the including module answers for nothing it does not
// define: `Plain` is the next glob's, and its method lists under it.
pub use generated::*;
pub use plain::*;
