//! A crate whose tests return only inside a closure or a nested fn, or
//! with `Err(..)`: the two controls beside php_heap's failure.

/// The sign of `n`.
pub fn sign(n: i32) -> i32 {
    n.signum()
}

#[cfg(test)]
mod tests {
    use super::sign;

    #[test]
    fn a_closure_and_a_nested_fn_return_on_their_own() {
        let negative = |n: i32| {
            if n < 0 {
                return true;
            }
            false
        };
        fn positive(n: i32) -> bool {
            if n > 0 {
                return true;
            }
            false
        }
        assert!(negative(sign(-2)) && positive(sign(2)));
    }

    #[test]
    fn a_result_test_returns_err() -> Result<(), String> {
        if sign(0) != 0 {
            return Err("zero's sign".to_owned());
        }
        Ok(())
    }
}
