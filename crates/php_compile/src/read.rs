//! What the compiler walks while `php_syntax` is a stub (design §4.9).
//!
//! `php_syntax::Parsed` carries the source and no tree yet, and the tree
//! is inherited work of its own; so what is here reads the source into
//! the few statements the engine's loop serves today, and [`crate::emit`]
//! emits from them. Every name below mirrors the tree node that replaces
//! it, so when §4.9's copy lands this file goes and `emit` is given the
//! tree instead.
//!
//! What it reads: the open and close tags with the text around them,
//! `<?=` as the `echo` php reads it as, an `echo` of one or more
//! expressions, the empty statement, an expression statement, and inside
//! an expression a number, a single- or double-quoted string with no
//! interpolation, `true`, `false`, `null`, a constant by name, a call of
//! a named function, a parenthesised expression, unary `-` and `+`, and
//! `+`, `-`, `*` and `/` at php's two precedence levels, left to right.
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
    /// A constant named as written, php's `ZEND_AST_CONST`.
    Constant(Vec<u8>),
    /// `-$a` and `+$a`, php's `ZEND_AST_UNARY_MINUS` and
    /// `ZEND_AST_UNARY_PLUS`.
    Unary {
        minus: bool,
        operand: Box<Expr>,
    },
    /// `$a + $b`, php's `ZEND_AST_BINARY_OP`.
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Call {
        name: Vec<u8>,
        arguments: Vec<Expr>,
    },
}

/// The arithmetic operators this reads, each the opcode php's
/// `zend_compile_binary_op` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    /// `ZEND_ADD`
    Add,
    /// `ZEND_SUB`
    Sub,
    /// `ZEND_MUL`
    Mul,
    /// `ZEND_DIV`
    Div,
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
        let short = reader.open_tag();
        reader.statements(&mut out, short)?;
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
        self.source
            .get(self.at..)
            .is_some_and(|rest| rest.starts_with(text))
    }

    /// The bytes from `start` to where the reader stands.
    fn taken(&self, start: usize) -> Vec<u8> {
        self.source.get(start..self.at).unwrap_or_default().to_vec()
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
        self.taken(start)
    }

    /// Step over the open tag; `true` for `<?=`, which php reads as an
    /// `echo` of the statement that follows it.
    fn open_tag(&mut self) -> bool {
        if self.starts_with(b"<?php") {
            self.bump(5);
            return false;
        }
        if self.starts_with(b"<?=") {
            self.bump(3);
            return true;
        }
        false
    }

    /// Every statement up to the close tag or the end of the source.
    /// `short` is `<?=`, whose first statement php reads as an `echo`.
    fn statements(&mut self, out: &mut Read, short: bool) -> Result<(), Trouble> {
        let mut short = short;
        loop {
            self.skip_trivia();
            if self.at >= self.source.len() {
                return Ok(());
            }
            if self.starts_with(b"?>") {
                self.bump(2);
                // php eats one newline directly after a close tag.
                if self.starts_with(b"\r\n") {
                    self.bump(2);
                } else if self.byte(0) == Some(b'\n') {
                    self.bump(1);
                }
                return Ok(());
            }
            let line = self.line;
            let statement = if short {
                short = false;
                Some(Stmt::Echo(self.echoed()?))
            } else {
                self.statement()?
            };
            if let Some(statement) = statement {
                out.push((statement, line));
            }
        }
    }

    /// One statement, or nothing for the empty statement `;`.
    fn statement(&mut self) -> Result<Option<Stmt>, Trouble> {
        if self.byte(0) == Some(b';') {
            self.bump(1);
            return Ok(None);
        }
        if self.word_ahead(b"echo") {
            self.bump(4);
            return Ok(Some(Stmt::Echo(self.echoed()?)));
        }
        let expr = self.expr()?;
        self.end_of_statement()?;
        Ok(Some(Stmt::Effect(expr)))
    }

    /// The expressions of an `echo`, which `<?=` has too, up to the end
    /// of the statement.
    fn echoed(&mut self) -> Result<Vec<Expr>, Trouble> {
        let mut parts = vec![self.expr()?];
        loop {
            self.skip_trivia();
            if self.byte(0) == Some(b',') {
                self.bump(1);
                parts.push(self.expr()?);
                continue;
            }
            self.end_of_statement()?;
            return Ok(parts);
        }
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

    /// One expression: php's precedence, the additive level over the
    /// multiplicative one, each left to right.
    fn expr(&mut self) -> Result<Expr, Trouble> {
        self.additive()
    }

    /// `+` and `-`, php's lower arithmetic level.
    fn additive(&mut self) -> Result<Expr, Trouble> {
        let mut left = self.multiplicative()?;
        loop {
            self.skip_trivia();
            let op = match (self.byte(0), self.byte(1)) {
                (Some(b'+'), next) if !matches!(next, Some(b'+' | b'=')) => BinaryOp::Add,
                (Some(b'-'), next) if !matches!(next, Some(b'-' | b'=' | b'>')) => BinaryOp::Sub,
                _ => return Ok(left),
            };
            self.bump(1);
            let right = self.multiplicative()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    /// `*` and `/`, php's higher arithmetic level. `**` is php's own
    /// level above them and is not read, so it is refused rather than
    /// taken for a `*`.
    fn multiplicative(&mut self) -> Result<Expr, Trouble> {
        let mut left = self.unary()?;
        loop {
            self.skip_trivia();
            let op = match (self.byte(0), self.byte(1)) {
                (Some(b'*'), next) if !matches!(next, Some(b'*' | b'=')) => BinaryOp::Mul,
                // A `/` that opens a comment is trivia, already stepped over.
                (Some(b'/'), next) if next != Some(b'=') => BinaryOp::Div,
                _ => return Ok(left),
            };
            self.bump(1);
            let right = self.unary()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    /// `-$a` and `+$a`, which bind tighter than either level and nest.
    fn unary(&mut self) -> Result<Expr, Trouble> {
        self.skip_trivia();
        let minus = match (self.byte(0), self.byte(1)) {
            (Some(b'-'), next) if !matches!(next, Some(b'-' | b'=')) => true,
            (Some(b'+'), next) if !matches!(next, Some(b'+' | b'=')) => false,
            _ => return self.operand(),
        };
        self.bump(1);
        Ok(Expr::Unary {
            minus,
            operand: Box::new(self.unary()?),
        })
    }

    /// What an operator stands between: a number or a string as written, a name, or a
    /// parenthesised expression.
    fn operand(&mut self) -> Result<Expr, Trouble> {
        self.skip_trivia();
        match self.byte(0) {
            Some(b'\'') => self.single_quoted(),
            Some(b'"') => self.double_quoted(),
            Some(b) if b.is_ascii_digit() || (b == b'.' && self.digit_ahead(1)) => self.number(),
            Some(b'(') => {
                self.bump(1);
                let inner = self.expr()?;
                self.skip_trivia();
                if self.byte(0) != Some(b')') {
                    return Err(self.trouble("a parenthesis no close parenthesis matches"));
                }
                self.bump(1);
                Ok(inner)
            }
            Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b'\\' => self.named(),
            _ => Err(self.trouble("an expression this compiler cannot read yet")),
        }
    }

    /// A name: `true`, `false` and `null`, which php reads whatever their
    /// case, another constant by name, or a call.
    fn named(&mut self) -> Result<Expr, Trouble> {
        let name = self.name();
        self.skip_trivia();
        if self.byte(0) != Some(b'(') {
            return match name.to_ascii_lowercase().as_slice() {
                b"true" => Ok(Expr::Bool(true)),
                b"false" => Ok(Expr::Bool(false)),
                b"null" => Ok(Expr::Null),
                _ => Ok(Expr::Constant(name)),
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
        self.taken(start)
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
        // php's `EXPONENT_DNUM`: an exponent makes the number a float
        // however its mantissa was written.
        if matches!(self.byte(0), Some(b'e' | b'E'))
            && (self.digit_ahead(1)
                || (matches!(self.byte(1), Some(b'+' | b'-')) && self.digit_ahead(2)))
        {
            float = true;
            self.bump(1);
            if matches!(self.byte(0), Some(b'+' | b'-')) {
                self.bump(1);
            }
            while self
                .byte(0)
                .is_some_and(|b| b.is_ascii_digit() || b == b'_')
            {
                self.bump(1);
            }
        }
        let digits: String = self
            .taken(start)
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
                        // `\x41`, `\101` and `\u{1F600}` are php's other
                        // escapes; refused rather than read as the
                        // backslash php does not leave there.
                        Some(b'x' | b'u' | b'0'..=b'7') => {
                            return Err(
                                self.trouble("a string escape this compiler cannot read yet")
                            );
                        }
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
    fn arithmetic_is_read_at_php_s_two_precedence_levels() {
        let read = read(b"<?php echo 1 + 2 * 3;").unwrap_or_default();
        assert_eq!(
            read,
            vec![(
                Stmt::Echo(vec![Expr::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(Expr::Int(1)),
                    right: Box::new(Expr::Binary {
                        op: BinaryOp::Mul,
                        left: Box::new(Expr::Int(2)),
                        right: Box::new(Expr::Int(3)),
                    }),
                }]),
                1
            )],
            "`*` binds tighter than `+`"
        );
    }

    #[test]
    fn a_level_reads_left_to_right_and_a_parenthesis_regroups() {
        let left_to_right = read(b"<?php echo 1 - 2 - 3;").unwrap_or_default();
        assert_eq!(
            left_to_right,
            vec![(
                Stmt::Echo(vec![Expr::Binary {
                    op: BinaryOp::Sub,
                    left: Box::new(Expr::Binary {
                        op: BinaryOp::Sub,
                        left: Box::new(Expr::Int(1)),
                        right: Box::new(Expr::Int(2)),
                    }),
                    right: Box::new(Expr::Int(3)),
                }]),
                1
            )]
        );
        let grouped = read(b"<?php echo (1 + 2) * 3;").unwrap_or_default();
        assert_eq!(
            grouped,
            vec![(
                Stmt::Echo(vec![Expr::Binary {
                    op: BinaryOp::Mul,
                    left: Box::new(Expr::Binary {
                        op: BinaryOp::Add,
                        left: Box::new(Expr::Int(1)),
                        right: Box::new(Expr::Int(2)),
                    }),
                    right: Box::new(Expr::Int(3)),
                }]),
                1
            )]
        );
    }

    #[test]
    fn a_minus_is_the_unary_operator_php_reads_it_as() {
        let read = read(b"<?php echo -2.5, -INF;").unwrap_or_default();
        assert_eq!(
            read,
            vec![(
                Stmt::Echo(vec![
                    Expr::Unary {
                        minus: true,
                        operand: Box::new(Expr::Float(2.5)),
                    },
                    Expr::Unary {
                        minus: true,
                        operand: Box::new(Expr::Constant(b"INF".to_vec())),
                    },
                ]),
                1
            )],
            "php's scanner has no negative literal; the operator is the tree node"
        );
    }

    #[test]
    fn a_name_that_opens_no_call_is_a_constant() {
        let read = read(b"<?php echo NAN;").unwrap_or_default();
        assert_eq!(
            read,
            vec![(Stmt::Echo(vec![Expr::Constant(b"NAN".to_vec())]), 1)]
        );
    }

    #[test]
    fn an_exponent_makes_a_number_a_float_however_it_is_written() {
        let read = read(b"<?php echo 1e25, 5e-324, 1.5E+3;").unwrap_or_default();
        assert_eq!(
            read,
            vec![(
                Stmt::Echo(vec![
                    Expr::Float(1e25),
                    Expr::Float(5e-324),
                    Expr::Float(1.5e3)
                ]),
                1
            )]
        );
    }

    #[test]
    fn an_operator_this_does_not_read_is_refused_rather_than_taken_for_another() {
        for source in [
            b"<?php echo 2 ** 3;".as_slice(),
            b"<?php echo 7 % 2;".as_slice(),
            b"<?php echo 'a' . 'b';".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    #[test]
    fn what_it_cannot_read_is_refused_at_its_line() {
        let trouble = read(b"<?php\n$x = 1;").err();
        assert_eq!(trouble.map(|t| t.line), Some(2));
    }

    #[test]
    fn a_short_echo_tag_echoes_what_follows_it() {
        let read = read(b"<?= 1, 2 ?>after").unwrap_or_default();
        assert_eq!(
            read,
            vec![
                (Stmt::Echo(vec![Expr::Int(1), Expr::Int(2)]), 1),
                (Stmt::Inline(b"after".to_vec()), 1),
            ]
        );
    }

    #[test]
    fn the_empty_statement_is_no_statement_at_all() {
        assert_eq!(read(b"<?php ;;").unwrap_or_default(), Vec::new());
    }

    #[test]
    fn a_string_escape_it_cannot_read_is_refused_rather_than_mis_read() {
        for source in [
            b"<?php echo \"\\x41\";".as_slice(),
            b"<?php echo \"\\101\";".as_slice(),
            b"<?php echo \"\\u{1F600}\";".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    #[test]
    fn a_close_tag_eats_one_newline_however_it_is_written() {
        assert_eq!(
            read(b"<?php echo 1; ?>\r\nafter").unwrap_or_default().len(),
            2,
            "the echo and the text after it, with the newline eaten"
        );
        assert_eq!(
            read(b"<?php echo 1; ?>\r\nafter")
                .unwrap_or_default()
                .last()
                .cloned(),
            Some((Stmt::Inline(b"after".to_vec()), 2))
        );
    }
}
