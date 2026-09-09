//! Reading Rust source as code: one function that returns the unsafe
//! sites of a text, over one blanker of what is not code, so a sensor
//! never fires on a word inside a string or a comment and the two sensors
//! of one rule agree on what a site is.
//!
//! Two sensors of `unsafe.homes` read it. `permit` judges the whole written
//! file at the tool call and `unsafe-audit` counts the sites of one line
//! after `lines::code` has taken the comments away; before this module each
//! carried a blanker of its own, and the harness's own negative controls
//! are strings (`unsafe_sites("unsafe { a(); }")`), so the fidelity of the
//! blanker decided whether a sensor could be edited at all (#18). The
//! matcher then stayed split, `permit` on the keyword before a block or an
//! item and the audit on every word-bounded keyword, so an unsafe attribute
//! was a site for one and not the other (#46). A rule read two ways is two
//! rules, which is what harness-design.md §4.1 says of a rule stated
//! anywhere but the manifest, and what the two hook adapters were taught on
//! 2026-09-08.

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
/// literals blanked, so a pattern over the result reads code alone. What
/// is blanked keeps its length and its newlines, so an offset into the
/// result is an offset into the text.
///
/// A raw string closes on its own hash count, a block comment nests as
/// Rust's does, a backslash escapes the quote that would end a string, and
/// a lifetime is told from a character literal by the quote that does not
/// close it. A literal or a comment left open at the end of the text is
/// blanked to the end, which is what a fragment of one line is.
pub fn without_comments_or_literals(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut scan = Scan {
        chars: &chars,
        i: 0,
        out: String::with_capacity(text.len()),
    };
    while scan.i < scan.chars.len() {
        if scan.comment() || scan.raw_string() || scan.string() || scan.character() {
            continue;
        }
        scan.keep();
    }
    scan.out
}

/// One pass over the text: where it stands and what it has written.
struct Scan<'a> {
    chars: &'a [char],
    i: usize,
    out: String,
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
        self.blank_n(prefix + hashes + 1);
        while self.i < self.chars.len() {
            let closes = self.at(0) == '"' && (1..=hashes).all(|n| self.at(n) == '#');
            self.blank();
            if closes {
                self.blank_n(hashes);
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
        self.blank_n(quote);
        self.blank_to_the_closing('"');
        true
    }

    /// A character literal, which a lifetime is not: `'a` carries no
    /// closing quote, and `'\n'` is escaped.
    fn character(&mut self) -> bool {
        if self.at(0) != '\'' || !(self.at(1) == '\\' || self.at(2) == '\'') {
            return false;
        }
        self.blank_n(1);
        self.blank_to_the_closing('\'');
        true
    }

    /// Blank to the quote that closes the literal, an escaped one passing
    /// with what it escapes.
    fn blank_to_the_closing(&mut self, quote: char) {
        while self.i < self.chars.len() {
            let c = self.at(0);
            self.blank();
            if c == '\\' {
                self.blank();
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

#[cfg(test)]
mod tests {
    use super::{unsafe_sites, without_comments_or_literals as code};

    /// What the text reads as once a literal of `blanks` characters
    /// between `before` and `after` is blanked; the blanked text keeps its
    /// length, so the count is the literal's own.
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
}
