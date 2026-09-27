//! `Z/ext/standard/levenshtein.c`: `reference_levdist`, the two-row
//! dynamic programme with php's three costs.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::attr::php_function;
use php_ext::values::PhpString;

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
#[php_function]
fn levenshtein(
    string1: PhpString,
    string2: PhpString,
    #[php(default = 1)] insertion_cost: i64,
    #[php(default = 1)] replacement_cost: i64,
    #[php(default = 1)] deletion_cost: i64,
) -> i64 {
    levdist(
        string1.as_bytes(),
        string2.as_bytes(),
        insertion_cost,
        replacement_cost,
        deletion_cost,
    )
}
