//! `Z/ext/standard/levenshtein.c`: `reference_levdist`, the two-row
//! dynamic programme with php's three costs.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::FunctionDecl;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::optional_long;
use crate::{argument_count_between, string_argument};

/// The one.
///
/// ```php
/// echo new ReflectionFunction('levenshtein');
/// ```
/// ```expect
/// Function [ <internal:standard> function levenshtein ] {
///
///   - Parameters [5] {
///     Parameter #0 [ <required> string $string1 ]
///     Parameter #1 [ <required> string $string2 ]
///     Parameter #2 [ <optional> int $insertion_cost = 1 ]
///     Parameter #3 [ <optional> int $replacement_cost = 1 ]
///     Parameter #4 [ <optional> int $deletion_cost = 1 ]
///   }
///   - Return [ int ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(
        stub(
            "levenshtein(string $string1, string $string2, int $insertion_cost = 1, int $replacement_cost = 1, int $deletion_cost = 1): int",
        ),
        levenshtein as Entry,
    )]
}

/// `reference_levdist(string1, string2, cost_ins, cost_rep, cost_del)`.
fn levdist<'a>(mut a: &'a [u8], mut b: &'a [u8], ins: i64, rep: i64, del: i64) -> i64 {
    let long = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    if a.is_empty() {
        return long(b.len()).wrapping_mul(ins);
    }
    if b.is_empty() {
        return long(a.len()).wrapping_mul(del);
    }
    if a.len() < b.len() && ins == rep && rep == del {
        std::mem::swap(&mut a, &mut b);
    }
    let mut p1: Vec<i64> = (0..=b.len()).map(|i| long(i).wrapping_mul(ins)).collect();
    let mut p2: Vec<i64> = vec![0; b.len() + 1];
    for &x in a {
        p2[0] = p1[0].wrapping_add(del);
        for (i2, &y) in b.iter().enumerate() {
            let mut c0 = p1[i2].wrapping_add(if x == y { 0 } else { rep });
            let c1 = p1[i2 + 1].wrapping_add(del);
            if c1 < c0 {
                c0 = c1;
            }
            let c2 = p2[i2].wrapping_add(ins);
            if c2 < c0 {
                c0 = c2;
            }
            p2[i2 + 1] = c0;
        }
        std::mem::swap(&mut p1, &mut p2);
    }
    p1[b.len()]
}

/// `levenshtein(string $string1, string $string2, int $insertion_cost
/// = 1, int $replacement_cost = 1, int $deletion_cost = 1): int`.
///
/// ```php
/// var_dump(levenshtein("kitten", "sitting"), levenshtein("", "abc"), levenshtein("abc", ""), levenshtein("abc", "abc"), levenshtein("abc", "adc", 1, 5, 1), levenshtein("ab", "abcd", 3, 1, 2), levenshtein("abcd", "ab", 3, 1, 2), levenshtein(str_repeat("a", 300), str_repeat("b", 300)));
/// ```
/// ```expect
/// int(3)
/// int(3)
/// int(3)
/// int(0)
/// int(2)
/// int(6)
/// int(4)
/// int(300)
/// ```
fn levenshtein(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 5)?;
    let a = string_argument(req, 1)?;
    let b = string_argument(req, 2)?;
    let ins = optional_long(req, 3, 1)?;
    let rep = optional_long(req, 4, 1)?;
    let del = optional_long(req, 5, 1)?;
    Ok(Value::int(levdist(
        a.as_bytes(),
        b.as_bytes(),
        ins,
        rep,
        del,
    )))
}
