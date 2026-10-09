//! Denies one lint of the rule's six: the crate fails, naming the five
//! no module above its sources denies.
#![deny(clippy::unwrap_used)]

pub fn one() -> u8 {
    1
}
