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
//! a `for` and a `while` whose body is a block or one statement, a
//! `switch` with its cases between braces, `break` and `continue` with
//! or without a depth, a top-level `class` with no parent, interface,
//! attribute or modifier whose members are untyped properties and
//! constants, each declarator taking the pending doc comment as php's
//! `backup_doc_comment` takes it, the
//! unbraced `namespace A\B;`, and
//! inside an expression a `match`, a number, a single- or double-quoted string with no
//! interpolation, `true`, `false`, `null`, a constant by name, a call of
//! a named function, a parenthesised expression, a variable `$a` that is
//! a CV, an assignment `$a = expr` to one, `$a++` and `++$a` on one,
//! unary `-` and `+`, a cast to a scalar type in any of the spellings
//! php's scanner takes, and `<`, `.`, `<<`, `>>`, `+`, `-`, `*`, `/`, `%`
//! and `**` at php's precedence levels: `<` under `.` under the shifts
//! under the additive level under the multiplicative one, `<` not
//! associating and the rest left to right, and `**` right to left above
//! unary `-`, with `=` below them all and read right to left.
//! Anything else is a [`crate::CompileError`], so what this cannot read is
//! refused rather than silently compiled to something else.

use std::fmt;
use std::ops::Deref;

use php_unit::Level;
use php_unit::decl::Visibility;

use crate::StackLimit;

/// One statement.
#[derive(Debug, PartialEq)]
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
    /// `namespace A\B;`, php's `ZEND_AST_NAMESPACE` with no body: the
    /// name as written, in force for what follows it. The braced form
    /// and the global `namespace;` are refused as what this reader
    /// cannot read yet.
    Namespace(Vec<u8>),
    /// `switch (expr) { case …: … default: … }`, php's `ZEND_AST_SWITCH`
    /// over a `ZEND_AST_SWITCH_LIST`.
    Switch { expr: Expr, cases: Vec<Case> },
    /// `break;` or `break n;`, php's `ZEND_AST_BREAK` with the depth
    /// written after it, if one is; what it may be is the emitter's to
    /// refuse as `zend_compile_break_continue` refuses it.
    Break(Option<Expr>),
    /// `continue;` or `continue n;`, php's `ZEND_AST_CONTINUE`.
    Continue(Option<Expr>),
    /// `class M { … }` at the top level, php's `ZEND_AST_CLASS`.
    Class(Class),
}

/// One class declaration, php's `zend_ast_decl` for a class: the name as
/// written, the doc comment taken before its `{` (`class_declaration_statement`
/// reduces `backup_doc_comment` after `implements_list`), and its
/// members as php's `ZEND_AST_PROP_GROUP` and `ZEND_AST_CLASS_CONST_GROUP`
/// hold them, one declarator each so the doc comment and the line are
/// the declarator's own.
#[derive(Debug, PartialEq)]
pub(crate) struct Class {
    pub(crate) name: Vec<u8>,
    pub(crate) doc_comment: Option<Vec<u8>>,
    pub(crate) properties: Vec<Property>,
    pub(crate) constants: Vec<Constant>,
}

/// One property declarator, php's `ZEND_AST_PROP_ELEM` with the group's
/// modifiers beside it: `public $x`, `static $y = 1`. Its doc comment is
/// what was pending after its last token, php's `property: T_VARIABLE
/// backup_doc_comment | T_VARIABLE '=' expr backup_doc_comment`.
#[derive(Debug, PartialEq)]
pub(crate) struct Property {
    pub(crate) name: Vec<u8>,
    pub(crate) visibility: Visibility,
    pub(crate) is_static: bool,
    pub(crate) default: Option<Expr>,
    pub(crate) doc_comment: Option<Vec<u8>>,
    pub(crate) line: u32,
}

/// One class constant declarator, php's `ZEND_AST_CONST_ELEM` with the
/// group's modifiers beside it: `const A = 1`, `final protected const B = 2`.
/// Its doc comment is taken after its value, `class_const_decl: identifier
/// '=' expr backup_doc_comment`.
#[derive(Debug, PartialEq)]
pub(crate) struct Constant {
    pub(crate) name: Vec<u8>,
    pub(crate) visibility: Visibility,
    pub(crate) is_final: bool,
    pub(crate) value: Expr,
    pub(crate) doc_comment: Option<Vec<u8>>,
    pub(crate) line: u32,
}

/// One `case cond:` or `default:` of a `switch` with the statements
/// under it, php's `ZEND_AST_SWITCH_CASE`: the condition at the line it
/// starts on, which the opline over it carries, and the keyword's line.
#[derive(Debug, PartialEq)]
pub(crate) struct Case {
    pub(crate) cond: Option<(Expr, u32)>,
    pub(crate) body: Read,
    pub(crate) line: u32,
}

/// One `cond, cond => body` or `default => body` of a `match`, php's
/// `ZEND_AST_MATCH_ARM`: the conditions, none for the default arm, each
/// at its own line, and the body at its.
#[derive(Debug, PartialEq)]
pub(crate) struct Arm {
    pub(crate) conds: Option<Vec<(Expr, u32)>>,
    pub(crate) body: Expr,
    pub(crate) line: u32,
}

/// One expression.
#[derive(Debug, PartialEq)]
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
        operand: Child,
    },
    /// `(int)$a`, php's `ZEND_AST_CAST` with the type in `attr`.
    Cast {
        to: CastTo,
        operand: Child,
    },
    /// `$a + $b`, php's `ZEND_AST_BINARY_OP`.
    Binary {
        op: BinaryOp,
        left: Child,
        right: Child,
    },
    Call {
        name: Vec<u8>,
        arguments: Vec<Expr>,
    },
    /// `$a`, php's `ZEND_AST_VAR` over a name written as is, which
    /// `zend_try_compile_cv` makes a CV: not `$this`, which is [`Expr::This`],
    /// and not `$_GET` or its kin, which php fetches by other oplines and
    /// this refuses.
    Var(Vec<u8>),
    /// `$this`, php's `ZEND_AST_VAR` over the one name `is_this_fetch`
    /// picks out, fetched by `FETCH_THIS` and never a CV.
    This,
    /// `$o->p`, php's `ZEND_AST_PROP` over any expression and a property
    /// named as written; `$o?->p` and `$o->$p` are not read.
    Prop {
        object: Child,
        name: Vec<u8>,
    },
    /// `$a = expr`, php's `ZEND_AST_ASSIGN` to a variable.
    Assign {
        var: Vec<u8>,
        value: Child,
    },
    /// `$o->p = expr`, php's `ZEND_AST_ASSIGN` to a `ZEND_AST_PROP`:
    /// the property's receiver, its name as written, and the value.
    AssignProp {
        object: Child,
        name: Vec<u8>,
        value: Child,
    },
    /// `$b = &$a`, php's `ZEND_AST_ASSIGN_REF` over two variables, the
    /// target and the source, each a name written as is. php's grammar
    /// puts a variable and nothing else after the `&`; a property, a
    /// dimension or a call there is not read yet.
    AssignRef {
        var: Vec<u8>,
        source: Vec<u8>,
    },
    /// `++$a`, php's `ZEND_AST_PRE_INC`, whose grammar puts a variable
    /// and nothing else after the `++`.
    PreInc(Vec<u8>),
    /// `$a++`, php's `ZEND_AST_POST_INC`.
    PostInc(Vec<u8>),
    /// `--$a`, php's `ZEND_AST_PRE_DEC`, the same grammar as `++$a`.
    PreDec(Vec<u8>),
    /// `$a--`, php's `ZEND_AST_POST_DEC`.
    PostDec(Vec<u8>),
    /// `match (expr) { arms }`, php's `ZEND_AST_MATCH` over a
    /// `ZEND_AST_MATCH_ARM_LIST`; `line` is the subject's, which the
    /// `MATCH` and the chain over it are emitted at.
    Match {
        expr: Child,
        arms: Vec<Arm>,
        line: u32,
    },
    /// `new C(args)`, php's `ZEND_AST_NEW` over a class named as
    /// written; `new $c`, `new static` and an anonymous class are not
    /// read.
    New {
        class: Vec<u8>,
        arguments: Vec<Expr>,
    },
    /// `C::m(args)`, php's `ZEND_AST_STATIC_CALL` over a class and a
    /// method both named as written.
    StaticCall {
        class: Vec<u8>,
        method: Vec<u8>,
        arguments: Vec<Expr>,
    },
    /// `$o->m(args)`, php's `ZEND_AST_METHOD_CALL` over any expression
    /// and a method named as written.
    MethodCall {
        object: Child,
        method: Vec<u8>,
        arguments: Vec<Expr>,
    },
    /// `$o instanceof C`, php's `ZEND_AST_INSTANCEOF` over a class named
    /// as written.
    Instanceof {
        expr: Child,
        class: Vec<u8>,
    },
}

/// One child of an expression, php's `zend_ast.child[i]`: the expression
/// itself behind a pointer, dropped as `zend_ast_destroy` frees it. That
/// walk recurses on every child but the first, and takes the first in a
/// loop (`tail_call`, `Z/Zend/zend_ast.c:1367-1379`), so a left-nested
/// chain, `1+1+1+…` as the grammar groups it, is freed in constant stack
/// however long it is. A derived drop would recurse on both, and the
/// chain the compiler refuses at its stack limit ([`StackLimit`]) would
/// then take the thread down on the way out instead.
#[derive(PartialEq)]
pub(crate) struct Child(Box<Expr>);

impl Child {
    pub(crate) fn new(expr: Expr) -> Child {
        Child(Box::new(expr))
    }

    /// The expression out, `Null` left in its place for the drop to find.
    fn take(mut self) -> Expr {
        std::mem::replace(&mut self.0, Expr::Null)
    }
}

impl Deref for Child {
    type Target = Expr;

    fn deref(&self) -> &Expr {
        &self.0
    }
}

impl fmt::Debug for Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let mut first = std::mem::replace(&mut *self.0, Expr::Null);
        loop {
            first = match first {
                Expr::Unary { operand, .. } | Expr::Cast { operand, .. } => operand.take(),
                Expr::Binary { left, right, .. } => {
                    drop(right);
                    left.take()
                }
                Expr::Assign { value, .. } => value.take(),
                Expr::AssignProp { object, value, .. } => {
                    drop(value);
                    object.take()
                }
                _ => return,
            };
        }
    }
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

impl BinaryOp {
    /// The operator as php's grammar spells it, what a constant
    /// expression's `Binary` node names it by (design §4.7).
    pub(crate) fn spelling(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Pow => "**",
            BinaryOp::Sl => "<<",
            BinaryOp::Sr => ">>",
            BinaryOp::Concat => ".",
            BinaryOp::IsSmaller => "<",
            BinaryOp::IsSmallerOrEqual => "<=",
            BinaryOp::Greater => ">",
            BinaryOp::GreaterOrEqual => ">=",
            BinaryOp::IsEqual => "==",
            BinaryOp::IsNotEqual => "!=",
            BinaryOp::IsIdentical => "===",
            BinaryOp::IsNotIdentical => "!==",
            BinaryOp::Spaceship => "<=>",
        }
    }
}

/// The type a cast is to, php's `attr` on `ZEND_AST_CAST`: the scanner's
/// `T_INT_CAST`, `T_DOUBLE_CAST`, `T_STRING_CAST`, `T_ARRAY_CAST`,
/// `T_OBJECT_CAST` and `T_BOOL_CAST`, each scalar one with the canonical
/// spelling and the one 8.5 deprecates (`zend_language_scanner.l` at the
/// pin). `(unset)` is refused as the compile error `zend_compile_cast`
/// raises for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastTo {
    /// `(int)` and `(integer)`: `IS_LONG`.
    Long,
    /// `(float)` and `(double)`: `IS_DOUBLE`.
    Double,
    /// `(string)` and `(binary)`: `IS_STRING`.
    Str,
    /// `(array)`: `IS_ARRAY`.
    Array,
    /// `(object)`: `IS_OBJECT`.
    Object,
    /// `(bool)` and `(boolean)`: `_IS_BOOL`, which php compiles to
    /// `ZEND_BOOL` and never to `ZEND_CAST`.
    Bool,
}

impl CastTo {
    /// php's type constant, `ZEND_CAST`'s `extended_value`.
    pub(crate) fn type_code(self) -> u32 {
        match self {
            CastTo::Long => 4,
            CastTo::Double => 5,
            CastTo::Str => 6,
            CastTo::Array => 7,
            CastTo::Object => 8,
            // `_IS_BOOL` is 18 and reaches no opline: `ZEND_BOOL` carries
            // nothing in `extended_value`.
            CastTo::Bool => 0,
        }
    }
}

/// The line each statement stands on, beside it.
pub(crate) type Read = Vec<(Stmt, u32)>;

/// A place in the source, and php's line number for it.
struct Reader<'a> {
    source: &'a [u8],
    at: usize,
    line: u32,
    /// The thread's stack limit, held to where the reader recurses: this
    /// walk stands in for the compiler's over the tree (design §4.9), so
    /// the check `zend_compile_expr` makes is made here.
    stack: StackLimit,
    /// php's `CG(doc_comment)`: the last `/**` comment the scanner passed,
    /// cleared by every `}` (`zend_language_scanner.l`, the
    /// `<ST_IN_SCRIPTING>"}"` rule at 8.5.9) and taken, then cleared,
    /// where the grammar reduces `backup_doc_comment`; the rule
    /// `php_syntax::tree::builder` walks, kept here the same way.
    doc_comment: Option<Vec<u8>>,
}

/// What one source file holds, or the first thing in it this cannot read.
pub(crate) fn read(source: &[u8], stack: StackLimit) -> Result<Read, Trouble> {
    let mut reader = Reader {
        source,
        at: 0,
        line: 1,
        stack,
        doc_comment: None,
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

/// What the reader could not read: php's line, what stood there, and
/// the level it refuses at, `E_PARSE` for what this reader cannot read
/// yet and `E_COMPILE_ERROR` for what php's compiler refuses.
pub(crate) struct Trouble {
    pub(crate) level: Level,
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

    /// `len` bytes from `ahead` past where the reader stands, fewer at the end.
    fn taken_ahead(&self, ahead: usize, len: usize) -> &[u8] {
        let start = (self.at + ahead).min(self.source.len());
        let end = (start + len).min(self.source.len());
        self.source.get(start..end).unwrap_or_default()
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
            level: Level::Parse,
            what: what.into(),
            line: self.line,
        }
    }

    /// `zend_check_stack_limit` where the reader recurses.
    fn check_stack(&self) -> Result<(), Trouble> {
        self.stack.check(self.line).map_err(|refused| Trouble {
            level: refused.level,
            what: refused.message,
            line: refused.line,
        })
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
                self.close_brace();
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
        if self.word_ahead(b"switch") {
            self.bump(6);
            return self.switch().map(Some);
        }
        if self.word_ahead(b"break") {
            self.bump(5);
            return self.depth().map(|depth| Some(Stmt::Break(depth)));
        }
        if self.word_ahead(b"continue") {
            self.bump(8);
            return self.depth().map(|depth| Some(Stmt::Continue(depth)));
        }
        if self.word_ahead(b"class") {
            self.bump(5);
            return self.class().map(Some);
        }
        if self.word_ahead(b"namespace") && self.byte(9) != Some(b'\\') {
            self.bump(9);
            return self.namespace().map(Some);
        }
        let expr = self.expr()?;
        self.end_of_statement()?;
        Ok(Some(Stmt::Effect(expr)))
    }

    /// `class M { members }`, the keyword read: php's
    /// `class_declaration_statement` with no modifier, no `extends`, no
    /// `implements` and no attribute, which are refused as what this
    /// cannot read yet rather than read as a class without them. The
    /// name is what `zend_assert_valid_class_name` refuses when it is a
    /// reserved one, php's text at php's level. The doc comment is taken
    /// before the `{`. The members are property groups and constant
    /// groups; a method, a typed property or constant, a trait `use`,
    /// an enum case, an accessor list or an attribute is refused the same way.
    fn class(&mut self) -> Result<Stmt, Trouble> {
        self.skip_trivia();
        let name = self.name();
        if name.is_empty() || name.contains(&b'\\') {
            return Err(self.trouble("a class this compiler cannot read yet"));
        }
        if is_reserved_class_name(&name) {
            return Err(Trouble {
                level: Level::CompileError,
                what: format!(
                    "Cannot use \"{}\" as a class name as it is reserved",
                    String::from_utf8_lossy(&name)
                ),
                line: self.line,
            });
        }
        self.skip_trivia();
        if self.byte(0) != Some(b'{') {
            return Err(self.trouble(
                "a class with a parent, an interface or an attribute this compiler cannot read yet",
            ));
        }
        let doc_comment = self.doc_comment.take();
        self.bump(1);
        let mut class = Class {
            name,
            doc_comment,
            properties: Vec::new(),
            constants: Vec::new(),
        };
        loop {
            self.skip_trivia();
            if self.byte(0) == Some(b'}') {
                self.close_brace();
                return Ok(Stmt::Class(class));
            }
            if self.at >= self.source.len() {
                return Err(self.trouble("a class no brace closes"));
            }
            self.member(&mut class)?;
        }
    }

    /// One member group of a class body: the modifiers, then a constant
    /// group or a property group. php's `zend_add_member_modifier`
    /// refuses a second visibility in its own words; every other
    /// modifier this does not model is refused as not yet read.
    fn member(&mut self, class: &mut Class) -> Result<(), Trouble> {
        let line = self.line;
        let mut visibility = None;
        let mut is_static = false;
        let mut is_final = false;
        loop {
            self.skip_trivia();
            let seen = if self.word_ahead(b"public") {
                Some(Visibility::Public)
            } else if self.word_ahead(b"protected") {
                Some(Visibility::Protected)
            } else if self.word_ahead(b"private") {
                Some(Visibility::Private)
            } else if self.word_ahead(b"var") {
                Some(Visibility::Public)
            } else {
                None
            };
            if let Some(seen) = seen {
                if visibility.is_some() {
                    return Err(Trouble {
                        level: Level::CompileError,
                        what: String::from("Multiple access type modifiers are not allowed"),
                        line: self.line,
                    });
                }
                visibility = Some(seen);
                self.name();
                continue;
            }
            if self.word_ahead(b"static") {
                self.bump(6);
                is_static = true;
                continue;
            }
            if self.word_ahead(b"final") {
                self.bump(5);
                is_final = true;
                continue;
            }
            break;
        }
        let visibility = visibility.unwrap_or_default();
        if self.word_ahead(b"const") && !is_static {
            self.bump(5);
            return self.constants(class, visibility, is_final, line);
        }
        if self.byte(0) == Some(b'$') && !is_final {
            return self.properties(class, visibility, is_static, line);
        }
        Err(self.trouble("a class member this compiler cannot read yet"))
    }

    /// `const A = 1, B = 2;` with the group's modifiers already read:
    /// each declarator takes the doc comment after its value. A type
    /// before the name (8.3) is refused as not yet read.
    fn constants(
        &mut self,
        class: &mut Class,
        visibility: Visibility,
        is_final: bool,
        line: u32,
    ) -> Result<(), Trouble> {
        loop {
            self.skip_trivia();
            let name = self.name();
            if name.is_empty() || name.contains(&b'\\') {
                return Err(self.trouble("a class constant this compiler cannot read yet"));
            }
            self.skip_trivia();
            if self.byte(0) != Some(b'=') {
                return Err(self.trouble("a class constant this compiler cannot read yet"));
            }
            self.bump(1);
            let value = self.expr()?;
            self.skip_trivia();
            let doc_comment = self.doc_comment.take();
            class.constants.push(Constant {
                name,
                visibility,
                is_final,
                value,
                doc_comment,
                line,
            });
            match self.byte(0) {
                Some(b',') => self.bump(1),
                Some(b';') => {
                    self.bump(1);
                    return Ok(());
                }
                _ => return Err(self.trouble("a class constant this compiler cannot read yet")),
            }
        }
    }

    /// `$x, $y = 1;` with the group's modifiers already read: each
    /// declarator takes the doc comment after its last token, the
    /// variable or the default. An accessor list after a declarator is
    /// refused as not yet read.
    fn properties(
        &mut self,
        class: &mut Class,
        visibility: Visibility,
        is_static: bool,
        line: u32,
    ) -> Result<(), Trouble> {
        loop {
            self.skip_trivia();
            if self.byte(0) != Some(b'$') {
                return Err(self.trouble("a property this compiler cannot read yet"));
            }
            self.bump(1);
            let name = self.name();
            if name.is_empty() || name.contains(&b'\\') {
                return Err(self.trouble("a property this compiler cannot read yet"));
            }
            self.skip_trivia();
            let default = if self.byte(0) == Some(b'=') {
                self.bump(1);
                let value = self.expr()?;
                self.skip_trivia();
                Some(value)
            } else {
                None
            };
            let doc_comment = self.doc_comment.take();
            class.properties.push(Property {
                name,
                visibility,
                is_static,
                default,
                doc_comment,
                line,
            });
            match self.byte(0) {
                Some(b',') => self.bump(1),
                Some(b';') => {
                    self.bump(1);
                    return Ok(());
                }
                _ => return Err(self.trouble("a property this compiler cannot read yet")),
            }
        }
    }

    /// A `}`: php's scanner clears the pending doc comment at every one.
    fn close_brace(&mut self) {
        self.bump(1);
        self.doc_comment = None;
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

    /// `switch (expr) { … }`, the keyword read: the subject between the
    /// parentheses, then the cases between the braces, each `case expr`
    /// or `default` closed by `:` or `;` as php's grammar takes either,
    /// with the statements up to the next case or the closing brace.
    /// The alternative syntax, `switch (…): … endswitch;`, is not read
    /// yet.
    fn switch(&mut self) -> Result<Stmt, Trouble> {
        self.punctuation(b'(', "a `switch` with no parenthesis after it")?;
        let expr = self.expr()?;
        self.punctuation(b')', "a `switch` whose parenthesis nothing closes")?;
        self.punctuation(b'{', "a `switch` this compiler cannot read yet")?;
        let mut cases = Vec::new();
        loop {
            self.skip_trivia();
            if self.byte(0) == Some(b'}') {
                self.close_brace();
                return Ok(Stmt::Switch { expr, cases });
            }
            if self.byte(0) == Some(b';') {
                // php's grammar takes one `;` after the brace and nothing
                // else before the first case.
                if !cases.is_empty() {
                    return Err(self.trouble("a `switch` this compiler cannot read yet"));
                }
                self.bump(1);
                continue;
            }
            let line = self.line;
            let cond = if self.word_ahead(b"case") {
                self.bump(4);
                self.skip_trivia();
                let cond_line = self.line;
                Some((self.expr()?, cond_line))
            } else if self.word_ahead(b"default") {
                self.bump(7);
                None
            } else {
                return Err(self.trouble("a `switch` whose case this compiler cannot read"));
            };
            self.skip_trivia();
            if !matches!(self.byte(0), Some(b':' | b';')) {
                return Err(self.trouble("a `case` no colon closes"));
            }
            self.bump(1);
            let mut body = Read::new();
            loop {
                self.skip_trivia();
                if self.byte(0) == Some(b'}')
                    || self.word_ahead(b"case")
                    || self.word_ahead(b"default")
                {
                    break;
                }
                if self.at >= self.source.len() {
                    return Err(self.trouble("a `switch` no brace closes"));
                }
                if self.starts_with(b"?>") {
                    return Err(
                        self.trouble("a close tag inside a block this compiler cannot read yet")
                    );
                }
                let line = self.line;
                if let Some(statement) = self.statement()? {
                    body.push((statement, line));
                }
            }
            cases.push(Case { cond, body, line });
        }
    }

    /// What follows a `break` or a `continue`: nothing, or the
    /// expression php's grammar reads before the `;`.
    fn depth(&mut self) -> Result<Option<Expr>, Trouble> {
        self.skip_trivia();
        if self.byte(0) == Some(b';') || self.starts_with(b"?>") || self.at >= self.source.len() {
            self.end_of_statement()?;
            return Ok(None);
        }
        let depth = self.expr()?;
        self.end_of_statement()?;
        Ok(Some(depth))
    }

    /// `match (expr) { arms }`, the keyword read: the subject between
    /// the parentheses, then each arm as php's `match_arm` has it, the
    /// conditions separated by commas with one allowed before the `=>`,
    /// or `default`, then the body; arms separated by commas, with one
    /// allowed before the closing brace.
    fn r#match(&mut self) -> Result<Expr, Trouble> {
        self.punctuation(b'(', "a `match` with no parenthesis after it")?;
        self.skip_trivia();
        let line = self.line;
        let expr = Child::new(self.expr()?);
        self.punctuation(b')', "a `match` whose parenthesis nothing closes")?;
        self.punctuation(b'{', "a `match` with no brace after it")?;
        let mut arms = Vec::new();
        loop {
            self.skip_trivia();
            if self.byte(0) == Some(b'}') {
                self.close_brace();
                return Ok(Expr::Match { expr, arms, line });
            }
            if !arms.is_empty() {
                if self.byte(0) != Some(b',') {
                    return Err(self.trouble("a `match` whose arms no comma separates"));
                }
                self.bump(1);
                self.skip_trivia();
                if self.byte(0) == Some(b'}') {
                    self.close_brace();
                    return Ok(Expr::Match { expr, arms, line });
                }
            }
            let conds = if self.word_ahead(b"default") {
                self.bump(7);
                None
            } else {
                let mut conds = Vec::new();
                loop {
                    self.skip_trivia();
                    if !conds.is_empty() && self.starts_with(b"=>") {
                        break;
                    }
                    if !conds.is_empty() {
                        if self.byte(0) != Some(b',') {
                            return Err(
                                self.trouble("a `match` arm whose conditions no comma separates")
                            );
                        }
                        self.bump(1);
                        self.skip_trivia();
                        if self.starts_with(b"=>") {
                            break;
                        }
                    }
                    let line = self.line;
                    conds.push((self.expr()?, line));
                }
                Some(conds)
            };
            self.skip_trivia();
            if !self.starts_with(b"=>") {
                return Err(self.trouble("a `match` arm with no `=>`"));
            }
            self.bump(2);
            self.skip_trivia();
            let body_line = self.line;
            let body = self.expr()?;
            arms.push(Arm {
                conds,
                body,
                line: body_line,
            });
        }
    }

    /// `namespace Name;`, the keyword read: the name and its `;`. A
    /// keyword followed by `\` is the relative name `namespace\foo` and
    /// is read as an expression instead, so it never reaches here.
    fn namespace(&mut self) -> Result<Stmt, Trouble> {
        self.skip_trivia();
        let name = self.name();
        if name.is_empty() || name.starts_with(b"\\") {
            return Err(self.trouble("a namespace declaration this compiler cannot read yet"));
        }
        self.skip_trivia();
        if self.byte(0) == Some(b'{') {
            return Err(self.trouble("a namespace declaration this compiler cannot read yet"));
        }
        self.end_of_statement()?;
        Ok(Stmt::Namespace(name))
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
        self.check_stack()?;
        let left = self.equality()?;
        self.skip_trivia();
        // `==` is the equality read above, and `=>` closes a `match`
        // arm's conditions.
        if self.byte(0) != Some(b'=') || matches!(self.byte(1), Some(b'=' | b'>')) {
            return Ok(left);
        }
        match left {
            Expr::Var(var) => {
                self.bump(1);
                self.skip_trivia();
                if self.byte(0) == Some(b'&') {
                    self.bump(1);
                    return self.assign_ref(var);
                }
                Ok(Expr::Assign {
                    var,
                    value: Child::new(self.expr()?),
                })
            }
            Expr::Prop { object, name } => {
                self.bump(1);
                Ok(Expr::AssignProp {
                    object,
                    name,
                    value: Child::new(self.expr()?),
                })
            }
            _ => {
                Err(self
                    .trouble("an assignment to what is no variable this compiler cannot read yet"))
            }
        }
    }

    /// The source of `variable '=' ampersand variable`, php's
    /// `ZEND_AST_ASSIGN_REF`, after the `&`: a variable and nothing else,
    /// as php's grammar has it, so `$b = &$a = 1` is php's parse error and
    /// is refused here by what follows. `$this`, `$_GET` and its kin are
    /// refused as [`Self::variable`] refuses them, and a property, a
    /// dimension or a call as the source is not read yet.
    fn assign_ref(&mut self, var: Vec<u8>) -> Result<Expr, Trouble> {
        self.skip_trivia();
        if self.byte(0) != Some(b'$') {
            return Err(self.trouble(
                "a by-reference assignment from what is no variable this compiler cannot read yet",
            ));
        }
        match self.variable()? {
            Expr::Var(source) => Ok(Expr::AssignRef { var, source }),
            _ => {
                Err(self
                    .trouble("a by-reference assignment from $this this compiler cannot read yet"))
            }
        }
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
            left: Child::new(left),
            right: Child::new(right),
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
            left: Child::new(left),
            right: Child::new(right),
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
                left: Child::new(left),
                right: Child::new(right),
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
                left: Child::new(left),
                right: Child::new(right),
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
                left: Child::new(left),
                right: Child::new(right),
            };
        }
    }

    /// `*`, `/` and `%`, php's higher arithmetic level.
    fn multiplicative(&mut self) -> Result<Expr, Trouble> {
        let mut left = self.instance_of()?;
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
            let right = self.instance_of()?;
            left = Expr::Binary {
                op,
                left: Child::new(left),
                right: Child::new(right),
            };
        }
    }

    /// `expr instanceof C`, php's level between `!` and the unary
    /// operators (`%precedence T_INSTANCEOF` stands below `'~'` and the
    /// casts in `zend_language_parser.y`), so `-$a instanceof C` is
    /// php's `(-$a) instanceof C`. The rule is `expr T_INSTANCEOF
    /// class_name_reference` with no associativity declared, so a
    /// second `instanceof` reads the first as its left side, php's
    /// `($o instanceof E) instanceof F`, a chain grouped to the left as
    /// the arithmetic levels are; the oracle at 8.5.9 compiles it to two
    /// `INSTANCEOF`s, the second over the first's temporary, and a
    /// bool on the left is no object, so the chain is `false`.
    ///
    /// ```php
    /// $o = 1;
    /// var_dump($o instanceof E instanceof F);
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    ///
    /// The class is a name as written; `self`, `parent` and `static`
    /// are the emitter's to refuse, and a class held in a variable is
    /// not read.
    fn instance_of(&mut self) -> Result<Expr, Trouble> {
        let mut left = self.unary()?;
        loop {
            self.skip_trivia();
            if !self.word_ahead(b"instanceof") {
                return Ok(left);
            }
            self.bump(10);
            self.skip_trivia();
            if !self.name_ahead() {
                return Err(self.trouble(
                    "an `instanceof` of what is no class name this compiler cannot read yet",
                ));
            }
            let class = self.name();
            left = Expr::Instanceof {
                expr: Child::new(left),
                class,
            };
        }
    }

    /// `-$a` and `+$a`, which bind tighter than either level and nest.
    /// `**` binds tighter still, so `-2 ** 2` is php's `-(2 ** 2)`.
    fn unary(&mut self) -> Result<Expr, Trouble> {
        self.check_stack()?;
        self.skip_trivia();
        let minus = match (self.byte(0), self.byte(1)) {
            (Some(b'-'), next) if !matches!(next, Some(b'-' | b'=')) => true,
            (Some(b'+'), next) if !matches!(next, Some(b'+' | b'=')) => false,
            _ => return self.power(),
        };
        self.bump(1);
        Ok(Expr::Unary {
            minus,
            operand: Child::new(self.unary()?),
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
            left: Child::new(left),
            right: Child::new(self.unary()?),
        })
    }

    /// What an operator stands between: a number or a string as written, a name, a
    /// variable, with `++` or `--` before or after it, or a parenthesised expression.
    fn operand(&mut self) -> Result<Expr, Trouble> {
        self.skip_trivia();
        match self.byte(0) {
            Some(b'\'') => self.single_quoted(),
            Some(b'"') => self.double_quoted(),
            Some(b'$') => {
                let receiver = self.incremented()?;
                self.member_calls(receiver)
            }
            // php's `T_INC variable` and `T_DEC variable`: the variable is
            // the whole operand, so `++$a ** 2` is php's `(++$a) ** 2`.
            Some(step @ (b'+' | b'-')) if self.byte(1) == Some(step) => {
                self.bump(2);
                self.skip_trivia();
                let sign = if step == b'+' { "++" } else { "--" };
                if self.byte(0) != Some(b'$') {
                    return Err(self.trouble(format!("a `{sign}` before what is no variable")));
                }
                let Expr::Var(name) = self.variable()? else {
                    return Err(self.trouble(format!("a `{sign}` before what is no variable")));
                };
                Ok(if step == b'+' {
                    Expr::PreInc(name)
                } else {
                    Expr::PreDec(name)
                })
            }
            Some(b) if b.is_ascii_digit() || (b == b'.' && self.digit_ahead(1)) => self.number(),
            Some(b'(') if self.cast_ahead().is_some() => self.cast(),
            Some(b'(') => {
                self.bump(1);
                let inner = self.expr()?;
                self.skip_trivia();
                if self.byte(0) != Some(b')') {
                    return Err(self.trouble("a parenthesis no close parenthesis matches"));
                }
                self.bump(1);
                self.member_calls(inner)
            }
            Some(b'm' | b'M') if self.word_ahead(b"match") => {
                self.bump(5);
                self.r#match()
            }
            Some(b'n' | b'N') if self.word_ahead(b"new") => {
                self.bump(3);
                self.new_expression()
            }
            Some(b'c' | b'C') if self.word_ahead(b"clone") => {
                self.bump(5);
                self.clone_expression()
            }
            Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b'\\' => self.named(),
            _ => Err(self.trouble("an expression this compiler cannot read yet")),
        }
    }

    /// php's scanner's cast pattern, `"("{TABS_AND_SPACES}<word>{TABS_AND_SPACES}")"`
    /// with the word matched whatever its case, at the reader's `(`: the
    /// word and how far the `)` stands, or nothing where the parenthesis
    /// opens an expression.
    fn cast_ahead(&self) -> Option<(Vec<u8>, usize)> {
        let mut i = 1;
        while matches!(self.byte(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        let start = i;
        while self.byte(i).is_some_and(|b| b.is_ascii_alphabetic()) {
            i += 1;
        }
        let word = self
            .source
            .get(self.at + start..self.at + i)?
            .to_ascii_lowercase();
        while matches!(self.byte(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        if self.byte(i) != Some(b')') {
            return None;
        }
        let known: &[&[u8]] = &[
            b"int", b"integer", b"bool", b"boolean", b"float", b"double", b"real", b"string",
            b"binary", b"array", b"object", b"unset",
        ];
        known.contains(&word.as_slice()).then_some((word, i + 1))
    }

    /// `T_INT_CAST expr` and its kin: the cast token, then the operand at
    /// unary precedence, so `(int)$a ** 2` is php's `(int)($a ** 2)` as
    /// `-$a ** 2` is. `(unset)` is refused at the level `zend_compile_cast` refuses it,
    /// `E_COMPILE_ERROR`, which php raises once the whole file has parsed
    /// and `Parsed::diagnostics` carries there; and `(real)` is the
    /// scanner's own parse error, carried there too.
    fn cast(&mut self) -> Result<Expr, Trouble> {
        let Some((word, len)) = self.cast_ahead() else {
            return Err(self.trouble("an expression this compiler cannot read yet"));
        };
        let to = match word.as_slice() {
            b"int" | b"integer" => CastTo::Long,
            b"float" | b"double" => CastTo::Double,
            b"string" | b"binary" => CastTo::Str,
            b"array" => CastTo::Array,
            b"object" => CastTo::Object,
            b"bool" | b"boolean" => CastTo::Bool,
            b"unset" => {
                return Err(Trouble {
                    level: Level::CompileError,
                    what: "The (unset) cast is no longer supported".into(),
                    line: self.line,
                });
            }
            _ => return Err(self.trouble("a cast this compiler cannot read yet")),
        };
        self.bump(len);
        Ok(Expr::Cast {
            to,
            operand: Child::new(self.unary()?),
        })
    }

    /// A name: `true`, `false` and `null`, which php reads whatever their
    /// case, another constant by name, a call, or a call on a class,
    /// `C::m()`, php's `class_name T_PAAMAYIM_NEKUDOTAYIM member_name
    /// argument_list`. What else stands after a `::`, a class constant,
    /// a property of the class or `::class`, is not read yet.
    fn named(&mut self) -> Result<Expr, Trouble> {
        let name = self.name();
        self.skip_trivia();
        if self.byte(0) == Some(b':') && self.byte(1) == Some(b':') {
            self.bump(2);
            self.skip_trivia();
            let method = self.label();
            self.skip_trivia();
            if method.is_empty() || self.byte(0) != Some(b'(') {
                return Err(self.trouble(
                    "a class constant or a static property this compiler cannot read yet",
                ));
            }
            self.bump(1);
            let arguments = self.arguments()?;
            return self.member_calls(Expr::StaticCall {
                class: name,
                method,
                arguments,
            });
        }
        // php's grammar reads `unset(`, `isset(` and `empty(` as its
        // own constructs, never as a call by that name, and each is
        // still to be read; a `Call` named so would be emitted as
        // `INIT_FCALL_BY_NAME` for a function that cannot exist.
        if self.byte(0) == Some(b'(')
            && matches!(
                name.to_ascii_lowercase().as_slice(),
                b"unset" | b"isset" | b"empty"
            )
        {
            return Err(self.trouble(format!(
                "an `{}` this compiler cannot read yet",
                String::from_utf8_lossy(&name.to_ascii_lowercase())
            )));
        }
        if self.byte(0) != Some(b'(') {
            return match name.to_ascii_lowercase().as_slice() {
                b"true" => Ok(Expr::Bool(true)),
                b"false" => Ok(Expr::Bool(false)),
                b"null" => Ok(Expr::Null),
                _ => Ok(Expr::Constant(name)),
            };
        }
        self.bump(1);
        let arguments = self.arguments()?;
        self.member_calls(Expr::Call { name, arguments })
    }

    /// `argument_list` after its `(`: each argument an expression, `,`
    /// between them, up to the `)`. A trailing `,`, a by-reference `&`,
    /// an unpacked `...` and a named argument are not read.
    fn arguments(&mut self) -> Result<Vec<Expr>, Trouble> {
        let mut arguments = Vec::new();
        loop {
            self.skip_trivia();
            if self.byte(0) == Some(b')') {
                self.bump(1);
                return Ok(arguments);
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

    /// `clone expr`, the keyword read: php's parser makes it the call
    /// `\clone(expr)`, `ZEND_AST_CALL` over the known name with
    /// `ZEND_NAME_FQ` and one argument (`Z/Zend/zend_language_parser.y:1264`),
    /// and the compiler's special table takes it from there. `T_CLONE`
    /// binds tighter than every operator, so the operand is what stands
    /// before the first one: an operand with its member calls, or, where
    /// a sign opens it, that unary expression, since nothing is reduced
    /// before the sign's operand is read.
    fn clone_expression(&mut self) -> Result<Expr, Trouble> {
        self.skip_trivia();
        let operand = match (self.byte(0), self.byte(1)) {
            (Some(sign @ (b'-' | b'+')), next) if next != Some(sign) => self.unary()?,
            _ => self.operand()?,
        };
        Ok(Expr::Call {
            name: b"\\clone".to_vec(),
            arguments: vec![operand],
        })
    }

    /// `new C(args)` and `new C`, the keyword read: php's `new_expr` over
    /// a class named as written, `ctor_arguments` optional. What else
    /// may follow `new`, a variable, `static` or `class`, is not read.
    /// A `->` after the arguments is 8.4's `new C()->m()`; without them
    /// it is php's parse error, and the receiver returned here ends the
    /// operand.
    fn new_expression(&mut self) -> Result<Expr, Trouble> {
        self.skip_trivia();
        if !self.name_ahead() {
            return Err(
                self.trouble("a `new` of what is no class name this compiler cannot read yet")
            );
        }
        let class = self.name();
        self.skip_trivia();
        if self.byte(0) != Some(b'(') {
            return Ok(Expr::New {
                class,
                arguments: Vec::new(),
            });
        }
        self.bump(1);
        let arguments = self.arguments()?;
        self.member_calls(Expr::New { class, arguments })
    }

    /// `receiver->m(args)`, `receiver->p` and the chain of them, php's
    /// `callable_variable T_OBJECT_OPERATOR property_name argument_list`
    /// and `T_OBJECT_OPERATOR property_name` with no list after it, each
    /// member the receiver of the next. A property named by what is no
    /// label, `$o->$p` and `$o->{'p'}`, is not read yet; `?->` is left
    /// where it stands.
    fn member_calls(&mut self, mut receiver: Expr) -> Result<Expr, Trouble> {
        loop {
            self.skip_trivia();
            if !(self.byte(0) == Some(b'-') && self.byte(1) == Some(b'>')) {
                return Ok(receiver);
            }
            self.bump(2);
            self.skip_trivia();
            let member = self.label();
            if member.is_empty() {
                return Err(
                    self.trouble("a property named at run time this compiler cannot read yet")
                );
            }
            self.skip_trivia();
            if self.byte(0) != Some(b'(') {
                receiver = Expr::Prop {
                    object: Child::new(receiver),
                    name: member,
                };
                continue;
            }
            self.bump(1);
            let arguments = self.arguments()?;
            receiver = Expr::MethodCall {
                object: Child::new(receiver),
                method: member,
                arguments,
            };
        }
    }

    /// Whether a name, php's `T_STRING` or `T_NAME_*`, starts here.
    fn name_ahead(&self) -> bool {
        self.byte(0)
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'\\')
    }

    /// php's `LABEL`, one identifier with no namespace separator in it:
    /// a member's name, which `identifier` lets a reserved word be.
    fn label(&mut self) -> Vec<u8> {
        let start = self.at;
        while self
            .byte(0)
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
        {
            self.bump(1);
        }
        self.taken(start)
    }

    /// A variable, and php's `variable T_INC` or `variable T_DEC` after
    /// it: `$a++` is one operand, so `-$a++` is php's `-($a++)`. A `++`
    /// or `--` after that is php's parse error, and is refused by
    /// whatever reads on.
    fn incremented(&mut self) -> Result<Expr, Trouble> {
        let var = self.variable()?;
        self.skip_trivia();
        let step = match (self.byte(0), self.byte(1)) {
            (Some(b'+'), Some(b'+')) => Expr::PostInc,
            (Some(b'-'), Some(b'-')) => Expr::PostDec,
            _ => return Ok(var),
        };
        let Expr::Var(name) = var else {
            return Ok(var);
        };
        self.bump(2);
        Ok(step(name))
    }

    /// `$` and php's `LABEL`: a letter, `_` or a byte past ASCII, then
    /// those and digits. `$this` is [`Expr::This`], which php fetches by
    /// `FETCH_THIS` (`is_this_fetch`). What else `zend_try_compile_cv`
    /// refuses is refused here too: `$_GET` and its kin, which it fetches
    /// by `FETCH_R`; and `$$a` and `${`, which name a variable at runtime.
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
        if name == b"this" {
            return Ok(Expr::This);
        }
        if is_auto_global(&name) {
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
                        // `\101`: up to three octal digits, read as php's
                        // `(char) ZEND_STRTOL(octal_buf, NULL, 8)`, so the
                        // byte is the low eight bits of what they spell, and
                        // the scanner's warning for one past `\377` is the
                        // lexer's, on `Parsed::diagnostics`.
                        Some(b'0'..=b'7') => {
                            let mut digits = 1;
                            while digits < 3 && matches!(self.byte(1 + digits), Some(b'0'..=b'7')) {
                                digits += 1;
                            }
                            let value = self
                                .taken_ahead(1, digits)
                                .iter()
                                .fold(0u8, |n, d| n.wrapping_mul(8).wrapping_add(d - b'0'));
                            bytes.push(value);
                            self.bump(1 + digits);
                            continue;
                        }
                        // `\x41`: one or two hex digits; `\x` before anything
                        // else is left as written, as php's scanner leaves it.
                        Some(b'x') if self.byte(2).is_some_and(|b| b.is_ascii_hexdigit()) => {
                            let digits = if self.byte(3).is_some_and(|b| b.is_ascii_hexdigit()) {
                                2
                            } else {
                                1
                            };
                            let value = self.taken_ahead(2, digits).iter().fold(0u32, |n, d| {
                                n * 16 + char::from(*d).to_digit(16).unwrap_or(0)
                            });
                            bytes.push(u8::try_from(value).unwrap_or(u8::MAX));
                            self.bump(2 + digits);
                            continue;
                        }
                        // `\u{1F600}` is php's other escape; refused rather
                        // than read as the backslash php does not leave
                        // there.
                        Some(b'u') => {
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
                    // A doc comment is `/**` and whitespace, php's
                    // scanner pattern `"/**"{WHITESPACE}`, so `/**/` and
                    // `/**x*/` are plain comments.
                    let start = self.at;
                    let is_doc = self.byte(2) == Some(b'*')
                        && self
                            .byte(3)
                            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'));
                    self.bump(2);
                    while self.at < self.source.len() && !self.starts_with(b"*/") {
                        self.bump(1);
                    }
                    self.bump(2.min(self.source.len() - self.at));
                    if is_doc {
                        self.doc_comment = Some(self.taken(start));
                    }
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

/// `zend_is_reserved_class_name`: the names `reserved_class_names` lists
/// in `zend_compile.c`, which no class may take, whatever the case.
fn is_reserved_class_name(name: &[u8]) -> bool {
    matches!(
        name.to_ascii_lowercase().as_slice(),
        b"bool"
            | b"false"
            | b"float"
            | b"int"
            | b"null"
            | b"parent"
            | b"self"
            | b"static"
            | b"string"
            | b"true"
            | b"void"
            | b"never"
            | b"iterable"
            | b"object"
            | b"mixed"
    )
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
    use super::{BinaryOp, CastTo, Child, Expr, Level, Read, Stmt, Trouble};

    /// The reader on a thread nothing holds to a limit.
    fn read(source: &[u8]) -> Result<Read, Trouble> {
        super::read(source, crate::StackLimit::UNCHECKED)
    }

    #[test]
    fn an_echo_of_one_number_is_one_statement() {
        let read = read(b"<?php echo 1;").unwrap_or_default();
        assert_eq!(read, vec![(Stmt::Echo(vec![Expr::Int(1)]), 1)]);
    }

    /// `clone $a ** 2` is php's `(clone $a) ** 2`, the construct binding
    /// tighter than the operator, and the tree is the call php's parser
    /// makes of it.
    #[test]
    fn a_clone_is_the_fully_qualified_call_php_s_parser_makes_of_it() {
        let read = read(b"<?php echo clone $a ** 2;").unwrap_or_default();
        assert_eq!(
            read,
            vec![(
                Stmt::Echo(vec![Expr::Binary {
                    op: BinaryOp::Pow,
                    left: Child::new(Expr::Call {
                        name: b"\\clone".to_vec(),
                        arguments: vec![Expr::Var(b"a".to_vec())],
                    }),
                    right: Child::new(Expr::Int(2)),
                }]),
                1
            )]
        );
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
                    left: Child::new(Expr::Int(1)),
                    right: Child::new(Expr::Binary {
                        op: BinaryOp::Mul,
                        left: Child::new(Expr::Int(2)),
                        right: Child::new(Expr::Int(3)),
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
                    left: Child::new(Expr::Binary {
                        op: BinaryOp::Sub,
                        left: Child::new(Expr::Int(1)),
                        right: Child::new(Expr::Int(2)),
                    }),
                    right: Child::new(Expr::Int(3)),
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
                    left: Child::new(Expr::Binary {
                        op: BinaryOp::Add,
                        left: Child::new(Expr::Int(1)),
                        right: Child::new(Expr::Int(2)),
                    }),
                    right: Child::new(Expr::Int(3)),
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
                        operand: Child::new(Expr::Float(2.5)),
                    },
                    Expr::Unary {
                        minus: true,
                        operand: Child::new(Expr::Constant(b"INF".to_vec())),
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
                    left: Child::new(Expr::Str(b"a".to_vec())),
                    right: Child::new(Expr::Str(b"b".to_vec())),
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
                    left: Child::new(Expr::Int(8)),
                    right: Child::new(Expr::Int(1)),
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
                    left: Child::new(Expr::Int(2)),
                    right: Child::new(Expr::Unary {
                        minus: true,
                        operand: Child::new(Expr::Int(1)),
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
                        value: Child::new(Expr::Int(0)),
                    }],
                    cond: vec![Expr::Binary {
                        op: BinaryOp::IsSmaller,
                        left: Child::new(Expr::Var(b"i".to_vec())),
                        right: Child::new(Expr::Int(1000)),
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
                    left: Child::new(Expr::PostInc(b"a".to_vec())),
                    right: Child::new(Expr::Var(b"b".to_vec())),
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
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    /// `--` at the level `++` is read: the variable and its `--` are one
    /// operand, and php's scanner takes `--` before `-`, so `$a---$b` is
    /// `($a--) - $b` and `---$a` is `T_DEC - $a`, a parse error.
    #[test]
    fn a_decrement_takes_a_variable_and_nothing_else() {
        assert_eq!(
            shape(b"<?php echo -$a-- - --$b ** 2;"),
            shape(b"<?php echo (-($a--)) - ((--$b) ** 2);"),
            "the variable and its `--` are one operand"
        );
        assert_eq!(
            read(b"<?php $a---$b;").unwrap_or_default(),
            vec![(
                Stmt::Effect(Expr::Binary {
                    op: BinaryOp::Sub,
                    left: Child::new(Expr::PostDec(b"a".to_vec())),
                    right: Child::new(Expr::Var(b"b".to_vec())),
                }),
                1
            )],
            "php's scanner takes `--` before `-`"
        );
        assert_eq!(
            read(b"<?php $a = --$b;").unwrap_or_default(),
            vec![(
                Stmt::Effect(Expr::Assign {
                    var: b"a".to_vec(),
                    value: Child::new(Expr::PreDec(b"b".to_vec())),
                }),
                1
            )],
        );
        for source in [
            b"<?php echo 1--;".as_slice(),
            b"<?php echo --1;".as_slice(),
            b"<?php echo $a----;".as_slice(),
            b"<?php echo --$a--;".as_slice(),
            b"<?php echo --f();".as_slice(),
            b"<?php echo ---$a;".as_slice(),
        ] {
            assert!(read(source).is_err(), "{}", String::from_utf8_lossy(source));
        }
    }

    /// `variable '=' ampersand variable`: the target and the source as
    /// names, the `&` spaced either way, and the source a variable and
    /// nothing else.
    #[test]
    fn a_by_reference_assignment_reads_its_two_names() {
        for source in [
            b"<?php $b = &$a;".as_slice(),
            b"<?php $b =& $a;".as_slice(),
            b"<?php $b = & $a;".as_slice(),
        ] {
            assert_eq!(
                read(source).unwrap_or_default(),
                vec![(
                    Stmt::Effect(Expr::AssignRef {
                        var: b"b".to_vec(),
                        source: b"a".to_vec(),
                    }),
                    1
                )],
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for source in [
            b"<?php $b = &$a = 1;".as_slice(),
            b"<?php $b = &1;".as_slice(),
            b"<?php $b = &$this;".as_slice(),
            b"<?php $b = &$_GET;".as_slice(),
            b"<?php $b = &$a & 1;".as_slice(),
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
                    value: Child::new(Expr::Binary {
                        op: BinaryOp::Add,
                        left: Child::new(Expr::Int(1)),
                        right: Child::new(Expr::Var(b"b".to_vec())),
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
                        value: Child::new(Expr::Assign {
                            var: b"c".to_vec(),
                            value: Child::new(Expr::Call {
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
        assert_eq!(
            read(b"<?php echo $this;").unwrap_or_default(),
            vec![(Stmt::Echo(vec![Expr::This]), 1)],
            "$this is the one such variable read, as its own expression"
        );
        for source in [
            b"<?php $this = 1;".as_slice(),
            b"<?php unset($this);".as_slice(),
            b"<?php isset($a);".as_slice(),
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
        assert!(read(b"<?php echo \"\\u{1F600}\";").is_err());
    }

    /// php's scanner over `"…"`: an octal escape of up to three digits
    /// is the low eight bits of the number they spell, a hex escape of
    /// one or two digits the byte, and `\x` before anything else stands
    /// as written.
    #[test]
    fn octal_and_hex_escapes_are_the_bytes_php_reads() {
        assert_eq!(
            read(b"<?php echo \"\\101\\x41\\7\\777\\400\\xg\\x4\\1234\";").unwrap_or_default(),
            vec![(
                Stmt::Echo(vec![Expr::Str(b"AA\x07\xff\x00\\xg\x04S4".to_vec())]),
                1
            )]
        );
    }

    /// The cast spellings php's scanner takes, whatever their case and
    /// with blanks inside the parentheses, each to its type; `(unset)` is
    /// refused as php's compile error, and a parenthesis that opens no
    /// cast opens an expression.
    #[test]
    fn a_cast_is_read_at_unary_precedence() {
        let cast = |to: CastTo, operand: Expr| Expr::Cast {
            to,
            operand: Child::new(operand),
        };
        assert_eq!(
            read(b"<?php (int)$a; ( Integer )$a; (FLOAT)$a; (double)$a; (string)$a; (binary)$a; (array)$a; (OBJECT)$a; (bool)$a; (boolean)$a;")
                .unwrap_or_default()
                .into_iter()
                .map(|(stmt, _)| stmt)
                .collect::<Vec<_>>(),
            [
                CastTo::Long,
                CastTo::Long,
                CastTo::Double,
                CastTo::Double,
                CastTo::Str,
                CastTo::Str,
                CastTo::Array,
                CastTo::Object,
                CastTo::Bool,
                CastTo::Bool
            ]
            .into_iter()
            .map(|to| Stmt::Effect(cast(to, Expr::Var(b"a".to_vec()))))
            .collect::<Vec<_>>()
        );
        // `(int)$a ** 2` is `(int)($a ** 2)`, as `-$a ** 2` is `-($a ** 2)`;
        // `(int)$a + 1` casts `$a` alone.
        assert_eq!(
            read(b"<?php (int)$a ** 2; (int)$a + 1;").unwrap_or_default(),
            vec![
                (
                    Stmt::Effect(cast(
                        CastTo::Long,
                        Expr::Binary {
                            op: BinaryOp::Pow,
                            left: Child::new(Expr::Var(b"a".to_vec())),
                            right: Child::new(Expr::Int(2)),
                        }
                    )),
                    1
                ),
                (
                    Stmt::Effect(Expr::Binary {
                        op: BinaryOp::Add,
                        left: Child::new(cast(CastTo::Long, Expr::Var(b"a".to_vec()))),
                        right: Child::new(Expr::Int(1)),
                    }),
                    1
                ),
            ]
        );
        assert_eq!(
            read(b"<?php (1 + 2);").unwrap_or_default(),
            vec![(
                Stmt::Effect(Expr::Binary {
                    op: BinaryOp::Add,
                    left: Child::new(Expr::Int(1)),
                    right: Child::new(Expr::Int(2)),
                }),
                1
            )]
        );
        let unset = read(b"<?php\n(unset)$a;").err();
        assert_eq!(
            unset.as_ref().map(|t| (t.level, t.what.as_str(), t.line)),
            Some((
                Level::CompileError,
                "The (unset) cast is no longer supported",
                2
            ))
        );
    }

    #[test]
    fn a_close_tag_eats_one_newline_however_it_is_written() {
        assert_eq!(
            read(b"<?php echo 1; ?>\r\nafter").unwrap_or_default().len(),
            2,
            "the echo and the text after it, with the newline eaten"
        );
        assert_eq!(
            read(b"<?php echo 1; ?>\r\nafter").unwrap_or_default().pop(),
            Some((Stmt::Inline(b"after".to_vec()), 2))
        );
    }
}
