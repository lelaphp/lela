//! The libFuzzer smoke lane's pure part (#3607; #3740): its own crate
//! with its own ceiling, so the lines ruled for it cannot be spent by
//! other harness work. Nothing here runs a process or reads the store;
//! the adapter under `xtask/src` does, and calls this crate. It depends
//! on no workspace crate. #3607 has written nothing here yet.

#![forbid(unsafe_code)]
