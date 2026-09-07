//! What a line budget counts (design §4.1): non-blank, non-comment lines,
//! tests excluded. One count for Rust, one for everything else.

/// The counted lines of one file, by its extension: Rust through
/// `rust`, TOML without its `#` comments, and every other text as its
/// non-blank lines.
pub fn count(path: &str, text: &str) -> usize {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("rs") => rust(text),
        Some("toml") => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .count(),
        _ => text.lines().filter(|l| !l.trim().is_empty()).count(),
    }
}

/// Non-blank, non-comment lines of Rust, with every `#[cfg(test)]` item
/// left out: the attribute, and the `mod` or `fn` under it to the end of
/// its braces or its `;`.
pub fn rust(text: &str) -> usize {
    code(text, true).len()
}

/// The code of each line with its comments cut out, as `(line number,
/// code)` for every line that has any, `#[cfg(test)]` items left out when
/// `skip_tests`. What an audit reads, so a mention in a comment is not a
/// site.
pub fn code(text: &str, skip_tests: bool) -> Vec<(usize, String)> {
    let mut counter = Counter {
        skip_tests,
        ..Counter::default()
    };
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let code = counter.code_on(line)?;
            (!code.trim().is_empty()).then(|| (i + 1, code))
        })
        .collect()
}

#[derive(Default)]
struct Counter {
    skip_tests: bool,
    /// Nesting depth of `/* */`, which nests in Rust.
    block: usize,
    /// Inside a `#[cfg(test)]` item: brace depth, and whether a brace has
    /// opened yet (before it, a `;` ends the item).
    test: Option<(usize, bool)>,
}

impl Counter {
    /// The code on the line outside comments, tracking block comments
    /// across lines; `None` inside a skipped test item.
    fn code_on(&mut self, line: &str) -> Option<String> {
        if let Some((depth, opened)) = self.test {
            self.test = skip_test_item(line, depth, opened);
            return None;
        }
        let trimmed = line.trim();
        if self.skip_tests && self.block == 0 && trimmed.starts_with("#[cfg(test)]") {
            self.test = skip_test_item(trimmed.trim_start_matches("#[cfg(test)]"), 0, false);
            return None;
        }
        let mut code = String::new();
        let mut rest = line;
        while !rest.is_empty() {
            if self.block > 0 {
                match (rest.find("*/"), rest.find("/*")) {
                    (Some(close), Some(open)) if open < close => {
                        self.block += 1;
                        rest = &rest[open + 2..];
                    }
                    (Some(close), _) => {
                        self.block -= 1;
                        rest = &rest[close + 2..];
                    }
                    (None, Some(open)) => {
                        self.block += 1;
                        rest = &rest[open + 2..];
                    }
                    (None, None) => break,
                }
                continue;
            }
            let line_comment = rest.find("//");
            let block_open = rest.find("/*");
            // A `//` or `/*` inside a string literal is text, not a comment:
            // copy the literal through its closing quote first.
            if let Some(quote) = rest.find('"')
                && line_comment.is_none_or(|l| quote < l)
                && block_open.is_none_or(|b| quote < b)
            {
                let end = string_end(&rest[quote + 1..]).map_or(rest.len(), |e| quote + 1 + e);
                code.push_str(&rest[..end]);
                rest = &rest[end..];
                continue;
            }
            match (line_comment, block_open) {
                (Some(l), Some(b)) if l < b => {
                    code.push_str(&rest[..l]);
                    break;
                }
                (Some(l), None) => {
                    code.push_str(&rest[..l]);
                    break;
                }
                (_, Some(b)) => {
                    code.push_str(&rest[..b]);
                    code.push(' ');
                    self.block += 1;
                    rest = &rest[b + 2..];
                }
                (None, None) => {
                    code.push_str(rest);
                    break;
                }
            }
        }
        Some(code)
    }
}

/// The index just past the closing quote of a string literal whose opening
/// quote precedes `s`, escapes honoured; `None` when it runs past the line.
fn string_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::{count, rust};

    #[test]
    fn blank_and_comment_lines_are_not_counted() {
        let text = "//! doc\n\n/// item doc\nfn a() {} // trailing\n  \n/* one */ let x = 1;\n";
        assert_eq!(rust(text), 2);
    }

    #[test]
    fn block_comments_span_lines_and_nest() {
        let text = "fn a() {}\n/* start\n/* nested */ still\nend */ fn b() {}\nfn c() {}\n";
        assert_eq!(rust(text), 3);
    }

    #[test]
    fn a_cfg_test_module_is_left_out() {
        let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {\n    }\n}\nfn b() {}\n";
        assert_eq!(rust(text), 2);
        let text = "fn a() {}\n#[cfg(test)]\nmod tests;\nfn b() {}\n";
        assert_eq!(rust(text), 2);
        let text = "#[cfg(test)] fn t() { let x = 1; }\nfn b() {}\n";
        assert_eq!(rust(text), 1);
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_text() {
        let text = "let url = \"https://x\"; // real\nlet s = \"/* not\";\nfn a() {}\n";
        assert_eq!(rust(text), 3);
        let code = super::code(text, true);
        assert_eq!(code[0].1, "let url = \"https://x\"; ");
    }

    #[test]
    fn code_is_the_line_without_its_comments() {
        let text = "fn a() {} // unsafe here is a word\n/* unsafe */ let x = 1;\n#[cfg(test)]\nfn t() { unsafe {} }\n";
        let code = super::code(text, true);
        assert_eq!(code.len(), 2);
        assert_eq!(code[0], (1, "fn a() {} ".to_owned()));
        assert!(!code[1].1.contains("unsafe"), "{:?}", code[1]);
        assert_eq!(super::code(text, false).len(), 4);
    }

    #[test]
    fn other_texts_count_non_blank_lines_and_toml_drops_its_comments() {
        assert_eq!(count("a.md", "# Title\n\ntext\n"), 2);
        assert_eq!(count("a.toml", "# c\nkey = 1\n\n[t]\n"), 2);
        assert_eq!(count("a.rs", "// c\nfn a() {}\n"), 1);
    }
}
