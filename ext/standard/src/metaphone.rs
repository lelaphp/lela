//! `Z/ext/standard/metaphone.c`: Michael G Schwern's metaphone as php
//! carries it, the traditional variant, over the word up to its first
//! NUL.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::FunctionDecl;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::Value;

use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::{optional_long, string_value};
use crate::{argument_count_between, string_argument};

/// The one.
///
/// ```php
/// echo new ReflectionFunction('metaphone');
/// ```
/// ```expect
/// Function [ <internal:standard> function metaphone ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> int $max_phonemes = 0 ]
///   }
///   - Return [ string ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(
        stub("metaphone(string $string, int $max_phonemes = 0): string"),
        metaphone as Entry,
    )]
}

/// `_codes`, the letter classes: 1 a vowel, 2 unchanged, 4 affects a
/// following H, 8 softens C and G, 16 stops GH becoming F.
const CODES: [u8; 26] = [
    1, 16, 4, 16, 9, 2, 4, 16, 9, 2, 0, 2, 2, 2, 1, 4, 0, 2, 4, 4, 1, 0, 0, 0, 8, 0,
];

/// `encode(c)` for an uppercase letter, zero for anything else.
fn class(c: u8) -> u8 {
    if c.is_ascii_uppercase() {
        CODES[usize::from(c - b'A')]
    } else {
        0
    }
}

fn vowel(c: u8) -> bool {
    class(c) & 1 != 0
}
fn affect_h(c: u8) -> bool {
    class(c) & 4 != 0
}
fn make_soft(c: u8) -> bool {
    class(c) & 8 != 0
}
fn no_gh_to_f(c: u8) -> bool {
    class(c) & 16 != 0
}

/// `metaphone(word, word_len, max_phonemes, &phoned_word, 1)`. The
/// arms stand letter by letter in the order php's `switch` has them,
/// so two letters with one answer keep two arms.
#[allow(
    clippy::too_many_lines,
    clippy::match_same_arms,
    clippy::collapsible_match
)]
fn phonize(word: &[u8], max: usize) -> Vec<u8> {
    // The word is read as the C string it is: to its first NUL.
    let word = word.split(|c| *c == 0).next().unwrap_or(&[]);
    let raw = |i: usize| word.get(i).copied().unwrap_or(0);
    let up = |i: usize| raw(i).to_ascii_uppercase();
    let mut out = Vec::new();
    let mut w = 0usize;
    while !raw(w).is_ascii_alphabetic() {
        if raw(w) == 0 {
            return out;
        }
        w += 1;
    }
    // The first letter, handled apart.
    let first = up(w);
    let next = up(w + 1);
    match first {
        b'A' if next == b'E' => {
            out.push(b'E');
            w += 2;
        }
        b'A' => {
            out.push(b'A');
            w += 1;
        }
        b'G' | b'K' | b'P' if next == b'N' => {
            out.push(b'N');
            w += 2;
        }
        b'W' if next == b'R' => {
            out.push(b'R');
            w += 2;
        }
        b'W' if next == b'H' || vowel(next) => {
            out.push(b'W');
            w += 2;
        }
        b'X' => {
            out.push(b'S');
            w += 1;
        }
        b'E' | b'I' | b'O' | b'U' => {
            out.push(first);
            w += 1;
        }
        _ => {}
    }
    while raw(w) != 0 && (max == 0 || out.len() < max) {
        if !raw(w).is_ascii_alphabetic() {
            w += 1;
            continue;
        }
        let c = up(w);
        let back = |n: usize| if w >= n { up(w - n) } else { 0 };
        let prev = back(1);
        // Drop duplicates, except CC.
        if c == prev && c != b'C' {
            w += 1;
            continue;
        }
        let next = up(w + 1);
        let after_next = if raw(w + 1) != 0 { up(w + 2) } else { 0 };
        // `Look_Ahead_Letter(n)`: `n` on, or the NUL met first.
        let ahead = |n: usize| {
            let mut idx = 0;
            while raw(w + idx) != 0 && idx < n {
                idx += 1;
            }
            up(w + idx)
        };
        let mut skip = 0;
        match c {
            b'B' if prev != b'M' => out.push(b'B'),
            b'C' if make_soft(next) => {
                if next == b'I' && after_next == b'A' {
                    out.push(b'X');
                } else if prev != b'S' {
                    out.push(b'S');
                }
            }
            b'C' if next == b'H' => {
                out.push(b'X');
                skip += 1;
            }
            b'C' => out.push(b'K'),
            b'D' if next == b'G' && make_soft(after_next) => {
                out.push(b'J');
                skip += 1;
            }
            b'D' => out.push(b'T'),
            b'G' if next == b'H' => {
                if !(no_gh_to_f(back(3)) || back(4) == b'H') {
                    out.push(b'F');
                    skip += 1;
                }
            }
            b'G' if next == b'N' => {
                let dropped =
                    !after_next.is_ascii_alphabetic() || (after_next == b'E' && ahead(3) == b'D');
                if !dropped {
                    out.push(b'K');
                }
            }
            b'G' if make_soft(next) && prev != b'G' => out.push(b'J'),
            b'G' => out.push(b'K'),
            b'H' if vowel(next) && !affect_h(prev) => out.push(b'H'),
            b'K' if prev != b'C' => out.push(b'K'),
            b'P' => out.push(if next == b'H' { b'F' } else { b'P' }),
            b'Q' => out.push(b'K'),
            b'S' if next == b'I' && (after_next == b'O' || after_next == b'A') => {
                out.push(b'X');
            }
            b'S' if next == b'H' => {
                out.push(b'X');
                skip += 1;
            }
            b'S' => out.push(b'S'),
            b'T' if next == b'I' && (after_next == b'O' || after_next == b'A') => {
                out.push(b'X');
            }
            b'T' if next == b'H' => {
                out.push(b'0');
                skip += 1;
            }
            b'T' if !(next == b'C' && after_next == b'H') => out.push(b'T'),
            b'V' => out.push(b'F'),
            b'W' if vowel(next) => out.push(b'W'),
            b'X' => {
                out.push(b'K');
                out.push(b'S');
            }
            b'Y' if vowel(next) => out.push(b'Y'),
            b'Z' => out.push(b'S'),
            b'F' | b'J' | b'L' | b'M' | b'N' | b'R' => out.push(c),
            _ => {}
        }
        w += 1 + skip;
    }
    out
}

/// `metaphone(string $string, int $max_phonemes = 0): string`: the
/// word's metaphone key, at most `$max_phonemes` of it when that is
/// not zero; `X` writes `KS` whole, so the key may end one past.
///
/// ```php
/// var_dump(metaphone("Thompson"), metaphone("knight"), metaphone("Schmidt"), metaphone("character"), metaphone("Xavier"), metaphone("wright"), metaphone("aero"), metaphone("laugh"), metaphone("dodge"), metaphone("gnome"), metaphone("Thumb", 2), metaphone("axe", 1), metaphone("123"), metaphone(""), metaphone("ci cia sch tio"));
/// try { metaphone("a", -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "0MPSN"
/// string(3) "NFT"
/// string(5) "SXMTT"
/// string(5) "XRKTR"
/// string(3) "SFR"
/// string(3) "RFT"
/// string(2) "ER"
/// string(2) "LF"
/// string(2) "TJ"
/// string(2) "NM"
/// string(2) "0M"
/// string(1) "A"
/// string(0) ""
/// string(0) ""
/// string(5) "SXSXX"
/// metaphone(): Argument #2 ($max_phonemes) must be greater than or equal to 0
/// ```
fn metaphone(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let phones = optional_long(req, 2, 0)?;
    if phones < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    let out = phonize(
        string.as_bytes(),
        usize::try_from(phones).unwrap_or(usize::MAX),
    );
    string_value(req, &out)
}
