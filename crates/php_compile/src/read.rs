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
//! expressions, the empty statement, an expression statement, a block,
//! a `for` and a `while` whose body is a block or one statement, and
//! inside an expression a number, a single- or double-quoted string with no
//! interpolation, `true`, `false`, `null`, a constant by name, a call of
//! a named function, a parenthesised expression, a variable `$a` that is
//! a CV, an assignment `$a = expr` to one, `$a++` and `++$a` on one,
//! unary `-` and `+`, and `<`, `.`, `<<`, `>>`, `+`, `-`, `*`, `/`, `%`
//! and `**` at php's precedence levels: `<` under `.` under the shifts
//! under the additive level under the multiplicative one, `<` not
//! associating and the rest left to right, and `**` right to left above
//! unary `-`, with `=` below them all and read right to left.
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
    /// `{ ... }`, php's `ZEND_AST_STMT_LIST`.
    Block(Read),
    /// `for (init; cond; step) body`, php's `ZEND_AST_FOR`: three
    /// expression lists, each empty where nothing is written between the
    /// semicolons, and the body's statements.
    For {
        init: Vec<Expr>,
        cond: Vec<Expr>,
        step: Vec<Expr>,
        body: Read,
    },
    /// `while (cond) body`, php's `ZEND_AST_WHILE`.
    While { cond: Expr, body: Read },
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
    /// `$a`, php's `ZEND_AST_VAR` over a name written as is, which
    /// `zend_try_compile_cv` makes a CV: not `$this` and not `$_GET` or
    /// its kin, which php fetches by other oplines and this refuses.
    Var(Vec<u8>),
    /// `$a = expr`, php's `ZEND_AST_ASSIGN` to a variable.
    Assign {
        var: Vec<u8>,
        value: Box<Expr>,
    },
    /// `++$a`, php's `ZEND_AST_PRE_INC`, whose grammar puts a variable
    /// and nothing else after the `++`.
    PreInc(Vec<u8>),
    /// `$a++`, php's `ZEND_AST_POST_INC`.
    PostInc(Vec<u8>),
}

/// The binary operators this reads, each the opcode php's
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
    /// `ZEND_MOD`
    Mod,
    /// `ZEND_POW`
    Pow,
    /// `ZEND_SL`
    Sl,
    /// `ZEND_SR`
    Sr,
    /// `ZEND_CONCAT`
    Concat,
    /// `ZEND_IS_SMALLER`
    IsSmaller,
    /// `ZEND_IS_SMALLER_OR_EQUAL`
    IsSmallerOrEqual,
    /// `>`, php's `ZEND_AST_GREATER`, which `zend_compile_greater` emits
    /// as `ZEND_IS_SMALLER` with the operands swapped.
    Greater,
    /// `>=`, php's `ZEND_AST_GREATER_EQUAL`, `ZEND_IS_SMALLER_OR_EQUAL`
    /// the same way.
    GreaterOrEqual,
    /// `ZEND_IS_EQUAL`
    IsEqual,
    /// `ZEND_IS_NOT_EQUAL`, which `<>` is as well.
    IsNotEqual,
    /// `ZEND_IS_IDENTICAL`
    IsIdentical,
    /// `ZEND_IS_NOT_IDENTICAL`
    IsNotIdentical,
    /// `ZEND_SPACESHIP`
    Spaceship,
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
        reader.statements(&mut out, short, false)?;
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

    /// Every statement up to the close tag or the end of the source, or
    /// inside a block up to its close brace. `short` is `<?=`, whose
    /// first statement php reads as an `echo`.
    fn statements(&mut self, out: &mut Read, short: bool, in_block: bool) -> Result<(), Trouble> {
        let mut short = short;
        loop {
            self.skip_trivia();
            if self.at >= self.source.len() {
                if in_block {
                    return Err(self.trouble("a block no brace closes"));
                }
                return Ok(());
            }
            if in_block && self.byte(0) == Some(b'}') {
                self.bump(1);
                return Ok(());
            }
            if self.starts_with(b"?>") {
                // php reads text between the tags inside a block as an
                // `echo`; this does not yet.
                if in_block {
                    return Err(
                        self.trouble("a close tag inside a block this compiler cannot read yet")
                    );
                }
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
        if self.byte(0) == Some(b'{') {
            self.bump(1);
            let mut body = Read::new();
            self.statements(&mut body, false, true)?;
            return Ok(Some(Stmt::Block(body)));
        }
        if self.word_ahead(b"echo") {
            self.bump(4);
            return Ok(Some(Stmt::Echo(self.echoed()?)));
        }
        if self.word_ahead(b"for") {
            self.bump(3);
            return self.r#for().map(Some);
        }
        if self.word_ahead(b"while") {
            self.bump(5);
            return self.r#while().map(Some);
        }
        let expr = self.expr()?;
        self.end_of_statement()?;
        Ok(Some(Stmt::Effect(expr)))
    }

    /// `for (init; cond; step) body`, the keyword read: three expression
    /// lists between the parentheses, then the body.
    fn r#for(&mut self) -> Result<Stmt, Trouble> {
        self.punctuation(b'(', "a `for` with no parenthesis after it")?;
        let init = self.expr_list(b';')?;
        let cond = self.expr_list(b';')?;
        let step = self.expr_list(b')')?;
        let body = self.body()?;
        Ok(Stmt::For {
            init,
            cond,
            step,
            body,
        })
    }

    /// `while (cond) body`, the keyword read.
    fn r#while(&mut self) -> Result<Stmt, Trouble> {
        self.punctuation(b'(', "a `while` with no parenthesis after it")?;
        let cond = self.expr()?;
        self.punctuation(b')', "a `while` whose parenthesis nothing closes")?;
        let body = self.body()?;
        Ok(Stmt::While { cond, body })
    }

    /// One expected byte, or the trouble that names what was wanted.
    fn punctuation(&mut self, wanted: u8, what: &str) -> Result<(), Trouble> {
        self.skip_trivia();
        if self.byte(0) != Some(wanted) {
            return Err(self.trouble(what));
        }
        self.bump(1);
        Ok(())
    }

    /// php's `for_exprs`: nothing, or expressions separated by commas,
    /// up to `close`, which is taken.
    fn expr_list(&mut self, close: u8) -> Result<Vec<Expr>, Trouble> {
        let mut exprs = Vec::new();
        loop {
            self.skip_trivia();
            if self.byte(0) == Some(close) {
                self.bump(1);
                return Ok(exprs);
            }
            if !exprs.is_empty() {
                if self.byte(0) != Some(b',') {
                    return Err(self.trouble("a `for` whose expressions this compiler cannot read"));
                }
                self.bump(1);
            }
            exprs.push(self.expr()?);
        }
    }

    /// A loop's body: the statements of a block, one statement, or none
    /// for the empty statement, each with its line.
    fn body(&mut self) -> Result<Read, Trouble> {
        self.skip_trivia();
        let mut body = Read::new();
        if self.byte(0) == Some(b'{') {
            self.bump(1);
            self.statements(&mut body, false, true)?;
            return Ok(body);
        }
        let line = self.line;
        if let Some(statement) = self.statement()? {
            body.push((statement, line));
        }
        Ok(body)
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

    /// One expression: an assignment, which php's grammar puts below
    /// every operator here and reads right to left, `$a = $b = 1` being
    /// `$a = ($b = 1)`; else php's precedence, one level per function
    /// below, each left to right. php's rule is `variable '=' expr`, so
    /// `1 + $a = 2` is php's `1 + ($a = 2)`; that is refused here rather
    /// than read as the other grouping.
    fn expr(&mut self) -> Result<Expr, Trouble> {
        let left = self.equality()?;
        self.skip_trivia();
        if !(self.byte(0) == Some(b'=') && self.byte(1) != Some(b'=')) {
            return Ok(left);
        }
        let Expr::Var(var) = left else {
            return Err(
                self.trouble("an assignment to what is no variable this compiler cannot read yet")
            );
        };
        self.bump(1);
        Ok(Expr::Assign {
            var,
            value: Box::new(self.expr()?),
        })
    }

    /// `==`, `!=`, `<>`, `===`, `!==` and `<=>`, php's non-associative
    /// level below the comparisons: one equality, and a second operator
    /// of the level after it is php's parse error, refused here.
    fn equality(&mut self) -> Result<Expr, Trouble> {
        let left = self.comparison()?;
        let Some((op, width)) = self.equality_ahead() else {
            return Ok(left);
        };
        self.bump(width);
        let right = self.comparison()?;
        if self.equality_ahead().is_some() {
            return Err(self.trouble("an equality of an equality, which php does not read"));
        }
        Ok(Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    /// The operator of the equality level standing here, and its width
    /// in bytes: the scanner takes the longest, so `!==` before `!=`
    /// and `<=>` before `<=`. A `=` alone is the assignment.
    fn equality_ahead(&mut self) -> Option<(BinaryOp, usize)> {
        self.skip_trivia();
        Some(match (self.byte(0), self.byte(1), self.byte(2)) {
            (Some(b'='), Some(b'='), Some(b'=')) => (BinaryOp::IsIdentical, 3),
            (Some(b'='), Some(b'='), _) => (BinaryOp::IsEqual, 2),
            (Some(b'!'), Some(b'='), Some(b'=')) => (BinaryOp::IsNotIdentical, 3),
            (Some(b'<'), Some(b'='), Some(b'>')) => (BinaryOp::Spaceship, 3),
            (Some(b'!'), Some(b'='), _) | (Some(b'<'), Some(b'>'), _) => (BinaryOp::IsNotEqual, 2),
            _ => return None,
        })
    }

    /// `<`, `<=`, `>` and `>=`, php's non-associative level below `.`:
    /// one comparison, and a second operator of the level after it is
    /// php's parse error, refused here. `<<` and `>>` are the shifts,
    /// which read `<<=` and `>>=` as theirs to refuse; `<>` and `<=>`
    /// are the level below.
    fn comparison(&mut self) -> Result<Expr, Trouble> {
        let left = self.concat()?;
        let Some((op, width)) = self.comparison_ahead() else {
            return Ok(left);
        };
        self.bump(width);
        let right = self.concat()?;
        if self.comparison_ahead().is_some() {
            return Err(self.trouble("a comparison of a comparison, which php does not read"));
        }
        Ok(Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    /// The operator of the comparison level standing here, and its
    /// width in bytes; nothing for a shift, an equality or a `<=>`.
    fn comparison_ahead(&mut self) -> Option<(BinaryOp, usize)> {
        self.skip_trivia();
        Some(match (self.byte(0), self.byte(1), self.byte(2)) {
            (Some(b'<'), Some(b'='), Some(b'>')) | (Some(b'<'), Some(b'<' | b'>'), _) => {
                return None;
            }
            (Some(b'<'), Some(b'='), _) => (BinaryOp::IsSmallerOrEqual, 2),
            (Some(b'<'), _, _) => (BinaryOp::IsSmaller, 1),
            (Some(b'>'), Some(b'>'), _) => return None,
            (Some(b'>'), Some(b'='), _) => (BinaryOp::GreaterOrEqual, 2),
            (Some(b'>'), _, _) => (BinaryOp::Greater, 1),
            _ => return None,
        })
    }

    /// `.`, php's own level since 8.0, below the shifts. A `.=` assigns
    /// and a `...` unpacks, neither of which this reads; a `.` before a
    /// digit opens a number, which php's scanner takes first, so `'a' .5`
    /// is php's parse error and is refused here.
    fn concat(&mut self) -> Result<Expr, Trouble> {
        let mut left = self.shift()?;
        loop {
            self.skip_trivia();
            if !(self.byte(0) == Some(b'.')
                && !matches!(self.byte(1), Some(b'.' | b'='))
                && !self.digit_ahead(1))
            {
                return Ok(left);
            }
            self.bump(1);
            let right = self.shift()?;
            left = Expr::Binary {
                op: BinaryOp::Concat,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    /// `<<` and `>>`, php's level below the arithmetic. A `<<<` opens a
    /// heredoc and a `<<=` or `>>=` assigns, none of which this reads.
    fn shift(&mut self) -> Result<Expr, Trouble> {
        let mut left = self.additive()?;
        loop {
            self.skip_trivia();
            let op = match (self.byte(0), self.byte(1), self.byte(2)) {
                (Some(b'<'), Some(b'<'), next) if !matches!(next, Some(b'<' | b'=')) => {
                    BinaryOp::Sl
                }
                (Some(b'>'), Some(b'>'), next) if next != Some(b'=') => BinaryOp::Sr,
                _ => return Ok(left),
            };
            self.bump(2);
            let right = self.additive()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
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

    /// `*`, `/` and `%`, php's higher arithmetic level.
    fn multiplicative(&mut self) -> Result<Expr, Trouble> {
        let mut left = self.unary()?;
        loop {
            self.skip_trivia();
            let op = match (self.byte(0), self.byte(1)) {
                (Some(b'*'), next) if !matches!(next, Some(b'*' | b'=')) => BinaryOp::Mul,
                // A `/` that opens a comment is trivia, already stepped over.
                (Some(b'/'), next) if next != Some(b'=') => BinaryOp::Div,
                (Some(b'%'), next) if next != Some(b'=') => BinaryOp::Mod,
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
    /// `**` binds tighter still, so `-2 ** 2` is php's `-(2 ** 2)`.
    fn unary(&mut self) -> Result<Expr, Trouble> {
        self.skip_trivia();
        let minus = match (self.byte(0), self.byte(1)) {
            (Some(b'-'), next) if !matches!(next, Some(b'-' | b'=')) => true,
            (Some(b'+'), next) if !matches!(next, Some(b'+' | b'=')) => false,
            _ => return self.power(),
        };
        self.bump(1);
        Ok(Expr::Unary {
            minus,
            operand: Box::new(self.unary()?),
        })
    }

    /// `**`, php's own level above unary `-`, read right to left: its
    /// right side is a unary expression, so `2 ** -1` and `2 ** 3 ** 2`
    /// are php's.
    fn power(&mut self) -> Result<Expr, Trouble> {
        let left = self.operand()?;
        self.skip_trivia();
        if !(self.byte(0) == Some(b'*') && self.byte(1) == Some(b'*') && self.byte(2) != Some(b'='))
        {
            return Ok(left);
        }
        self.bump(2);
        Ok(Expr::Binary {
            op: BinaryOp::Pow,
            left: Box::new(left),
            right: Box::new(self.unary()?),
        })
    }

    /// What an operator stands between: a number or a string as written, a name, a
    /// variable, with `++` before or after it, or a parenthesised expression.
    fn operand(&mut self) -> Result<Expr, Trouble> {
        self.skip_trivia();
        match self.byte(0) {
            Some(b'\'') => self.single_quoted(),
            Some(b'"') => self.double_quoted(),
            Some(b'$') => self.incremented(),
            // php's `T_INC variable`: the variable is the whole operand,
            // so `++$a ** 2` is php's `(++$a) ** 2`.
            Some(b'+') if self.byte(1) == Some(b'+') => {
                self.bump(2);
                self.skip_trivia();
                if self.byte(0) != Some(b'$') {
                    return Err(self.trouble("a `++` before what is no variable"));
                }
                let Expr::Var(name) = self.variable()? else {
                    return Err(self.trouble("a `++` before what is no variable"));
                };
                Ok(Expr::PreInc(name))
            }
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

    /// A variable, and php's `variable T_INC` after it: `$a++` is one
    /// operand, so `-$a++` is php's `-($a++)`. A `++` after that is
    /// php's parse error, and is refused by whatever reads on.
    fn incremented(&mut self) -> Result<Expr, Trouble> {
        let var = self.variable()?;
        self.skip_trivia();
        if !(self.byte(0) == Some(b'+') && self.byte(1) == Some(b'+')) {
            return Ok(var);
        }
        let Expr::Var(name) = var else {
            return Ok(var);
        };
        self.bump(2);
        Ok(Expr::PostInc(name))
    }

    /// `$` and php's `LABEL`: a letter, `_` or a byte past ASCII, then
    /// those and digits. What `zend_try_compile_cv` refuses is refused
    /// here too: `$this`, which php fetches by `FETCH_THIS`, and `$_GET`
    /// and its kin, which it fetches by `FETCH_R`; and `$$a` and `${`,
    /// which name a variable at runtime.
    fn variable(&mut self) -> Result<Expr, Trouble> {
        let start = self.at + 1;
        if !self
            .byte(1)
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b >= 0x80)
        {
            return Err(self.trouble("a variable this compiler cannot read yet"));
        }
        self.bump(2);
        while self
            .byte(0)
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
        {
            self.bump(1);
        }
        let name = self.taken(start);
        if name == b"this" || is_auto_global(&name) {
            return Err(self.trouble("a variable this compiler cannot read yet"));
        }
        Ok(Expr::Var(name))
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
        // php's `DNUM` takes a point with digits on either side of it,
        // so `1.` is the float 1.0 and `1.'a'` is a parse error, not `1 . 'a'`.
        while let Some(b) = self.byte(0) {
            if b.is_ascii_digit() || b == b'_' {
                self.bump(1);
            } else if b == b'.' && !float && (self.at > start || self.digit_ahead(1)) {
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

/// `zend_is_auto_global`: the names `php_startup_auto_globals` and
/// `zend_startup` register, which no CV holds.
fn is_auto_global(name: &[u8]) -> bool {
    matches!(
        name,
        b"GLOBALS"
            | b"_GET"
            | b"_POST"
            | b"_COOKIE"
            | b"_SERVER"
            | b"_ENV"
            | b"_REQUEST"
            | b"_FILES"
    )
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
            b"<?php echo 'a' .= 'b';".as_slice(),
            b"<?php echo 1 ?? 2;".as_slice(),
            b"<?php echo 1 | 2;".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    /// The one expression each level of php's table produces, so that a
    /// level that moved is a failing test and not a wrong answer.
    fn shape(source: &[u8]) -> String {
        format!("{:?}", read(source).unwrap_or_default())
            .replace("Box::new", "")
            .replace(' ', "")
    }

    #[test]
    fn a_modulo_stands_at_php_s_multiplicative_level() {
        assert_eq!(
            shape(b"<?php echo 1 + 2 % 3;"),
            shape(b"<?php echo 1 + (2 % 3);"),
            "`%` binds tighter than `+`"
        );
        assert_eq!(
            shape(b"<?php echo 2 * 3 % 4;"),
            shape(b"<?php echo (2 * 3) % 4;"),
            "`%` and `*` are one level, read left to right"
        );
    }

    #[test]
    fn a_concatenation_stands_below_the_shifts_and_reads_left_to_right() {
        assert_eq!(
            shape(b"<?php echo 1 . 2 + 3;"),
            shape(b"<?php echo 1 . (2 + 3);"),
            "`+` binds tighter than `.`"
        );
        assert_eq!(
            shape(b"<?php echo 1 + 2 . 3;"),
            shape(b"<?php echo (1 + 2) . 3;"),
            "`+` binds tighter than `.` on its left too"
        );
        assert_eq!(
            shape(b"<?php echo 1 . 2 << 3;"),
            shape(b"<?php echo 1 . (2 << 3);"),
            "`<<` binds tighter than `.`"
        );
        assert_eq!(
            shape(b"<?php echo 1 << 2 . 3;"),
            shape(b"<?php echo (1 << 2) . 3;"),
            "`<<` binds tighter than `.` on its left too"
        );
        assert_eq!(
            shape(b"<?php echo 1 . 2 . 3;"),
            shape(b"<?php echo (1 . 2) . 3;"),
            "`.` reads left to right"
        );
        assert_eq!(
            read(b"<?php echo 'a' . 'b';").unwrap_or_default(),
            vec![(
                Stmt::Echo(vec![Expr::Binary {
                    op: BinaryOp::Concat,
                    left: Box::new(Expr::Str(b"a".to_vec())),
                    right: Box::new(Expr::Str(b"b".to_vec())),
                }]),
                1
            )]
        );
        // php's scanner takes `1.` and `.5` as numbers before the
        // parser sees a `.`, so these are php's parse errors.
        assert_eq!(
            read(b"<?php echo 1.;").unwrap_or_default(),
            vec![(Stmt::Echo(vec![Expr::Float(1.0)]), 1)]
        );
        for source in [
            b"<?php echo 1.'a';".as_slice(),
            b"<?php echo 'a'.5;".as_slice(),
            b"<?php echo 'a' ... 'b';".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    #[test]
    fn a_shift_stands_below_php_s_arithmetic_and_reads_left_to_right() {
        assert_eq!(
            shape(b"<?php echo 1 << 2 + 3;"),
            shape(b"<?php echo 1 << (2 + 3);"),
            "`+` binds tighter than `<<`"
        );
        assert_eq!(
            shape(b"<?php echo 1 + 2 >> 3;"),
            shape(b"<?php echo (1 + 2) >> 3;"),
            "`+` binds tighter than `>>`"
        );
        assert_eq!(
            shape(b"<?php echo 1 << 2 >> 3;"),
            shape(b"<?php echo (1 << 2) >> 3;"),
            "`<<` and `>>` are one level, read left to right"
        );
        assert_eq!(
            read(b"<?php echo 8 >> 1;").unwrap_or_default(),
            vec![(
                Stmt::Echo(vec![Expr::Binary {
                    op: BinaryOp::Sr,
                    left: Box::new(Expr::Int(8)),
                    right: Box::new(Expr::Int(1)),
                }]),
                1
            )]
        );
        for source in [
            b"<?php echo 1 <<= 2;".as_slice(),
            b"<?php echo 1 >>= 2;".as_slice(),
            b"<?php echo <<<EOT\nx\nEOT;".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    #[test]
    fn a_power_stands_above_unary_minus_and_reads_right_to_left() {
        assert_eq!(
            shape(b"<?php echo -2 ** 2;"),
            shape(b"<?php echo -(2 ** 2);"),
            "`**` binds tighter than unary `-`"
        );
        assert_eq!(
            shape(b"<?php echo 2 ** 3 ** 2;"),
            shape(b"<?php echo 2 ** (3 ** 2);"),
            "`**` reads right to left"
        );
        assert_eq!(
            shape(b"<?php echo 2 ** 3 * 4;"),
            shape(b"<?php echo (2 ** 3) * 4;"),
            "`**` binds tighter than `*`"
        );
        assert_eq!(
            read(b"<?php echo 2 ** -1;").unwrap_or_default(),
            vec![(
                Stmt::Echo(vec![Expr::Binary {
                    op: BinaryOp::Pow,
                    left: Box::new(Expr::Int(2)),
                    right: Box::new(Expr::Unary {
                        minus: true,
                        operand: Box::new(Expr::Int(1)),
                    }),
                }]),
                1
            )],
            "the right side of `**` is a unary expression"
        );
    }

    /// The one loop php's own shape has, the bench of #156 runs and
    /// [`php_heap::Str::bytes`]'s witness is.
    #[test]
    fn a_comparison_and_an_increment_make_php_s_own_for_loop() {
        assert_eq!(
            read(b"<?php for ($i = 0; $i < 1000; $i++) { echo 'dax'; }").unwrap_or_default(),
            vec![(
                Stmt::For {
                    init: vec![Expr::Assign {
                        var: b"i".to_vec(),
                        value: Box::new(Expr::Int(0)),
                    }],
                    cond: vec![Expr::Binary {
                        op: BinaryOp::IsSmaller,
                        left: Box::new(Expr::Var(b"i".to_vec())),
                        right: Box::new(Expr::Int(1000)),
                    }],
                    step: vec![Expr::PostInc(b"i".to_vec())],
                    body: vec![(Stmt::Echo(vec![Expr::Str(b"dax".to_vec())]), 1)],
                },
                1
            )]
        );
    }

    #[test]
    fn a_comparison_stands_below_the_concatenation_and_does_not_associate() {
        assert_eq!(
            shape(b"<?php echo 1 + 2 < 3 . 4;"),
            shape(b"<?php echo (1 + 2) < (3 . 4);"),
            "`.` binds tighter than `<`, and so does `+`"
        );
        assert_eq!(
            shape(b"<?php $a = 1 < 2;"),
            shape(b"<?php $a = (1 < 2);"),
            "`=` binds below `<`"
        );
        assert_eq!(
            shape(b"<?php echo 1 <= 2 . 3, 1 > 2 . 3, 1 >= 2 . 3;"),
            shape(b"<?php echo 1 <= (2 . 3), 1 > (2 . 3), 1 >= (2 . 3);"),
            "the four of the level stand together"
        );
        for source in [
            b"<?php echo 1 < 2 < 3;".as_slice(),
            b"<?php echo 1 <= 2 > 3;".as_slice(),
            b"<?php echo 1 > 2 >= 3;".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    /// php's equality level, `%nonassoc T_IS_EQUAL T_IS_NOT_EQUAL
    /// T_IS_IDENTICAL T_IS_NOT_IDENTICAL T_SPACESHIP`, one line below
    /// the comparisons in `zend_language_parser.y`.
    #[test]
    fn an_equality_stands_below_the_comparison_and_does_not_associate() {
        assert_eq!(
            shape(b"<?php echo 1 < 2 == 3 < 4, 1 . 2 != 3 <= 4, 1 <> 2;"),
            shape(b"<?php echo (1 < 2) == (3 < 4), (1 . 2) != (3 <= 4), 1 != 2;"),
            "`<` binds tighter than `==`, and `<>` is `!=`"
        );
        assert_eq!(
            shape(b"<?php $a = 1 === 2; $b = 1 !== 2; $c = 1 <=> 2;"),
            shape(b"<?php $a = (1 === 2); $b = (1 !== 2); $c = (1 <=> 2);"),
            "`=` binds below each of them"
        );
        let echo =
            read(b"<?php echo 1 === 2, 1 !== 2, 1 <=> 2, 1 == 2, 1 != 2;").unwrap_or_default();
        let ops: Vec<BinaryOp> = match &echo[0].0 {
            Stmt::Echo(exprs) => exprs
                .iter()
                .filter_map(|e| match e {
                    Expr::Binary { op, .. } => Some(*op),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(
            ops,
            vec![
                BinaryOp::IsIdentical,
                BinaryOp::IsNotIdentical,
                BinaryOp::Spaceship,
                BinaryOp::IsEqual,
                BinaryOp::IsNotEqual
            ],
            "the longest operator is the one read, `!==` before `!=` and `<=>` before `<=`"
        );
        for source in [
            b"<?php echo 1 == 2 == 3;".as_slice(),
            b"<?php echo 1 === 2 != 3;".as_slice(),
            b"<?php echo 1 <=> 2 <=> 3;".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    #[test]
    fn an_increment_takes_a_variable_and_nothing_else() {
        assert_eq!(
            shape(b"<?php echo -$a++ + ++$b ** 2;"),
            shape(b"<?php echo (-($a++)) + ((++$b) ** 2);"),
            "the variable and its `++` are one operand"
        );
        assert_eq!(
            read(b"<?php $a+++$b;").unwrap_or_default(),
            vec![(
                Stmt::Effect(Expr::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(Expr::PostInc(b"a".to_vec())),
                    right: Box::new(Expr::Var(b"b".to_vec())),
                }),
                1
            )],
            "php's scanner takes `++` before `+`"
        );
        for source in [
            b"<?php echo 1++;".as_slice(),
            b"<?php echo ++1;".as_slice(),
            b"<?php echo $a++++;".as_slice(),
            b"<?php echo ++$a++;".as_slice(),
            b"<?php echo ++f();".as_slice(),
            b"<?php echo $a--;".as_slice(),
            b"<?php echo --$a;".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    #[test]
    fn a_for_reads_its_three_lists_and_a_body_of_either_shape() {
        assert_eq!(
            read(b"<?php for (;;) {}").unwrap_or_default(),
            vec![(
                Stmt::For {
                    init: vec![],
                    cond: vec![],
                    step: vec![],
                    body: vec![],
                },
                1
            )]
        );
        assert_eq!(
            read(b"<?php FOR (f(); 1, 2; g())\n  echo 3;").unwrap_or_default(),
            vec![(
                Stmt::For {
                    init: vec![Expr::Call {
                        name: b"f".to_vec(),
                        arguments: vec![],
                    }],
                    cond: vec![Expr::Int(1), Expr::Int(2)],
                    step: vec![Expr::Call {
                        name: b"g".to_vec(),
                        arguments: vec![],
                    }],
                    body: vec![(Stmt::Echo(vec![Expr::Int(3)]), 2)],
                },
                1
            )],
            "the keyword whatever its case, and one statement as the body at its own line"
        );
        assert_eq!(
            read(b"<?php for (;;);").unwrap_or_default(),
            vec![(
                Stmt::For {
                    init: vec![],
                    cond: vec![],
                    step: vec![],
                    body: vec![],
                },
                1
            )],
            "the empty statement is a body with nothing in it"
        );
    }

    #[test]
    fn a_while_and_a_block_keep_their_statements_and_lines() {
        assert_eq!(
            read(b"<?php\nwhile (true) {\n  echo 1;\n  echo 2;\n}\n{ echo 3; }")
                .unwrap_or_default(),
            vec![
                (
                    Stmt::While {
                        cond: Expr::Bool(true),
                        body: vec![
                            (Stmt::Echo(vec![Expr::Int(1)]), 3),
                            (Stmt::Echo(vec![Expr::Int(2)]), 4),
                        ],
                    },
                    2
                ),
                (Stmt::Block(vec![(Stmt::Echo(vec![Expr::Int(3)]), 6)]), 6),
            ]
        );
    }

    #[test]
    fn a_loop_this_cannot_read_is_refused_rather_than_mis_read() {
        for source in [
            b"<?php for (;;) { echo 1;".as_slice(),
            b"<?php for (;;) { ?>x<?php }".as_slice(),
            b"<?php for (1 2;;) {}".as_slice(),
            b"<?php while 1 {}".as_slice(),
            b"<?php while (1 {}".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    #[test]
    fn what_it_cannot_read_is_refused_at_its_line() {
        let trouble = read(b"<?php\n$x | 1;").err();
        assert_eq!(trouble.map(|t| t.line), Some(2));
    }

    #[test]
    fn a_variable_is_read_where_an_operand_stands_and_an_assignment_below_every_operator() {
        let read = read(b"<?php $a = 1 + $b; echo $a, $b = $c = f($a);").unwrap_or_default();
        assert_eq!(
            read[0],
            (
                Stmt::Effect(Expr::Assign {
                    var: b"a".to_vec(),
                    value: Box::new(Expr::Binary {
                        op: BinaryOp::Add,
                        left: Box::new(Expr::Int(1)),
                        right: Box::new(Expr::Var(b"b".to_vec())),
                    }),
                }),
                1
            ),
            "`=` binds below `+`"
        );
        assert_eq!(
            read[1],
            (
                Stmt::Echo(vec![
                    Expr::Var(b"a".to_vec()),
                    Expr::Assign {
                        var: b"b".to_vec(),
                        value: Box::new(Expr::Assign {
                            var: b"c".to_vec(),
                            value: Box::new(Expr::Call {
                                name: b"f".to_vec(),
                                arguments: vec![Expr::Var(b"a".to_vec())],
                            }),
                        }),
                    },
                ]),
                1
            ),
            "`=` reads right to left"
        );
    }

    #[test]
    fn a_variable_php_makes_no_cv_of_is_refused_rather_than_read_as_one() {
        for source in [
            b"<?php echo $this;".as_slice(),
            b"<?php echo $GLOBALS;".as_slice(),
            b"<?php echo $_SERVER;".as_slice(),
            b"<?php echo $$a;".as_slice(),
            b"<?php echo ${'a'};".as_slice(),
            b"<?php echo $1;".as_slice(),
            b"<?php 1 = 2;".as_slice(),
            b"<?php 1 + $a = 2;".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
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
