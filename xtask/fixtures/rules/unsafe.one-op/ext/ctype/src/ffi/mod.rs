//! The bindings deny the rule's lints at their module root.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

mod inner;

pub(crate) fn one() -> u8 {
    inner::one()
}
