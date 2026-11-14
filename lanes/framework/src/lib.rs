//! The framework lane's pure part (#2140; #3740): its own crate with its
//! own ceiling, so the lines ruled for it cannot be spent by other
//! harness work. Nothing here serves a project or reads the store; the
//! adapter under `xtask/src` does, and calls this crate. It depends on no
//! workspace crate. #2140 has written nothing here yet.

#![forbid(unsafe_code)]
