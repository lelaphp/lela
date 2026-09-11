//! What the compiler walks while `php_syntax` is a stub (design §4.9).
//!
//! `php_syntax::Parsed` carries the source and no tree yet, and the tree
//! is inherited work of its own; so what is here reads the source into
//! the few statements the engine's loop serves today, and [`crate::emit`]
//! emits from them. Every name below mirrors the tree node that replaces
//! it, so when §4.9's copy lands this file goes and `emit` is given the
//! tree instead.
//!
//! What it reads: the open and close tags with the text around them, an
//! `echo` of one or more expressions, an expression statement, and inside
//! an expression a number, a single- or double-quoted string with no
//! interpolation, `true`, `false`, `null`, and a call of a named function.
//! Anything else is a [`crate::CompileError`], so what this cannot read is
//! refused rather than silently compiled to something else.

/// One statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stmt {
    /// Text outside the tags, which php echoes as it stands.
    Inline(Vec<u8>),
    /// `echo $a, $b;`
    Echo(Vec<Expr>),
    /// An expression served for its effect; its value is discarded.
    Effect(Expr),
}

/// One expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Call { name: Vec<u8>, arguments: Vec<Expr> },
}

/// The line each statement stands on, beside it.
pub(crate) type Read = Vec<(Stmt, u32)>;

/// A place in the source, and php's line number for it.
struct Reader<'a> {
    source: &'a [u8],
    at: usize,
    line: u32,
}

/// What one source file holds, or the first thing in it this cannot read.
pub(crate) fn read(source: &[u8]) -> Result<Read, Trouble> {
    let mut reader = Reader {
        source,
        at: 0,
        line: 1,
    };
    let mut out = Read::new();
    loop {
        let line = reader.line;
        let text = reader.inline_text();
        if !text.is_empty() {
            out.push((Stmt::Inline(text), line));
        }
        if reader.at >= reader.source.len() {
            return Ok(out);
        }
        reader.open_tag();
        reader.statements(&mut out)?;
    }
}

/// What the reader could not read: php's line, and what stood there.
pub(crate) struct Trouble {
    pub(crate) what: String,
    pub(crate) line: u32,
}

impl Reader<'_> {
    fn byte(&self, ahead: usize) -> Option<u8> {
        self.source.get(self.at + ahead).copied()
    }

    fn starts_with(&self, text: &[u8]) -> bool {
        self.source[self.at..].starts_with(text)
    }

    fn bump(&mut self, by: usize) {
        for _ in 0..by {
            if self.byte(0) == Some(b'\n') {
                self.line += 1;
            }
            self.at += 1;
        }
    }

    fn trouble(&self, what: impl Into<String>) -> Trouble {
        Trouble {
            what: what.into(),
            line: self.line,
        }
    }

    /// The text up to the next open tag, or to the end.
    fn inline_text(&mut self) -> Vec<u8> {
        let start = self.at;
        while self.at < self.source.len()
            && !self.starts_with(b"<?php")
            && !self.starts_with(b"<?=")
        {
            self.bump(1);
        }
        self.source[start..self.at].to_vec()
    }

    fn open_tag(&mut self) {
        if self.starts_with(b"<?php") {
            self.bump(5);
        } else if self.starts_with(b"<?=") {
            self.bump(3);
        }
    }

    /// Every statement up to the close tag or the end of the source.
    fn statements(&mut self, out: &mut Read) -> Result<(), Trouble> {
        loop {
            self.skip_trivia();
            if self.at >= self.source.len() {
                return Ok(());
            }
            if self.starts_with(b"?>") {
                self.bump(2);
                // php eats one newline directly after a close tag.
                if self.byte(0) == Some(b'\n') {
                    self.bump(1);
                }
                return Ok(());
            }
            let line = self.line;
            let statement = self.statement()?;
            out.push((statement, line));
        }
    }

    fn statement(&mut self) -> Result<Stmt, Trouble> {
        if self.byte(0) == Some(b';') {
            self.bump(1);
            return Ok(Stmt::Echo(Vec::new()));
        }
        if self.word_ahead(b"echo") {
            self.bump(4);
            let mut parts = vec![self.expr()?];
            loop {
                self.skip_trivia();
                if self.byte(0) == Some(b',') {
                    self.bump(1);
                    parts.push(self.expr()?);
                    continue;
                }
                self.end_of_statement()?;
                return Ok(Stmt::Echo(parts));
            }
        }
        let expr = self.expr()?;
        self.end_of_statement()?;
        Ok(Stmt::Effect(expr))
    }

    /// A `;`, or a close tag, which php accepts in its place.
    fn end_of_statement(&mut self) -> Result<(), Trouble> {
        self.skip_trivia();
        if self.byte(0) == Some(b';') {
            self.bump(1);
            return Ok(());
        }
        if self.starts_with(b"?>") || self.at >= self.source.len() {
            return Ok(());
        }
        Err(self.trouble("a statement this compiler cannot read yet"))
    }

    fn expr(&mut self) -> Result<Expr, Trouble> {
        self.skip_trivia();
        match self.byte(0) {
            Some(b'\'') => self.single_quoted(),
            Some(b'"') => self.double_quoted(),
            Some(b) if b.is_ascii_digit() || (b == b'.' && self.digit_ahead(1)) => self.number(),
            Some(b'-') if self.digit_ahead(1) => {
                self.bump(1);
                match self.number()? {
                    Expr::Int(n) => Ok(Expr::Int(n.wrapping_neg())),
                    Expr::Float(f) => Ok(Expr::Float(-f)),
                    other => Ok(other),
                }
            }
            Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b'\\' => self.named(),
            _ => Err(self.trouble("an expression this compiler cannot read yet")),
        }
    }

    fn named(&mut self) -> Result<Expr, Trouble> {
        let name = self.name();
        self.skip_trivia();
        if self.byte(0) != Some(b'(') {
            return match name.to_ascii_lowercase().as_slice() {
                b"true" => Ok(Expr::Bool(true)),
                b"false" => Ok(Expr::Bool(false)),
                b"null" => Ok(Expr::Null),
                _ => Err(self.trouble("a constant this compiler cannot read yet")),
            };
        }
        self.bump(1);
        let mut arguments = Vec::new();
        loop {
            self.skip_trivia();
            if self.byte(0) == Some(b')') {
                self.bump(1);
                return Ok(Expr::Call { name, arguments });
            }
            if !arguments.is_empty() {
                if self.byte(0) != Some(b',') {
                    return Err(self.trouble("a call whose arguments this compiler cannot read"));
                }
                self.bump(1);
            }
            arguments.push(self.expr()?);
        }
    }

    /// A name, namespace separators and all, as written.
    fn name(&mut self) -> Vec<u8> {
        let start = self.at;
        while self
            .byte(0)
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'\\')
        {
            self.bump(1);
        }
        self.source[start..self.at].to_vec()
    }

    fn digit_ahead(&self, ahead: usize) -> bool {
        self.byte(ahead).is_some_and(|b| b.is_ascii_digit())
    }

    /// Whether `word` stands here and a name does not go on past it.
    fn word_ahead(&self, word: &[u8]) -> bool {
        let same = self.source[self.at..]
            .iter()
            .zip(word)
            .filter(|(a, b)| a.eq_ignore_ascii_case(b))
            .count();
        same == word.len()
            && !self
                .byte(word.len())
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
    }

    fn number(&mut self) -> Result<Expr, Trouble> {
        let start = self.at;
        let mut float = false;
        while let Some(b) = self.byte(0) {
            if b.is_ascii_digit() || b == b'_' {
                self.bump(1);
            } else if b == b'.' && !float && self.digit_ahead(1) {
                float = true;
                self.bump(1);
            } else {
                break;
            }
        }
        let digits: String = self.source[start..self.at]
            .iter()
            .filter(|b| **b != b'_')
            .map(|b| char::from(*b))
            .collect();
        if float {
            return match digits.parse() {
                Ok(f) => Ok(Expr::Float(f)),
                Err(_) => Err(self.trouble("a number this compiler cannot read")),
            };
        }
        // php's integer literal that does not fit becomes a float.
        match digits.parse::<i64>() {
            Ok(n) => Ok(Expr::Int(n)),
            Err(_) => match digits.parse::<f64>() {
                Ok(f) => Ok(Expr::Float(f)),
                Err(_) => Err(self.trouble("a number this compiler cannot read")),
            },
        }
    }

    /// `'...'`: a backslash escapes a backslash and a quote, nothing else.
    fn single_quoted(&mut self) -> Result<Expr, Trouble> {
        self.bump(1);
        let mut bytes = Vec::new();
        loop {
            match self.byte(0) {
                None => return Err(self.trouble("a string no quote closes")),
                Some(b'\'') => {
                    self.bump(1);
                    return Ok(Expr::Str(bytes));
                }
                Some(b'\\') if matches!(self.byte(1), Some(b'\\' | b'\'')) => {
                    bytes.extend(self.byte(1));
                    self.bump(2);
                }
                Some(b) => {
                    bytes.push(b);
                    self.bump(1);
                }
            }
        }
    }

    /// `"..."` with no interpolation: the escapes php's scanner takes.
    fn double_quoted(&mut self) -> Result<Expr, Trouble> {
        self.bump(1);
        let mut bytes = Vec::new();
        loop {
            match self.byte(0) {
                None => return Err(self.trouble("a string no quote closes")),
                Some(b'"') => {
                    self.bump(1);
                    return Ok(Expr::Str(bytes));
                }
                Some(b'$' | b'{') => {
                    return Err(self.trouble("an interpolated string this compiler cannot read"));
                }
                Some(b'\\') => {
                    let escaped = match self.byte(1) {
                        Some(b'n') => b'\n',
                        Some(b't') => b'\t',
                        Some(b'r') => b'\r',
                        Some(b'v') => 0x0b,
                        Some(b'f') => 0x0c,
                        Some(b'e') => 0x1b,
                        Some(b @ (b'\\' | b'"' | b'$')) => b,
                        _ => {
                            bytes.push(b'\\');
                            self.bump(1);
                            continue;
                        }
                    };
                    bytes.push(escaped);
                    self.bump(2);
                }
                Some(b) => {
                    bytes.push(b);
                    self.bump(1);
                }
            }
        }
    }

    /// Whitespace and the three comment forms.
    fn skip_trivia(&mut self) {
        loop {
            match self.byte(0) {
                Some(b) if b.is_ascii_whitespace() => self.bump(1),
                Some(b'/') if self.byte(1) == Some(b'/') => self.line_comment(),
                Some(b'#') if self.byte(1) != Some(b'[') => self.line_comment(),
                Some(b'/') if self.byte(1) == Some(b'*') => {
                    self.bump(2);
                    while self.at < self.source.len() && !self.starts_with(b"*/") {
                        self.bump(1);
                    }
                    self.bump(2.min(self.source.len() - self.at));
                }
                _ => return,
            }
        }
    }

    /// A line comment, which a close tag ends as php's scanner has it.
    fn line_comment(&mut self) {
        while self.at < self.source.len() && self.byte(0) != Some(b'\n') && !self.starts_with(b"?>")
        {
            self.bump(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_echo_of_one_number_is_one_statement() {
        let read = read(b"<?php echo 1;").unwrap_or_default();
        assert_eq!(read, vec![(Stmt::Echo(vec![Expr::Int(1)]), 1)]);
    }

    #[test]
    fn a_nested_call_keeps_its_arguments() {
        let read = read(b"<?php var_dump(extension_loaded('demo'));").unwrap_or_default();
        assert_eq!(
            read,
            vec![(
                Stmt::Effect(Expr::Call {
                    name: b"var_dump".to_vec(),
                    arguments: vec![Expr::Call {
                        name: b"extension_loaded".to_vec(),
                        arguments: vec![Expr::Str(b"demo".to_vec())],
                    }],
                }),
                1
            )]
        );
    }

    #[test]
    fn text_around_the_tags_is_inline_and_lines_are_counted() {
        let read = read(b"a\n<?php\necho \"b\\n\", 2.5, true, null; ?>c").unwrap_or_default();
        assert_eq!(read[0], (Stmt::Inline(b"a\n".to_vec()), 1));
        assert_eq!(
            read[1],
            (
                Stmt::Echo(vec![
                    Expr::Str(b"b\n".to_vec()),
                    Expr::Float(2.5),
                    Expr::Bool(true),
                    Expr::Null,
                ]),
                3
            )
        );
        assert_eq!(read[2], (Stmt::Inline(b"c".to_vec()), 3));
    }

    #[test]
    fn what_it_cannot_read_is_refused_at_its_line() {
        let trouble = read(b"<?php\n$x = 1;").err();
        assert_eq!(trouble.map(|t| t.line), Some(2));
    }
}
