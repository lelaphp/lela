//! `engine_secure.c`: the host's CSPRNG as an engine, with no state
//! (`state_size` zero) and no serialisation, its range `php_random_int`.

use crate::{Algo, Failure, Generated, csprng};

/// The state of `Random\Engine\Secure`: none.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Secure;

impl Algo for Secure {
    /// Eight CSPRNG bytes, a `zend_ulong` in memory order.
    ///
    /// ```php
    /// $e = new Random\Engine\Secure; var_dump(strlen($e->generate()));
    /// try { clone $e; } catch (Error $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(8)
    /// Trying to clone an uncloneable object of class Random\Engine\Secure
    /// ```
    fn generate(&mut self) -> Result<Generated, Failure> {
        Ok(Generated {
            result: csprng::word()?,
            size: 8,
        })
    }

    fn range(&mut self, min: i64, max: i64) -> Result<i64, Failure> {
        csprng::int(min, max)
    }
}
