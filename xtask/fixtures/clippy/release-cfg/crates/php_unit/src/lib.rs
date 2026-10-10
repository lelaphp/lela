//! Names the cfg only where no build reads it.

// debug_assertions
/// The word, as a string.
#[must_use]
pub fn word() -> &'static str {
    "debug_assertions"
}
