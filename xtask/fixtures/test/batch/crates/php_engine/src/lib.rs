//! Clean itself, and built on `php_compile`, which does not compile.

/// Five.
pub fn five() -> u8 {
    php_compile::four() + 1
}

#[cfg(test)]
mod tests {
    #[test]
    fn five_is_five() {
        assert_eq!(super::five(), 5);
    }
}
