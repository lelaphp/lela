//! The negative control of `xtask test`: two tests, one of which fails.

/// The one function the tests exercise.
pub fn one() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_is_one() {
        assert_eq!(super::one(), 1);
    }

    #[test]
    fn one_is_two() {
        assert_eq!(super::one(), 2, "the failing test the fixture is for");
    }
}
