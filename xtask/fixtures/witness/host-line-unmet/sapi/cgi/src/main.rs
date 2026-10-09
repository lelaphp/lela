//! A negative control (harness-design.md §4.1, §8): a host crate with no
//! fence, whose one line in `tests/lines.toml` the instrument must fail on.
fn main() {}
