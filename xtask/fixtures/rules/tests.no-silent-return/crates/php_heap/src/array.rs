//! The shape `crates/php_heap/src/array.rs` held before #3083: a test
//! whose setup could not finish returned, and passed having asserted
//! nothing.

/// The text of `bytes`, where it is UTF-8.
pub fn text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::text;

    #[test]
    fn a_setup_that_cannot_finish_returns() {
        let Ok(s) = std::str::from_utf8(b"abc") else {
            return;
        };
        assert_eq!(text(s.as_bytes()), Some("abc"));
    }

    #[test]
    fn a_closure_returns_and_a_result_returns_err_beside_it() -> Result<(), String> {
        let sign = |n: i32| {
            if n < 0 {
                return -1;
            }
            1
        };
        if sign(-3) > 0 {
            return Err("the closure's return is its own".to_owned());
        }
        assert_eq!(text(b"x"), Some("x"));
        Ok(())
    }
}
