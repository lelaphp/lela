//! Clean in the default build, with a lint behind its feature `gated`,
//! and a test that passes.

/// Six.
pub fn six() -> u8 {
    6
}

/// Behind `gated`: a `let` clippy calls needless, which only a run that
/// turns the feature on lints (#140).
#[cfg(feature = "gated")]
pub mod gated {
    /// Seven, bound and returned.
    pub fn seven() -> u8 {
        let seven = super::six() + 1;
        seven
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn six_is_six() {
        assert_eq!(super::six(), 6);
    }
}
