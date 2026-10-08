mod generated {
    //! What `build.rs` writes; api-check reads no file of OUT_DIR.
    include!(concat!(env!("OUT_DIR"), "/kind.rs"));
}
mod kind;

pub use generated::Kind;
