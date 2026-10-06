//! Built on `php_unit`, whose lint is its own: clean, and a test that
//! passes.

/// Three.
pub fn three() -> u8 {
    php_unit::two() + 1
}

#[cfg(test)]
mod tests {
    #[test]
    fn three_is_three() {
        assert_eq!(super::three(), 3);
    }
}
