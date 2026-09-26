//! The scanner's view as `token_get_all` answers it: php's scanner view,
//! `php_ext::syntax::scanned`, which `highlight_string` and the CLI's
//! `-s` and `-w` read too, cut where `tokenize()` stops. That stop is the
//! tokenizer's alone: three tokens after `T_HALT_COMPILER` that are not
//! whitespace, an open tag or a comment, whatever the three are, and
//! the rest of the source one `T_INLINE_HTML` on the third's line,
//! since php's `tokenize()` counts them and never reads the `();` the
//! lexer does (`Z/ext/tokenizer/tokenizer.c:355-371`).

use php_ext::syntax::{ScannedToken, Token, scanned};

/// php's tokens for `source` over the lexer's stream of it, whether
/// `token_get_all` runs the parser or not: what `TOKEN_PARSE` changes is
/// decided in [`super::feedback`] and put on these rows after.
pub(super) fn tokens(source: &[u8], tokens: &[Token]) -> Vec<ScannedToken> {
    let mut out = scanned(source, tokens);
    let mut halt: Option<u8> = None;
    for at in 0..out.len() {
        match out[at].name {
            Some("T_HALT_COMPILER") => halt = Some(3),
            Some("T_WHITESPACE" | "T_OPEN_TAG" | "T_COMMENT" | "T_DOC_COMMENT") => {}
            _ => {
                let Some(left) = &mut halt else {
                    continue;
                };
                *left -= 1;
                if *left == 0 {
                    let (rest, line) = (out[at].end, out[at].line);
                    out.truncate(at + 1);
                    if rest < source.len() {
                        out.push(ScannedToken {
                            name: Some("T_INLINE_HTML"),
                            start: rest,
                            end: source.len(),
                            line,
                        });
                    }
                    break;
                }
            }
        }
    }
    out
}
