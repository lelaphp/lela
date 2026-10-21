//! Reading Rust source as code: one scanner of what is not code, behind
//! the unsafe sites of a text and its code lines, so a sensor never fires
//! on a word inside a string or a comment, a budget counts what a comment
//! marker inside a literal is not, and every reader agrees on which bytes
//! are code.
//!
//! Two sensors of `unsafe.homes` read it. `permit` judges the whole written
//! file at the call and `unsafe-audit` counts the sites of the file
//! whole; before this module each carried a blanker of its own, and the
//! harness's own negative controls are strings
//! (`unsafe_sites("unsafe { a(); }")`), so the fidelity of the blanker
//! decided whether a sensor could be edited at all (#18). The matcher then
//! stayed split, `permit` on the keyword before a block or an item and the
//! audit on every word-bounded keyword, so an unsafe attribute was a site
//! for one and not the other (#46). The line counter `budget` read through
//! kept a string scanner of its own, which knew escaped double-quoted
//! strings and nothing else, so a quote inside a character literal opened
//! a string that never closed and a `/*` inside a raw string opened a
//! comment that swallowed the rest of the file (#47). A rule read two ways
//! is two rules, which is what harness-design.md §4.1 says of a rule stated
//! anywhere but the manifest, and what the two hook adapters were taught on
//! 2026-09-08. One parse of a file as Rust beside it, [`parse_file`], so every
//! reader names the file it could not parse alike.

use std::path::Path;

use anyhow::{Context, Result};

/// The character offsets in `text` where an unsafe contract is asserted,
/// so a sensor of `unsafe.homes` counts them, names their lines or asks
/// whether there is one; the text is read whole, since the token that makes
/// the keyword a site may stand on the next line. A
/// site is the keyword before a block, `fn`, `impl`, `trait` or `extern`,
/// or the unsafe attribute, `#[unsafe(no_mangle)]` and its kin: the
/// attribute asserts a contract the item's caller cannot uphold, as a block
/// does, which is why Rust 2024 spells it with the keyword and why a
/// binding outside `ext/*/src/ffi/` may not carry one. `unsafe_code` in a
/// lint attribute is not a site, nor is `is_unsafe` or the word inside a
/// string or a comment; the text is read through
/// [`without_comments_or_literals`] first.
pub fn unsafe_sites(text: &str) -> Vec<usize> {
    let code = without_comments_or_literals(text);
    let bytes = code.as_bytes();
    let word = |i: usize| {
        bytes
            .get(i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    };
    let mut sites = Vec::new();
    let mut from = 0;
    while let Some(at) = code[from..].find("unsafe") {
        let start = from + at;
        let end = start + "unsafe".len();
        from = end;
        if (start > 0 && word(start - 1)) || word(end) {
            continue;
        }
        // Where the token after the keyword starts.
        let token = code.len() - code[end..].trim_start().len();
        let rest = &code[token..];
        let asserts = rest.starts_with('{')
            || rest.starts_with('(')
            || ["fn", "impl", "trait", "extern"]
                .iter()
                .any(|item| rest.starts_with(item) && !word(token + item.len()));
        if asserts {
            // The blanked text is the text character for character, and a
            // blanked character may have been more than one byte.
            sites.push(code[..start].chars().count());
        }
    }
    sites
}

/// The text with its comments and its string, byte-string and character
/// literals blanked, so a pattern over it reads code alone. What
/// is blanked keeps its length and its newlines, so an offset into the
/// blanked text is an offset into the text.
///
/// A raw string closes on its own hash count, a block comment nests as
/// Rust's does, a backslash escapes the quote that would end a string, and
/// a lifetime is told from a character literal by the quote that does not
/// close it. A literal or a comment left open at the end of the text is
/// blanked to the end, which is what a fragment of one line is.
pub fn without_comments_or_literals(text: &str) -> String {
    scan(text, Literals::Blanked)
}

/// The code lines of a Rust text, as `(line number, code)` for every line
/// that has any: the line with its comments blanked and its literals kept,
/// `#[cfg(test)]` items left out when `skip_tests`, the attribute and the
/// `mod` or `fn` under it to the end of its braces or its `;`. What a
/// budget counts (harness-design.md §4.1): a line is code when anything on
/// it is neither blank nor a comment, so a line inside a string that spans
/// lines is one, and a line that is only a comment, however a quote in a
/// literal before it reads, is not.
pub fn code_lines(text: &str, skip_tests: bool) -> Vec<(usize, String)> {
    // Inside a `#[cfg(test)]` item: brace depth, and whether a brace has
    // opened yet (before it, a `;` ends the item).
    let mut item: Option<(usize, bool)> = None;
    scan(text, Literals::Kept)
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            if let Some((depth, opened)) = item {
                item = skip_test_item(line, depth, opened);
                return None;
            }
            let trimmed = line.trim();
            if skip_tests && trimmed.starts_with("#[cfg(test)]") {
                item = skip_test_item(trimmed.trim_start_matches("#[cfg(test)]"), 0, false);
                return None;
            }
            (!trimmed.is_empty()).then(|| (i + 1, line.to_owned()))
        })
        .collect()
}

/// Advance through one line of a test item; `None` when the item ended on it.
fn skip_test_item(line: &str, mut depth: usize, mut opened: bool) -> Option<(usize, bool)> {
    for c in line.chars() {
        match c {
            '{' => {
                depth += 1;
                opened = true;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if opened && depth == 0 {
                    return None;
                }
            }
            ';' if !opened => return None,
            _ => {}
        }
    }
    Some((depth, opened))
}

/// One pass over the text, comments blanked and literals read by `literals`.
fn scan(text: &str, literals: Literals) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut pass = Scan {
        chars: &chars,
        i: 0,
        out: String::with_capacity(text.len()),
        literals,
    };
    while pass.i < pass.chars.len() {
        if pass.comment() || pass.raw_string() || pass.string() || pass.character() {
            continue;
        }
        pass.keep();
    }
    pass.out
}

/// What a scan writes for a literal: blanks, for a pattern that must not
/// match inside one, or the literal itself, for a count of the lines that
/// are not comment.
#[derive(Clone, Copy)]
enum Literals {
    Blanked,
    Kept,
}

/// One pass over the text: where it stands and what it has written.
struct Scan<'a> {
    chars: &'a [char],
    i: usize,
    out: String,
    literals: Literals,
}

impl Scan<'_> {
    /// The character `n` past the cursor; past the end is `\0`, which no
    /// delimiter is.
    fn at(&self, n: usize) -> char {
        self.chars.get(self.i + n).copied().unwrap_or('\0')
    }

    /// Take the character under the cursor as code.
    fn keep(&mut self) {
        self.out.push(self.at(0));
        self.i += 1;
    }

    /// Blank the character under the cursor: a newline stays one, so lines
    /// are lines, and everything else is a space.
    fn blank(&mut self) {
        let c = self.at(0);
        self.out.push(if c == '\n' { '\n' } else { ' ' });
        self.i += 1;
    }

    /// Blank `n` characters.
    fn blank_n(&mut self, n: usize) {
        for _ in 0..n {
            self.blank();
        }
    }

    /// Take `n` characters of a literal, blanked or kept by the mode.
    fn literal(&mut self, n: usize) {
        for _ in 0..n {
            match self.literals {
                Literals::Blanked => self.blank(),
                Literals::Kept => self.keep(),
            }
        }
    }

    /// A line comment to the line's end, or a block comment to the end of
    /// the block, which nests.
    fn comment(&mut self) -> bool {
        if self.at(0) == '/' && self.at(1) == '/' {
            while self.i < self.chars.len() && self.at(0) != '\n' {
                self.blank();
            }
            return true;
        }
        if self.at(0) != '/' || self.at(1) != '*' {
            return false;
        }
        let mut depth = 0;
        while self.i < self.chars.len() {
            if self.at(0) == '/' && self.at(1) == '*' {
                depth += 1;
                self.blank_n(2);
            } else if self.at(0) == '*' && self.at(1) == '/' {
                depth -= 1;
                self.blank_n(2);
                if depth == 0 {
                    break;
                }
            } else {
                self.blank();
            }
        }
        true
    }

    /// A raw string with the hashes that close it: `r"…"`, `br##"…"##`.
    fn raw_string(&mut self) -> bool {
        let prefix = match (self.at(0), self.at(1)) {
            ('r', _) => 1,
            ('b', 'r') => 2,
            _ => return false,
        };
        if self.preceded_by_a_word() {
            return false;
        }
        let mut hashes = 0;
        while self.at(prefix + hashes) == '#' {
            hashes += 1;
        }
        if self.at(prefix + hashes) != '"' {
            return false;
        }
        self.literal(prefix + hashes + 1);
        while self.i < self.chars.len() {
            let closes = self.at(0) == '"' && (1..=hashes).all(|n| self.at(n) == '#');
            self.literal(1);
            if closes {
                self.literal(hashes);
                break;
            }
        }
        true
    }

    /// A string, whose backslash escapes the quote that would end it;
    /// `b"…"` is one too.
    fn string(&mut self) -> bool {
        let quote = match (self.at(0), self.at(1)) {
            ('"', _) => 1,
            ('b', '"') => 2,
            _ => return false,
        };
        self.literal(quote);
        self.literal_to_the_closing('"');
        true
    }

    /// A character literal, which a lifetime is not: `'a` carries no
    /// closing quote, and `'\n'` is escaped.
    fn character(&mut self) -> bool {
        if self.at(0) != '\'' || !(self.at(1) == '\\' || self.at(2) == '\'') {
            return false;
        }
        self.literal(1);
        self.literal_to_the_closing('\'');
        true
    }

    /// Read to the quote that closes the literal, an escaped one passing
    /// with what it escapes.
    fn literal_to_the_closing(&mut self, quote: char) {
        while self.i < self.chars.len() {
            let c = self.at(0);
            self.literal(1);
            if c == '\\' {
                self.literal(1);
            } else if c == quote {
                break;
            }
        }
    }

    /// Whether the character before the cursor can be part of an
    /// identifier, so the `r` of `str` is a name's tail and never a raw
    /// string's prefix.
    fn preceded_by_a_word(&self) -> bool {
        self.i
            .checked_sub(1)
            .and_then(|i| self.chars.get(i))
            .is_some_and(|c| c.is_alphanumeric() || *c == '_')
    }
}

/// The Rust file at `file`, parsed, an error naming it where it cannot be
/// read or is not one.
pub(crate) fn parse_file(file: &Path) -> Result<syn::File> {
    parsed(&crate::read(file)?, file)
}

/// `text`, the Rust file at `file`, parsed.
pub(crate) fn parsed(text: &str, file: &Path) -> Result<syn::File> {
    syn::parse_file(text).with_context(|| format!("parsing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::{code_lines, unsafe_sites, without_comments_or_literals as code};

    /// How many lines a budget counts.
    fn counted(text: &str) -> usize {
        code_lines(text, true).len()
    }

    /// What the text reads as once a literal or a comment of `blanks`
    /// characters between `before` and `after` is blanked; the blanked text
    /// keeps its length, so the count is the literal's own.
    fn blanked(before: &str, blanks: usize, after: &str) -> String {
        format!("{before}{}{after}", " ".repeat(blanks))
    }

    #[test]
    fn a_site_is_the_keyword_before_a_block_or_an_item_or_the_unsafe_attribute() {
        assert_eq!(unsafe_sites("unsafe { a(); } unsafe fn b()"), vec![0, 16]);
        assert_eq!(unsafe_sites("pub unsafe impl A for B {}"), vec![4]);
        assert_eq!(
            unsafe_sites("unsafe trait T {}\nunsafe extern \"C\" {}"),
            vec![0, 18]
        );
        assert_eq!(
            unsafe_sites("#[unsafe(no_mangle)]\npub extern \"C\" fn f() {}"),
            vec![2]
        );
        assert_eq!(unsafe_sites("#![unsafe(export_name = \"f\")]"), vec![3]);
    }

    #[test]
    fn the_token_that_makes_a_site_may_stand_on_a_later_line_past_a_comment() {
        assert_eq!(unsafe_sites("unsafe\n{ a() }"), vec![0]);
        assert_eq!(unsafe_sites("pub unsafe // why\n    fn f() {}"), vec![4]);
        assert_eq!(unsafe_sites("unsafe /* one */ {}"), vec![0]);
        assert!(unsafe_sites("unsafe\n").is_empty());
    }

    #[test]
    fn an_offset_counts_characters_so_a_wide_literal_before_a_site_moves_it_by_one() {
        assert_eq!(unsafe_sites("let s = \"é\"; unsafe {}"), vec![13]);
        assert_eq!(unsafe_sites("let s = \"e\"; unsafe {}"), vec![13]);
    }

    #[test]
    fn the_attribute_word_a_name_and_the_keyword_in_a_string_or_a_comment_are_not_sites() {
        assert!(unsafe_sites("#![forbid(unsafe_code)]").is_empty());
        assert!(unsafe_sites("let is_unsafe = 1; let unsafe_ = 2;").is_empty());
        assert!(unsafe_sites("let unsafe_fn = 1; unsafe_impl()").is_empty());
        assert!(unsafe_sites("unsafe fnord()").is_empty());
        assert_eq!(
            unsafe_sites("let s = \"unsafe { \\\" unsafe\"; unsafe {}"),
            vec![30]
        );
        assert!(unsafe_sites("// unsafe { a(); }\n/* #[unsafe(no_mangle)] */").is_empty());
        assert!(unsafe_sites(r##"let s = r#"unsafe fn b()"#;"##).is_empty());
    }

    #[test]
    fn what_is_blanked_keeps_its_length_so_an_offset_is_an_offset() {
        for text in [
            "let s = \"a\"; // b\nlet c = 'x';",
            "/* a */ r#\"b\"# 'c' b\"d\"",
        ] {
            assert_eq!(code(text).chars().count(), text.chars().count(), "{text}");
        }
    }

    #[test]
    fn a_keyword_in_a_string_or_a_comment_is_not_code_and_the_code_is_kept() {
        assert_eq!(code("let x = 1;"), "let x = 1;");
        assert_eq!(
            code(r#"let s = "unsafe { }";"#),
            blanked("let s = ", 12, ";")
        );
        assert_eq!(
            code("// unsafe { }\nlet x = 1;"),
            blanked("", "// unsafe { }".len(), "\nlet x = 1;")
        );
        assert_eq!(
            code("/* unsafe /* fn */ */ let x = 1;"),
            blanked("", "/* unsafe /* fn */ */".len(), " let x = 1;")
        );
    }

    #[test]
    fn a_raw_string_closes_on_its_own_hashes_and_a_name_ending_in_r_opens_none() {
        assert_eq!(code("let s = r#\"a\"#;"), blanked("let s = ", 6, ";"));
        assert_eq!(code("let s = r##\"a\"#\"##;"), blanked("let s = ", 10, ";"));
        // `str` ends in no raw string, so what follows it is still code.
        assert_eq!(
            code("let s: str = x; unsafe {}"),
            "let s: str = x; unsafe {}"
        );
    }

    #[test]
    fn an_escaped_quote_does_not_close_a_string_and_a_lifetime_is_not_a_character() {
        assert_eq!(code(r#"let s = "a\"b"; x"#), blanked("let s = ", 6, "; x"));
        assert_eq!(code("fn f<'a>(x: &'a str) {}"), "fn f<'a>(x: &'a str) {}");
        assert_eq!(code(r"let c = '\''; x"), blanked("let c = ", 4, "; x"));
    }

    #[test]
    fn a_literal_left_open_is_blanked_to_the_end_which_is_what_one_line_of_it_is() {
        assert_eq!(code(r#"let s = "unsafe {"#), "let s =          ");
        assert_eq!(code("/* unsafe {"), "           ");
    }

    #[test]
    fn blank_and_comment_lines_are_not_counted() {
        let text = "//! doc\n\n/// item doc\nfn a() {} // trailing\n  \n/* one */ let x = 1;\n";
        assert_eq!(counted(text), 2);
    }

    #[test]
    fn block_comments_span_lines_and_nest() {
        let text = "fn a() {}\n/* start\n/* nested */ still\nend */ fn b() {}\nfn c() {}\n";
        assert_eq!(counted(text), 3);
    }

    #[test]
    fn a_cfg_test_item_is_left_out() {
        let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {\n    }\n}\nfn b() {}\n";
        assert_eq!(counted(text), 2);
        let text = "fn a() {}\n#[cfg(test)]\nmod tests;\nfn b() {}\n";
        assert_eq!(counted(text), 2);
        let text = "#[cfg(test)] fn t() { let x = 1; }\nfn b() {}\n";
        assert_eq!(counted(text), 1);
        assert_eq!(code_lines(text, false).len(), 2);
    }

    #[test]
    fn a_comment_marker_inside_a_literal_is_code() {
        let text = "let url = \"https://x\"; // real\nlet s = \"/* not\";\nfn a() {}\n";
        assert_eq!(counted(text), 3);
        assert_eq!(
            code_lines(text, true)[0].1,
            blanked("let url = \"https://x\"; ", "// real".len(), "")
        );
        let text = "let s = r#\"\n/* c // d\n\"#;\nfn a() {}\n";
        assert_eq!(
            code_lines(text, true),
            vec![
                (1, "let s = r#\"".to_owned()),
                (2, "/* c // d".to_owned()),
                (3, "\"#;".to_owned()),
                (4, "fn a() {}".to_owned()),
            ]
        );
    }

    #[test]
    fn a_quote_inside_a_character_literal_opens_no_string() {
        // The block comment after the character literal is one, and it
        // ends where it ends.
        let text = "let q = '\"'; /* a\nb */\nfn a() {}\n";
        assert_eq!(
            code_lines(text, true),
            vec![
                (1, "let q = '\"';     ".to_owned()),
                (3, "fn a() {}".to_owned()),
            ]
        );
    }

    #[test]
    fn a_code_line_is_the_line_without_its_comments() {
        let text = "fn a() {} // unsafe here is a word\n/* unsafe */ let x = 1;\n#[cfg(test)]\nfn t() { unsafe {} }\n";
        let lines = code_lines(text, true);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            (
                1,
                blanked("fn a() {} ", "// unsafe here is a word".len(), "")
            )
        );
        assert_eq!(
            lines[1],
            (2, blanked("", "/* unsafe */".len(), " let x = 1;"))
        );
        assert_eq!(code_lines(text, false).len(), 4);
    }
}
