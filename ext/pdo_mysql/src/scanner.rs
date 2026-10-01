//! `mysql_sql_parser.re`: `pdo_mysql_scanner`, the placeholder scanner
//! PDO runs over a `MySQL` query in place of its default one. Beside the
//! default's rules it reads a backslash escape inside a quoted string, a
//! backquoted identifier, and the `#` and `-- ` comments, so a `?` or a
//! `:name` inside any of them is text. Each rule is re2c's, and the
//! longest match wins, as re2c's does.

use php_ext_pdo::connector::Token;

/// `SPECIALS`.
fn special(b: u8) -> bool {
    matches!(b, b':' | b'?' | b'"' | b'\'' | b'`' | b'/' | b'#' | b'-')
}

/// `pdo_mysql_scanner`: the token at `at` and where it ends; `None` at the
/// end of the query, `YYFILL`'s `PDO_PARSER_EOI`. No rule matches a NUL
/// (`ANYNOEOF` is `[\001-\377]`), so one inside the query ends the token
/// before it and is a byte of text of its own, the scan going on past it.
pub(crate) fn scan(sql: &[u8], at: usize) -> Option<(Token, usize)> {
    let first = *sql.get(at)?;
    if first == 0 {
        return Some((Token::Text, at + 1));
    }
    let byte = |i: usize| sql.get(i).copied().filter(|b| *b != 0);
    let run = |from: usize, pick: &dyn Fn(u8) -> bool| {
        let mut end = from;
        while byte(end).is_some_and(pick) {
            end += 1;
        }
        end
    };
    // A quoted string: a doubled quote and, for `"` and `'`, a backslash
    // and the byte after it run on; the longest closing quote wins.
    let quoted = |quote: u8, escapes: bool| {
        let mut best = None;
        let mut end = at + 1;
        loop {
            match byte(end) {
                Some(b'\\') if escapes => match byte(end + 1) {
                    Some(_) => end += 2,
                    None => return best,
                },
                Some(b) if b == quote => {
                    best = Some(end + 1);
                    if byte(end + 1) != Some(quote) {
                        return best;
                    }
                    end += 2;
                }
                Some(_) => end += 1,
                None => return best,
            }
        }
    };
    let to_line_end = |from: usize| run(from, &|b| b != b'\n');
    match first {
        b'"' | b'\'' => Some((Token::Text, quoted(first, true).unwrap_or(at + 1))),
        b'`' => Some((Token::Text, quoted(first, false).unwrap_or(at + 1))),
        b':' | b'?' if byte(at + 1) == Some(first) => Some((Token::Text, run(at, &|b| b == first))),
        b':' => {
            let end = run(at + 1, &|b| b.is_ascii_alphanumeric() || b == b'_');
            Some(if end > at + 1 {
                (Token::Bind, end)
            } else {
                (Token::Text, at + 1)
            })
        }
        b'?' => Some((Token::Position, at + 1)),
        b'/' if byte(at + 1) == Some(b'*') => {
            let mut end = at + 2;
            loop {
                match (byte(end), byte(end + 1)) {
                    (Some(b'*'), Some(b'/')) => return Some((Token::Text, end + 2)),
                    (Some(_), _) => end += 1,
                    (None, _) => return Some((Token::Text, at + 1)),
                }
            }
        }
        b'-' if byte(at + 1) == Some(b'-')
            && matches!(byte(at + 2), Some(b' ' | b'\t' | 0x0B | 0x0C | b'\r')) =>
        {
            Some((Token::Text, to_line_end(at + 3)))
        }
        b'#' => Some((Token::Text, to_line_end(at + 1))),
        b if special(b) => Some((Token::Text, at + 1)),
        _ => Some((Token::Text, run(at, &|b| !special(b)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(sql: &[u8]) -> Vec<(Token, &[u8])> {
        let mut out = Vec::new();
        let mut at = 0;
        while let Some((token, end)) = scan(sql, at) {
            if token != Token::Text {
                out.push((token, &sql[at..end]));
            }
            at = end;
        }
        out
    }

    #[test]
    fn placeholders_inside_strings_comments_and_identifiers_are_text() {
        let sql = b"SELECT ? , 'a\\'?' , \"b\"\"?\" , `c``?` , :name # ? :x\n, -- ?\n:y /* ? */";
        assert_eq!(
            tokens(sql),
            [
                (Token::Position, &b"?"[..]),
                (Token::Bind, b":name"),
                (Token::Bind, b":y")
            ]
        );
    }

    #[test]
    fn a_nul_is_text_and_the_scan_goes_on() {
        assert_eq!(
            tokens(b"SELECT \"a\0b\", ?"),
            [(Token::Position, &b"?"[..])]
        );
    }

    #[test]
    fn a_dash_dash_without_a_space_is_no_comment() {
        assert_eq!(tokens(b"SELECT 1--?"), [(Token::Position, &b"?"[..])]);
    }
}
