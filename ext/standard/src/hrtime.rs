//! `Z/ext/standard/hrtime.c`: `hrtime`, `zend_hrtime` as seconds and
//! nanoseconds or one count of nanoseconds. The clock is the request's
//! monotonic `Instant`, anchored at its first read to the wall clock's
//! nanoseconds, so it counts from an arbitrary point as php's does and
//! never steps back.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use php_ext::decl::FunctionDecl;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpArray, Value};

use crate::argument_count_between;
use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::optional_bool;

/// `ZEND_NANO_IN_SEC`.
const NANO_IN_SEC: u64 = 1_000_000_000;

/// The request state: the instant the clock was first read at and the
/// count it stood at then.
#[derive(Default)]
pub(super) struct State {
    anchor: Option<(Instant, u64)>,
}

/// The one.
///
/// ```php
/// echo new ReflectionFunction('hrtime');
/// ```
/// ```expect
/// Function [ <internal:standard> function hrtime ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> bool $as_number = false ]
///   }
///   - Return [ array|int|float|false ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(
        stub("hrtime(bool $as_number = false): array|int|float|false"),
        hrtime as Entry,
    )]
}

/// `zend_hrtime()`: nanoseconds from the anchor.
fn nanoseconds(req: &mut Request) -> u64 {
    let state = &mut req.state::<crate::State>().hrtime;
    let (at, base) = *state.anchor.get_or_insert_with(|| {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        (Instant::now(), u64::try_from(wall).unwrap_or(u64::MAX))
    });
    let elapsed = u64::try_from(at.elapsed().as_nanos()).unwrap_or(u64::MAX);
    base.saturating_add(elapsed)
}

/// `hrtime(bool $as_number = false): array|int|float|false`: `[seconds,
/// nanoseconds]`, or the nanoseconds as one integer.
///
/// ```php
/// $a = hrtime(); $n = hrtime(true); $b = hrtime();
/// var_dump(count($a), is_int($a[0]), is_int($a[1]), $a[1] < 1000000000, is_int($n), $n >= $a[0] * 1000000000 + $a[1], $b[0] * 1000000000 + $b[1] >= $n);
/// ```
/// ```expect
/// int(2)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
fn hrtime(req: &mut Request) -> Result<Value, Thrown> {
    let t = nanoseconds(req);
    argument_count_between(req, 0, 1)?;
    let number = optional_bool(req, 1, false)?;
    let long = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    if number {
        return Ok(Value::int(long(t)));
    }
    let mut pair = PhpArray::new(req)?;
    pair.append(req, Value::int(long(t / NANO_IN_SEC)))?;
    pair.append(req, Value::int(long(t % NANO_IN_SEC)))?;
    Ok(Value::array(pair))
}
