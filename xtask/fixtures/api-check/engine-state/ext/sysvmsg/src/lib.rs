//! An `include!` the sensor cannot follow: its file is named by an
//! expression no source fixes. The sensor fails the crate naming the line.

use php_ext::abi::EngineState;

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/kept.rs"));
