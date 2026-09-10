//! The harness with one public item beyond its lock: `measure` was added
//! without its line. A `pub(crate)` item is not a public item.
pub fn run() {}

pub fn measure() {}

pub(crate) fn inside() {}
