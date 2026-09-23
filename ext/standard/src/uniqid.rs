//! `Z/ext/standard/uniqid.c`: `uniqid`, the time in eight hex digits of
//! seconds and five of microseconds, waited on until the microsecond
//! moves past the last call's, as php's `prev_tv` makes it wait.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use php_ext::decl::FunctionDecl;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

use crate::argument_count_between;
use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::{optional_bool, optional_string, string_value};

/// The request state: `prev_tv`, the last time a call answered with.
#[derive(Default)]
pub(super) struct State {
    prev: Option<(u64, u32)>,
}

/// The one.
///
/// ```php
/// echo new ReflectionFunction('uniqid');
/// ```
/// ```expect
/// Function [ <internal:standard> function uniqid ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <optional> string $prefix = "" ]
///     Parameter #1 [ <optional> bool $more_entropy = false ]
///   }
///   - Return [ string ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(
        stub("uniqid(string $prefix = \"\", bool $more_entropy = false): string"),
        uniqid as Entry,
    )]
}

/// `gettimeofday()`: the seconds and microseconds since the epoch.
pub(super) fn now() -> (u64, u32) {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (elapsed.as_secs(), elapsed.subsec_micros())
}

/// `php_random_bytes_silent(&bytes, sizeof(uint32_t))`: four random
/// bytes, drawn through the standard library's per-process random
/// hash keys since this crate has no CSPRNG of its own yet.
fn random_u32() -> u32 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    u32::try_from(hasher.finish() & u64::from(u32::MAX)).unwrap_or(0)
}

/// `uniqid(string $prefix = "", bool $more_entropy = false): string`:
/// the prefix, `%08x%05x` of the seconds and the microseconds, and with
/// `$more_entropy` a random `%.8F` below ten after them.
///
/// ```php
/// $a = uniqid(); $b = uniqid(); $c = uniqid("pre_"); $d = uniqid("", true);
/// var_dump(strlen($a), $a !== $b, strlen($c), substr($c, 0, 4), strlen($d), $d[14], ctype_xdigit($a), $b > $a);
/// ```
/// ```expect
/// int(13)
/// bool(true)
/// int(17)
/// string(4) "pre_"
/// int(23)
/// string(1) "."
/// bool(true)
/// bool(true)
/// ```
fn uniqid(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 2)?;
    let prefix = optional_string(req, 1)?;
    let more_entropy = optional_bool(req, 2, false)?;
    let previous = req.state::<crate::State>().uniqid.prev;
    let mut tv = now();
    while Some(tv) == previous {
        tv = now();
    }
    req.state::<crate::State>().uniqid.prev = Some(tv);
    // `sec = (int) tv.tv_sec`, `usec = (int) (tv.tv_usec % 0x100000)`.
    let sec = u32::try_from(tv.0 & u64::from(u32::MAX)).unwrap_or(0);
    let usec = tv.1 % 0x0010_0000;
    let mut out = prefix.map(|p| p.as_bytes().to_vec()).unwrap_or_default();
    out.extend_from_slice(format!("{sec:08x}{usec:05x}").as_bytes());
    if more_entropy {
        let seed = f64::from(random_u32()) / f64::from(u32::MAX) * 10.0;
        out.extend_from_slice(format!("{seed:.8}").as_bytes());
    }
    string_value(req, &out)
}
