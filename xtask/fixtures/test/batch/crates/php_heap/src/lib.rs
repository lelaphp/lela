//! Clean: two tests that pass and a doctest.

/// One.
///
/// ```
/// assert_eq!(php_heap::one(), 1);
/// ```
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
    fn one_is_not_two() {
        assert_ne!(super::one(), 2);
    }
}
