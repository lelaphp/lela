//! `csprng.c`: the host's CSPRNG, `php_random_bytes` and `php_random_int`.
//! php reads `CCRandomGenerateBytes` on macOS and `getrandom(2)` on Linux,
//! falling back to `/dev/urandom`; this reads the `getrandom` crate, which
//! is `getentropy(2)` here and `getrandom(2)` there with the same fallback.
//! No byte of either is observable, so the choice is the smallest one: no
//! `unsafe` in this crate. A failure is the `errstr` php throws as
//! `Random\RandomException`, worded as its own.

use crate::Failure;

/// `php_random_bytes_ex`: `buf` filled from the host, or the sentence
/// php's `errstr` would carry.
pub(super) fn fill(buf: &mut [u8]) -> Result<(), Failure> {
    getrandom::fill(buf).map_err(|why| {
        Failure::Csprng(format!(
            "Failed to retrieve randomness from the operating system ({why})"
        ))
    })
}

/// One `zend_ulong` of the host's bytes, in memory order.
pub(super) fn word() -> Result<u64, Failure> {
    let mut bytes = [0; 8];
    fill(&mut bytes)?;
    Ok(u64::from_ne_bytes(bytes))
}

/// `php_random_int`: a uniform integer in `[min, max]`, drawn a word at
/// a time and redrawn over the ceiling below which the modulus is not
/// biased, php's rejection loop exactly.
///
/// ```php
/// var_dump(random_int(5, 5), random_int(PHP_INT_MIN, PHP_INT_MIN));
/// $n = random_int(-3, 3); var_dump($n >= -3 && $n <= 3);
/// $n = random_int(PHP_INT_MIN, PHP_INT_MAX); var_dump(is_int($n));
/// ```
/// ```expect
/// int(5)
/// int(-9223372036854775808)
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn int(min: i64, max: i64) -> Result<i64, Failure> {
    if min == max {
        return Ok(min);
    }
    let mut umax = max.cast_unsigned().wrapping_sub(min.cast_unsigned());
    let mut trial = word()?;
    if umax == u64::MAX {
        return Ok(trial.cast_signed());
    }
    umax += 1;
    if umax & (umax - 1) != 0 {
        let limit = u64::MAX - (u64::MAX % umax) - 1;
        while trial > limit {
            trial = word()?;
        }
    }
    Ok((trial % umax)
        .wrapping_add(min.cast_unsigned())
        .cast_signed())
}
