//! `Z/ext/standard/levenshtein.c`: `reference_levdist`, the two-row
//! dynamic programme with php's three costs.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Emalloc, PhpString};

/// `reference_levdist(string1, string2, cost_ins, cost_rep, cost_del)`.
/// `reference_levdist`: the distance, or `None` where the memory limit
/// refused a row, php 8.5.12's `false`. The two rows are php's
/// `safe_femalloc`'d ones, eight bytes for each byte of the second
/// string and one more, asked for fallibly on the request's heap and
/// held while the distance is reckoned.
fn levdist<'a>(
    req: &mut Request,
    (mut a, mut b): (&'a [u8], &'a [u8]),
    ins: i64,
    rep: i64,
    del: i64,
) -> Result<Option<i64>, Thrown> {
    let long = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    if a.is_empty() {
        return Ok(Some(long(b.len()).wrapping_mul(ins)));
    }
    if b.is_empty() {
        return Ok(Some(long(a.len()).wrapping_mul(del)));
    }
    if a.len() < b.len() && ins == rep && rep == del {
        std::mem::swap(&mut a, &mut b);
    }
    // "Each row is eight times the length of string2."
    let row = (b.len() + 1).saturating_mul(8);
    let Some(first) = req.memory_mut().fallible(|req| Emalloc::new(req, row))? else {
        return Ok(None);
    };
    let Some(second) = req.memory_mut().fallible(|req| Emalloc::new(req, row))? else {
        first.free(req);
        return Ok(None);
    };
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
    second.free(req);
    first.free(req);
    Ok(Some(p1[b.len()]))
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
///
/// The declarations as Reflection renders them.
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
///
/// Since 8.5.12 the two rows, eight bytes for each byte of the second
/// string, are asked for fallibly: past the memory limit one is refused,
/// and the limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '16M');
/// $b = str_repeat('b', 1 << 20);
/// var_dump(levenshtein('kitten', 'sitting'), levenshtein('ab', $b), levenshtein('ab', $b, 1, 2, 3), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 16777216 bytes exhausted (tried to allocate 8404992 bytes) in %s on line 4
/// ```
#[php_function]
fn levenshtein(
    req: &mut Request,
    string1: PhpString,
    string2: PhpString,
    #[php(default = 1)] insertion_cost: i64,
    #[php(default = 1)] replacement_cost: i64,
    #[php(default = 1)] deletion_cost: i64,
) -> Result<i64, Thrown> {
    let distance = levdist(
        req,
        (string1.as_bytes(), string2.as_bytes()),
        insertion_cost,
        replacement_cost,
        deletion_cost,
    )?;
    // RETURN_OOM() where a row was refused.
    Ok(distance.unwrap_or(0))
}
