//! `Z/ext/standard/versioning.c`: `version_compare` over
//! `php_canonicalize_version` and `php_version_compare`, the special
//! forms `dev < alpha = a < beta = b < RC = rc < # < pl = p` between the
//! numbers.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

use php_ext::abi::Union;
use php_ext::attr::php_function;
use php_ext::coerce::c_str;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::PhpString;

/// `php_canonicalize_version(version)`: `-`, `_` and `+` as `.`, a `.`
/// between a digit and a letter either way, any other non-alphanumeric
/// as `.`, runs of `.` as one, and a trailing `.` dropped.
fn canonicalize(version: &[u8]) -> Vec<u8> {
    let Some((&first, rest)) = version.split_first() else {
        return Vec::new();
    };
    let digit = |c: u8| c.is_ascii_digit() && c != b'.';
    let ndigit = |c: u8| !c.is_ascii_digit() && c != b'.';
    let mut out = vec![first];
    let mut lp = first;
    for &p in rest {
        let lq = out.last().copied().unwrap_or(0);
        if matches!(p, b'-' | b'_' | b'+') {
            if lq != b'.' {
                out.push(b'.');
            }
        } else if (ndigit(lp) && digit(p)) || (digit(lp) && ndigit(p)) {
            if lq != b'.' {
                out.push(b'.');
            }
            out.push(p);
        } else if !p.is_ascii_alphanumeric() {
            if lq != b'.' {
                out.push(b'.');
            }
        } else {
            out.push(p);
        }
        lp = p;
    }
    if out.last() == Some(&b'.') {
        out.pop();
    }
    out
}

/// `compare_special_version_forms(form1, form2)`: each form's order by
/// the first name it starts with, `-1` for one that starts with none.
fn special(form1: &[u8], form2: &[u8]) -> i64 {
    const FORMS: [(&[u8], i64); 10] = [
        (b"dev", 0),
        (b"alpha", 1),
        (b"a", 1),
        (b"beta", 2),
        (b"b", 2),
        (b"RC", 3),
        (b"rc", 3),
        (b"#", 4),
        (b"pl", 5),
        (b"p", 5),
    ];
    let order = |form: &[u8]| {
        FORMS
            .iter()
            .find(|(name, _)| form.starts_with(name))
            .map_or(-1, |(_, order)| *order)
    };
    (order(form1) - order(form2)).signum()
}

/// `strtol(p, NULL, 10)` over a component's leading digits.
fn leading_long(p: &[u8]) -> i64 {
    let mut value: i64 = 0;
    for &c in p.iter().take_while(|c| c.is_ascii_digit()) {
        value = value.saturating_mul(10).saturating_add(i64::from(c - b'0'));
    }
    value
}

/// `php_version_compare(orig_ver1, orig_ver2)`: -1, 0 or 1, walked as
/// php walks its buffers, each `.` found written over with a NUL.
fn version_cmp(v1: &[u8], v2: &[u8]) -> i64 {
    if v1.is_empty() || v2.is_empty() {
        return match (v1.is_empty(), v2.is_empty()) {
            (true, true) => 0,
            (false, _) => 1,
            _ => -1,
        };
    }
    let mut ver1 = if v1[0] == b'#' {
        v1.to_vec()
    } else {
        canonicalize(v1)
    };
    let mut ver2 = if v2[0] == b'#' {
        v2.to_vec()
    } else {
        canonicalize(v2)
    };
    let at = |v: &[u8], i: usize| v.get(i).copied().unwrap_or(0);
    // The component at `p`: to the next NUL.
    let component = |v: &[u8], p: usize| -> Vec<u8> {
        php_ext::coerce::c_str(v.get(p..).unwrap_or(&[])).to_vec()
    };
    let (mut p1, mut p2) = (0usize, 0usize);
    let (mut n1, mut n2): (Option<usize>, Option<usize>) = (Some(0), Some(0));
    let mut compare = 0;
    while at(&ver1, p1) != 0 && at(&ver2, p2) != 0 && n1.is_some() && n2.is_some() {
        n1 = ver1[p1..].iter().position(|c| *c == b'.').map(|d| d + p1);
        if let Some(d) = n1 {
            ver1[d] = 0;
        }
        n2 = ver2[p2..].iter().position(|c| *c == b'.').map(|d| d + p2);
        if let Some(d) = n2 {
            ver2[d] = 0;
        }
        let (c1, c2) = (component(&ver1, p1), component(&ver2, p2));
        let d1 = c1.first().is_some_and(u8::is_ascii_digit);
        let d2 = c2.first().is_some_and(u8::is_ascii_digit);
        compare = match (d1, d2) {
            (true, true) => match leading_long(&c1).cmp(&leading_long(&c2)) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            },
            (false, false) => special(&c1, &c2),
            (true, false) => special(b"#N#", &c2),
            (false, true) => special(&c1, b"#N#"),
        };
        if compare != 0 {
            break;
        }
        if let Some(d) = n1 {
            p1 = d + 1;
        }
        if let Some(d) = n2 {
            p2 = d + 1;
        }
    }
    if compare == 0 {
        if n1.is_some() {
            let rest = component(&ver1, p1);
            compare = if at(&ver1, p1).is_ascii_digit() {
                1
            } else {
                version_cmp(&rest, b"#N#")
            };
        } else if n2.is_some() {
            let rest = component(&ver2, p2);
            compare = if at(&ver2, p2).is_ascii_digit() {
                -1
            } else {
                version_cmp(b"#N#", &rest)
            };
        }
    }
    compare
}

/// `version_compare(string $version1, string $version2, ?string
/// $operator = null): int|bool`: -1, 0 or 1, or whether the operator
/// holds, `<`, `lt`, `<=`, `le`, `>`, `gt`, `>=`, `ge`, `==`, `=`,
/// `eq`, `!=`, `<>` and `ne`.
///
/// ```php
/// var_dump(version_compare("5.2", "5.10"), version_compare("1.0.0", "1.0"), version_compare("1.0rc1", "1.0"), version_compare("1.0-dev", "1.0alpha"), version_compare("1.0pl1", "1.0"), version_compare("", ""), version_compare("1", ""), version_compare("1.0", "1.0.0", "<"), version_compare("8.5.9", "8.5.10", "lt"), version_compare("1.2", "1.2", "eq"), version_compare("1.2", "1.3", "ne"), version_compare("1..2", "1.2"), version_compare("#1", "1"), version_compare("1.0a", "1.0b", ">="));
/// try { version_compare("1", "2", "~"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(-1)
/// int(1)
/// int(-1)
/// int(-1)
/// int(1)
/// int(0)
/// int(1)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// int(0)
/// int(0)
/// bool(false)
/// version_compare(): Argument #3 ($operator) must be a valid comparison operator
/// ```
///
/// The declarations as Reflection renders them.
///
/// ```php
/// echo new ReflectionFunction('version_compare');
/// ```
/// ```expect
/// Function [ <internal:standard> function version_compare ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $version1 ]
///     Parameter #1 [ <required> string $version2 ]
///     Parameter #2 [ <optional> ?string $operator = null ]
///   }
///   - Return [ int|bool ]
/// }
/// ```
#[php_function]
fn version_compare(
    req: &mut Request,
    version1: PhpString,
    version2: PhpString,
    #[php(default = null)] operator: Option<PhpString>,
) -> Result<Union<i64, bool>, Thrown> {
    // Both are C strings to php: each ends at its first NUL.
    let compare = version_cmp(c_str(version1.as_bytes()), c_str(version2.as_bytes()));
    let Some(op) = operator else {
        return Ok(Union::A(compare));
    };
    let holds = match op.as_bytes() {
        b"<" | b"lt" => compare == -1,
        b"<=" | b"le" => compare != 1,
        b">" | b"gt" => compare == 1,
        b">=" | b"ge" => compare != -1,
        b"==" | b"=" | b"eq" => compare == 0,
        b"!=" | b"<>" | b"ne" => compare != 0,
        _ => return Err(arg_value_error(req, 3, "be a valid comparison operator")),
    };
    Ok(Union::B(holds))
}
