//! Clean in the dev profile; in release the `mut` served only the debug
//! build's line.

/// One more in a debug build.
#[must_use]
pub fn count() -> u32 {
    let mut n = 1;
    #[cfg(debug_assertions)]
    {
        n += 1;
    }
    n
}
