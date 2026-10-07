//! A clippy lint, `needless_return`, and a test that passes.

/// Two, returned with a `return` clippy calls needless.
pub fn two() -> u8 {
    return 2;
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_is_two() {
        assert_eq!(super::two(), 2);
    }
}
