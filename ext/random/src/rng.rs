//! The RNG `ext/standard` reads (design §4.10, §12):
//! `php_random_algo_with_state`, the engine a draw is taken from, for
//! `shuffle`, `str_shuffle` and `array_rand`. Beside it are the walks
//! those natives and `Random\Randomizer` share, php's
//! `php_array_data_shuffle` and `php_binary_string_shuffle` as one
//! Fisher-Yates over a slice and `php_array_pick_keys`. php-src keeps
//! the walks in `ext/standard` and has `randomizer.c` include its headers;
//! here the edge runs one way, `standard` on `random` (ADR-0004), so the
//! walks live with the RNG they draw from.

use php_ext::attr::mirrors;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject};

use crate::engine_user::User;
use crate::{Algo, Failure, with_default, with_engine};

/// Where a draw is taken from: the request's default `Mt19937`, which
/// `mt_srand` seeds, or an engine object, a native one by the state its
/// class carries and a user one by its `generate()`.
#[derive(Debug, Clone, Copy)]
enum Source {
    Default,
    Object(PhpObject),
}

/// The RNG: `php_random_algo_with_state`, an engine and its state,
/// named for as long as a draw needs it.
#[derive(Debug, Clone, Copy)]
pub struct Rng(Source);

impl Rng {
    /// `php_random_default_engine()`, `php_random.h`'s `static inline`
    /// over the two `PHPAPI` declarations marked below: the request's
    /// `Mt19937`, seeded from the CSPRNG on the request's first draw
    /// unless `mt_srand` seeded it.
    #[must_use]
    #[mirrors("ext/random/php_random.h", "php_random_default_algo")]
    #[mirrors("ext/random/php_random.h", "php_random_default_status")]
    pub fn default_engine() -> Rng {
        Rng(Source::Default)
    }

    /// The engine `object` is, an instance of a class implementing
    /// `Random\Engine`.
    pub(crate) fn of(object: PhpObject) -> Rng {
        Rng(Source::Object(object))
    }

    /// `f` over the engine: the default state, an engine object's own,
    /// or a user engine's `generate()` served through the request.
    pub(crate) fn with<R>(self, req: &mut Request, f: impl FnOnce(&mut dyn Algo) -> R) -> R {
        match self.0 {
            Source::Default => with_default(req, |state| f(state)),
            Source::Object(object) => match with_engine(req, object, f) {
                Ok(answer) => answer,
                Err(f) => f(&mut User::new(req, object)),
            },
        }
    }

    /// `php_array_data_shuffle` over the array's values once packed, and
    /// `php_binary_string_shuffle` over a string's bytes: from the last
    /// place down, each swapped with one drawn from `[0, place]`.
    ///
    /// ```php
    /// $r = new Random\Randomizer(new Random\Engine\Mt19937(7)); echo $r->shuffleBytes("abcdefgh"), " ", implode(",", $r->shuffleArray(["x" => 1, 2, 3, 4])), "\n";
    /// ```
    /// ```expect
    /// ceadfbgh 1,3,2,4
    /// ```
    #[mirrors("ext/standard/php_array.h", "php_array_data_shuffle")]
    #[mirrors("ext/standard/php_string.h", "php_binary_string_shuffle")]
    pub fn shuffle<T>(self, req: &mut Request, items: &mut [T]) -> Result<(), Thrown> {
        let walked = self.with(req, |algo| walk(algo, items));
        walked.map_err(|failure| failure.raise(req))
    }

    /// `php_array_pick_keys`: `num` keys of `array` in the array's order,
    /// or for `num` of one the key of one element, sampled by bucket as
    /// php samples it. An empty array and a `num` out of range are php's
    /// `ValueError`s on the caller's first and second argument.
    ///
    /// ```php
    /// $r = new Random\Randomizer(new Random\Engine\Mt19937(7)); $a = range(0, 9); unset($a[2], $a[3], $a[9]);
    /// echo implode(",", $r->pickArrayKeys(["a" => 1, "b" => 2, "c" => 3], 1)), " ", implode(",", $r->pickArrayKeys($a, 3)), " ", implode(",", $r->pickArrayKeys($a, 1)), "\n";
    /// ```
    /// ```expect
    /// a 1,6,8 8
    /// ```
    #[mirrors("ext/standard/php_array.h", "php_array_pick_keys")]
    pub fn pick_keys(
        self,
        req: &mut Request,
        array: PhpArray,
        num: i64,
    ) -> Result<Vec<Key>, Thrown> {
        let buckets = buckets(array);
        let available = i64::try_from(buckets.len()).unwrap_or(i64::MAX);
        if available == 0 {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        if num == 1 {
            let picked = self.with(req, |algo| pick_one(algo, &buckets));
            return match picked {
                Ok(key) => Ok(vec![key]),
                Err(failure) => Err(failure.raise(req)),
            };
        }
        if num <= 0 || num > available {
            return Err(arg_value_error(
                req,
                2,
                "be between 1 and the number of elements in argument #1 ($array)",
            ));
        }
        let picked = self.with(req, |algo| pick_many(algo, available, num));
        let (chosen, negative) = picked.map_err(|failure| failure.raise(req))?;
        Ok(buckets
            .into_iter()
            .zip(chosen)
            .filter(|(_, chosen)| *chosen != negative)
            .map(|((_, key), _)| key)
            .collect())
    }
}

/// The Fisher-Yates walk from the end.
fn walk<T>(algo: &mut dyn Algo, items: &mut [T]) -> Result<(), Failure> {
    let mut left = items.len();
    while left > 1 {
        left -= 1;
        let at = algo.range(0, i64::try_from(left).unwrap_or(i64::MAX))?;
        let at = usize::try_from(at).unwrap_or(0);
        if at != left {
            items.swap(left, at);
        }
    }
    Ok(())
}

/// Each live element with the bucket it stands at, which is what
/// `php_array_pick_keys` samples when it asks for one key.
fn buckets(array: PhpArray) -> Vec<(u32, Key)> {
    let mut out = Vec::with_capacity(array.len());
    let mut position = 0;
    while let Some((at, key, _)) = array.live_from(position) {
        out.push((at, key));
        position = at + 1;
    }
    out
}

/// One key: by a linear scan where fewer than half the buckets are live,
/// and otherwise by sampling buckets until a live one is drawn.
fn pick_one(algo: &mut dyn Algo, buckets: &[(u32, Key)]) -> Result<Key, Failure> {
    let live = i64::try_from(buckets.len()).unwrap_or(i64::MAX);
    let used = buckets.last().map_or(0, |(at, _)| i64::from(*at) + 1);
    if live < used - (used >> 1) {
        let at = algo.range(0, live - 1)?;
        let at = usize::try_from(at).unwrap_or(0);
        return Ok(buckets.get(at).map_or(Key::Int(0), |(_, key)| key.clone()));
    }
    loop {
        let at = algo.range(0, used - 1)?;
        if let Some((_, key)) = buckets.iter().find(|(bucket, _)| i64::from(*bucket) == at) {
            return Ok(key.clone());
        }
    }
}

/// `num` distinct positions among `available`, as the bitset php fills:
/// the complement drawn where more than half are wanted, and
/// `PHP_RANDOM_RANGE_ATTEMPTS` repeats in a row php's
/// `BrokenRandomEngineError`.
fn pick_many(algo: &mut dyn Algo, available: i64, num: i64) -> Result<(Vec<bool>, bool), Failure> {
    let (mut left, negative) = if num > (available >> 1) {
        (available - num, true)
    } else {
        (num, false)
    };
    let mut chosen = vec![false; usize::try_from(available).unwrap_or(0)];
    let mut failures = 0;
    while left > 0 {
        let at = usize::try_from(algo.range(0, available - 1)?).unwrap_or(0);
        match chosen.get_mut(at) {
            Some(slot) if !*slot => {
                *slot = true;
                left -= 1;
                failures = 0;
            }
            _ => {
                failures += 1;
                if failures > crate::RANGE_ATTEMPTS {
                    return Err(Failure::Broken);
                }
            }
        }
    }
    Ok((chosen, negative))
}
