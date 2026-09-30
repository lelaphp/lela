//! `Z/Zend/zend_highlight.c`: `zend_highlight`, a source as HTML with
//! each token in the colour `php_get_highlight_struct` reads from the
//! five `highlight.*` entries, through `zend_html_puts`; and `zend_strip`,
//! the source with its comments dropped and each stretch of whitespace one
//! space. Both walk php's scanner view ([`crate::scanner`]), the one
//! `token_get_all` reads, so there is no second tokenizer.

use crate::scanner::ScannedToken;

/// `zend_syntax_highlighter_ini`: the five colours, as the ini has them.
pub struct Colours {
    pub comment: Vec<u8>,
    pub default: Vec<u8>,
    pub html: Vec<u8>,
    pub keyword: Vec<u8>,
    pub string: Vec<u8>,
}

/// Which of the five a token is written in: `zend_highlight` compares
/// the entries' pointers, not their text, so two entries set to the same
/// colour are still two, and a change between them still closes the
/// span.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Colour {
    Comment,
    Default,
    Html,
    Keyword,
    String,
}

impl Colours {
    fn text(&self, colour: Colour) -> &[u8] {
        match colour {
            Colour::Comment => &self.comment,
            Colour::Default => &self.default,
            Colour::Html => &self.html,
            Colour::Keyword => &self.keyword,
            Colour::String => &self.string,
        }
    }
}

/// `zend_html_putc` over each byte: `<`, `>` and `&` as entities and a
/// tab as four spaces.
fn html_puts(out: &mut Vec<u8>, text: &[u8]) {
    for &c in text {
        match c {
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'&' => out.extend_from_slice(b"&amp;"),
            b'\t' => out.extend_from_slice(b"    "),
            _ => out.push(c),
        }
    }
}

/// `zend_highlight` (`Z/Zend/zend_highlight.c:83`) over the tokens of
/// `source`: `<pre><code>` in the HTML colour, each token's text in its
/// colour, a span opened where the colour changes from the HTML one and
/// closed where it changes again, whitespace written in whatever colour
/// stands.
pub fn highlight(source: &[u8], tokens: &[ScannedToken], colours: &Colours) -> Vec<u8> {
    let mut out = Vec::new();
    let mut last = Colour::Html;
    out.extend_from_slice(b"<pre><code style=\"color: ");
    out.extend_from_slice(colours.text(last));
    out.extend_from_slice(b"\">");
    for token in tokens {
        let text = source.get(token.start..token.end).unwrap_or_default();
        let next = match token.name {
            Some("T_WHITESPACE") => {
                html_puts(&mut out, text);
                continue;
            }
            Some("T_INLINE_HTML") => Colour::Html,
            Some("T_COMMENT" | "T_DOC_COMMENT") => Colour::Comment,
            Some(
                "T_OPEN_TAG"
                | "T_OPEN_TAG_WITH_ECHO"
                | "T_CLOSE_TAG"
                | "T_LINE"
                | "T_FILE"
                | "T_DIR"
                | "T_TRAIT_C"
                | "T_METHOD_C"
                | "T_FUNC_C"
                | "T_PROPERTY_C"
                | "T_NS_C"
                | "T_CLASS_C",
            ) => Colour::Default,
            Some("T_ENCAPSED_AND_WHITESPACE" | "T_CONSTANT_ENCAPSED_STRING") => Colour::String,
            None if text == b"\"" => Colour::String,
            _ if token.carries_value() => Colour::Default,
            _ => Colour::Keyword,
        };
        if last != next {
            if last != Colour::Html {
                out.extend_from_slice(b"</span>");
            }
            last = next;
            if last != Colour::Html {
                out.extend_from_slice(b"<span style=\"color: ");
                out.extend_from_slice(colours.text(last));
                out.extend_from_slice(b"\">");
            }
        }
        html_puts(&mut out, text);
    }
    if last != Colour::Html {
        out.extend_from_slice(b"</span>");
    }
    out.extend_from_slice(b"</code></pre>");
    out
}

/// `zend_strip` (`Z/Zend/zend_highlight.c:170`) over the tokens of
/// `source`: whitespace one space where none was just written, a
/// comment nothing, a heredoc's close followed by the character after
/// it where that is not whitespace and a newline, and every other token
/// as it is.
pub fn strip(source: &[u8], tokens: &[ScannedToken]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev_space = false;
    let mut at = 0;
    while let Some(token) = tokens.get(at) {
        at += 1;
        let text = source.get(token.start..token.end).unwrap_or_default();
        match token.name {
            Some("T_WHITESPACE") => {
                if !prev_space {
                    out.push(b' ');
                    prev_space = true;
                }
            }
            Some("T_COMMENT" | "T_DOC_COMMENT") => {}
            Some("T_END_HEREDOC") => {
                out.extend_from_slice(text);
                // The token after it, a newline or a `;`, read here.
                if let Some(after) = tokens.get(at) {
                    at += 1;
                    if after.name != Some("T_WHITESPACE") {
                        out.extend_from_slice(
                            source.get(after.start..after.end).unwrap_or_default(),
                        );
                    }
                }
                out.push(b'\n');
                prev_space = true;
            }
            _ => {
                out.extend_from_slice(text);
                prev_space = false;
            }
        }
    }
    out
}
