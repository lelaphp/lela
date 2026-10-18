//! `zend_ast_export` (`Z/Zend/zend_ast.c:1475-2951`): the source text php
//! writes back from its syntax tree, which is what `zend_compile_assert`
//! hands `assert()` as its description where none is written. php prints
//! its tree and not the file, so the text is the tree's: no comment and
//! no whitespace of the file's, every parenthesis re-derived from the
//! operators' priorities, a number as its value, a string in the quotes
//! it needs, a name as written, `and` as `&&`, `<>` as `!=`, `isset($a,
//! $b)` as the two `isset` php's parser makes of it, and a statement
//! list at four spaces an indent.
//!
//! This tree is not php's, so each node is read as the one php's parser
//! would have built: a parenthesis is no node, `'a' . 'b'` and a string
//! with nothing interpolated are the one value the parser folded them to
//! ([`Emit::written`]), `clone` and `exit` are the calls to `\clone` and
//! `\exit` they are in 8.5, and a `global`, `static`, `echo` or `unset`
//! of several is that many statements.
//!
//! ```php
//! function t($f) { try { $f(); } catch (AssertionError $e) { echo $e->getMessage(), "\n"; } }
//! t(fn () => assert(-1 > 0 and 'a' . 'b' === "c\n"));
//! t(fn () => assert((1 + 2) * 3 == 1 + (2 * 3) || 0x10 <> 16.0));
//! t(fn () => assert(isset($a, $b[0]) && !empty($c->d) ?: $x ?? (int) $y));
//! t(fn () => assert(false && ($a = function &(array &$a, ?X $b = null, int|float ...$c) use ($d, &$e): ?static {
//!     global $g, $$h;
//!     static $s, $t = 1;
//!     echo "{$a} $b[0] $b[k] $c->d ${e} x", 2;
//!     if ($a) { return 1; } elseif ($b) { yield $a => $b; } else if ($c) { goto end; } else { unset($a, $b); }
//!     end:
//!     foreach ($a as $k => &$v) { $v = clone $v; break; }
//!     for ($i = 0, $j = 1; $i < $j; $i++, --$j) { continue 1; }
//!     try { throw new E('x'); } catch (A|B $e) { print $e; } finally { exit; }
//!     switch ($a) { case 1: $x = [1, 'k' => [2], ...$z]; default: $y = match ($a) { 1, 2 => A::B, default => A::class }; }
//!     do { $o?->p($q, name: $r)::$s[1]->{'t'}; } while (0);
//!     $f = static fn (): int => $a <=> $b;
//! }));
//! ```
//! ```expect exit=255
//! Parse error: syntax error, unexpected token ";", expecting ")" in %s on line 18
//! ```

use php_heap::{Exponent, double_to_string};
use php_syntax::tree::{self, NodeId, Stmt, TokenId};
use php_syntax::{Holder, Parsed, SyntaxKind, TextRange, TokenValue};

use super::args::Arg;
use super::{Emit, Source};
use crate::CompileError;

/// php's `ZEND_ACC_*` visibility and the modifiers `zend_ast_export`
/// prints of a declaration, in the order it prints them.
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits as written
#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    visibility: Option<&'static str>,
    set_visibility: Option<&'static str>,
    is_static: bool,
    is_abstract: bool,
    is_final: bool,
    is_readonly: bool,
}

impl Flags {
    /// The modifiers as written, read as php's parser reads them: `var`
    /// is `public`, and a member with no visibility is given `public`
    /// where `public_by_default` says the parser gives it one
    /// (`method_modifiers`, `class_const_modifiers` and a property's
    /// `member_modifier` list).
    fn of(parsed: &Parsed, modifiers: &[tree::Modifier], public_by_default: bool) -> Flags {
        let mut flags = Flags::default();
        for modifier in modifiers {
            match *modifier {
                tree::Modifier::PublicKw(_) | tree::Modifier::VarKw(_) => {
                    flags.visibility = Some("public ");
                }
                tree::Modifier::ProtectedKw(_) => flags.visibility = Some("protected "),
                tree::Modifier::PrivateKw(_) => flags.visibility = Some("private "),
                tree::Modifier::StaticKw(_) => flags.is_static = true,
                tree::Modifier::AbstractKw(_) => flags.is_abstract = true,
                tree::Modifier::FinalKw(_) => flags.is_final = true,
                tree::Modifier::ReadonlyKw(_) => flags.is_readonly = true,
                tree::Modifier::SetVisibility(id) => {
                    flags.set_visibility = parsed.tree[id].vis.and_then(|vis| {
                        match parsed.tokens.get(vis.0 as usize).map(|token| token.kind) {
                            Some(SyntaxKind::PRIVATE_KW) => Some("private(set) "),
                            Some(SyntaxKind::PROTECTED_KW) => Some("protected(set) "),
                            Some(SyntaxKind::PUBLIC_KW) => Some("public(set) "),
                            _ => None,
                        }
                    });
                }
            }
        }
        if public_by_default && flags.visibility.is_none() {
            flags.visibility = Some("public ");
        }
        flags
    }

    /// `zend_ast_export_visibility`: the visibility, and the set
    /// visibility where `with_set` says the target takes one (a property
    /// or a promoted parameter).
    fn visibility(self, out: &mut Vec<u8>, with_set: bool) {
        if let Some(visibility) = self.visibility {
            out.extend(visibility.as_bytes());
        }
        if with_set && let Some(set) = self.set_visibility {
            out.extend(set.as_bytes());
        }
    }
}

/// php's `zend_ast_decl` of a function, a closure, an arrow function or
/// a method, the one arm of `zend_ast_export_ex` all four go through.
struct Decl<'t> {
    kind: DeclKind,
    attrs: &'t [NodeId<tree::AttributeGroup>],
    flags: Flags,
    by_ref: bool,
    name: &'t [u8],
    params: NodeId<tree::ParamList>,
    uses: Option<NodeId<tree::ClosureUseClause>>,
    ret: Option<NodeId<tree::RetType>>,
    body: DeclBody,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeclKind {
    Function,
    Closure,
    Arrow,
    Method,
}

#[derive(Clone, Copy)]
enum DeclBody {
    None,
    Block(NodeId<tree::Block>),
    Arrow(tree::Expr),
}

/// A class-like declaration as `zend_ast_export_ex`'s `ZEND_AST_CLASS`
/// arm reads it.
struct ClassLike<'t> {
    attrs: &'t [NodeId<tree::AttributeGroup>],
    keyword: &'static str,
    modifiers: Flags,
    name: &'t [u8],
    backing: Option<tree::TypeExpr>,
    extends: Option<&'t [TokenId]>,
    implements: Option<&'t [TokenId]>,
    body: NodeId<tree::ClassBody>,
}

/// One arm of an `if` chain, php's `ZEND_AST_IF_ELEM`: the condition,
/// and the body as one statement or the list of the alternative syntax.
type IfArm = (
    Option<tree::Expr>,
    Option<Stmt>,
    Option<NodeId<tree::StmtList>>,
);

/// A child php holds as a name where `zend_ast_export_var` and
/// `zend_ast_export_name` read one: the string a member is written as,
/// or an expression.
#[derive(Clone, Copy)]
enum Named<'t> {
    Bare(&'t [u8]),
    Node(tree::Expr),
}

/// The text of one token, as the scanner handed it over
/// ([`php_syntax::token_text`]).
fn text<'p>(parsed: &'p Parsed<'_>, id: TokenId) -> &'p [u8] {
    php_syntax::token_text(parsed, id)
}

/// The kind of one token.
fn kind(parsed: &Parsed, id: TokenId) -> SyntaxKind {
    parsed
        .tokens
        .get(id.0 as usize)
        .map_or(SyntaxKind::ERROR, |token| token.kind)
}

/// `zend_ast_export_indent`: four spaces a level.
fn indent_to(out: &mut Vec<u8>, indent: usize) {
    for _ in 0..indent {
        out.extend(b"    ");
    }
}

/// `zend_ast_export_str`: a `'…'` string's body, `'` and `\` escaped.
fn export_str(out: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        if c == b'\'' || c == b'\\' {
            out.push(b'\\');
        }
        out.push(c);
    }
}

/// `zend_ast_export_qstr`: a `"…"` or `` `…` `` string's body, the
/// control bytes as their escapes and the quote, `$` and `\` escaped.
fn export_qstr(out: &mut Vec<u8>, quote: u8, s: &[u8]) {
    for &c in s {
        if c < b' ' {
            match c {
                b'\n' => out.extend(b"\\n"),
                b'\r' => out.extend(b"\\r"),
                b'\t' => out.extend(b"\\t"),
                0x0c => out.extend(b"\\f"),
                0x0b => out.extend(b"\\v"),
                0x1b => out.extend(b"\\e"),
                _ => {
                    out.extend(b"\\0");
                    out.push(b'0' + c / 8);
                    out.push(b'0' + c % 8);
                }
            }
        } else {
            if c == quote || c == b'$' || c == b'\\' {
                out.push(b'\\');
            }
            out.push(c);
        }
    }
}

/// `zend_ast_export_quoted_str`: `'…'`, or `"…"` where a control byte
/// is in the string.
fn export_quoted_str(out: &mut Vec<u8>, s: &[u8]) {
    if s.iter().any(|c| *c < b' ') {
        out.push(b'"');
        export_qstr(out, b'"', s);
        out.push(b'"');
    } else {
        out.push(b'\'');
        export_str(out, s);
        out.push(b'\'');
    }
}

/// `zend_ast_valid_var_char`.
fn valid_var_char(c: u8) -> bool {
    c == b'_' || c >= 127 || c.is_ascii_alphanumeric()
}

/// `zend_ast_valid_var_name`.
fn valid_var_name(s: &[u8]) -> bool {
    match s.split_first() {
        Some((first, rest)) => {
            (*first == b'_' || *first >= 127 || first.is_ascii_alphabetic())
                && rest.iter().all(|c| valid_var_char(*c))
        }
        None => false,
    }
}

/// `smart_str_append_double` with `zero_fraction`: the double at the
/// compile-time precision, `.0` after one that prints as an integer.
fn export_double(out: &mut Vec<u8>, value: f64, precision: i32) {
    let text = double_to_string(
        value,
        if precision == 0 { 1 } else { precision },
        Exponent::Upper,
    );
    out.extend(text.as_bytes());
    if value.is_finite() && !text.contains(['.', 'e', 'E']) {
        out.extend(b".0");
    }
}

/// The operator of a binary node and its priorities, php's
/// `BINARY_OP(op, p, pl, pr)`.
fn binary_op(kind: SyntaxKind) -> Option<(&'static str, i32, i32, i32)> {
    Some(match kind {
        SyntaxKind::PLUS => (" + ", 200, 200, 201),
        SyntaxKind::MINUS => (" - ", 200, 200, 201),
        SyntaxKind::STAR => (" * ", 210, 210, 211),
        SyntaxKind::SLASH => (" / ", 210, 210, 211),
        SyntaxKind::PERCENT => (" % ", 210, 210, 211),
        SyntaxKind::SHL => (" << ", 190, 190, 191),
        SyntaxKind::SHR => (" >> ", 190, 190, 191),
        SyntaxKind::DOT => (" . ", 185, 185, 186),
        SyntaxKind::PIPE => (" | ", 140, 140, 141),
        SyntaxKind::AMP => (" & ", 160, 160, 161),
        SyntaxKind::CARET => (" ^ ", 150, 150, 151),
        SyntaxKind::EQ_EQ_EQ => (" === ", 170, 171, 171),
        SyntaxKind::BANG_EQ_EQ => (" !== ", 170, 171, 171),
        SyntaxKind::EQ_EQ => (" == ", 170, 171, 171),
        SyntaxKind::BANG_EQ | SyntaxKind::LT_GT => (" != ", 170, 171, 171),
        SyntaxKind::LT => (" < ", 180, 181, 181),
        SyntaxKind::LT_EQ => (" <= ", 180, 181, 181),
        SyntaxKind::STAR_STAR => (" ** ", 250, 251, 250),
        SyntaxKind::XOR_KW => (" xor ", 40, 40, 41),
        SyntaxKind::SPACESHIP => (" <=> ", 180, 181, 181),
        SyntaxKind::GT => (" > ", 180, 181, 181),
        SyntaxKind::GT_EQ => (" >= ", 180, 181, 181),
        SyntaxKind::AMP_AMP | SyntaxKind::AND_KW => (" && ", 130, 130, 131),
        SyntaxKind::PIPE_PIPE | SyntaxKind::OR_KW => (" || ", 120, 120, 121),
        SyntaxKind::PIPE_GT => (" |> ", 183, 183, 184),
        SyntaxKind::QUESTION_QUESTION => (" ?? ", 110, 111, 110),
        _ => return None,
    })
}

/// The operator of an assignment, all at php's `90, 91, 90`.
fn assign_op(kind: SyntaxKind, by_ref: bool) -> &'static str {
    match kind {
        SyntaxKind::EQ if by_ref => " =& ",
        SyntaxKind::PLUS_EQ => " += ",
        SyntaxKind::MINUS_EQ => " -= ",
        SyntaxKind::STAR_EQ => " *= ",
        SyntaxKind::SLASH_EQ => " /= ",
        SyntaxKind::PERCENT_EQ => " %= ",
        SyntaxKind::SHL_EQ => " <<= ",
        SyntaxKind::SHR_EQ => " >>= ",
        SyntaxKind::DOT_EQ => " .= ",
        SyntaxKind::PIPE_EQ => " |= ",
        SyntaxKind::AMP_EQ => " &= ",
        SyntaxKind::CARET_EQ => " ^= ",
        SyntaxKind::STAR_STAR_EQ => " **= ",
        SyntaxKind::QUESTION_QUESTION_EQ => " ??= ",
        _ => " = ",
    }
}

/// A cast's text as `ZEND_AST_CAST` prints it, from the word inside the
/// written parentheses.
fn cast(written: &[u8]) -> &'static str {
    let word: Vec<u8> = written
        .iter()
        .filter(|c| !matches!(c, b'(' | b')' | b' ' | b'\t'))
        .map(u8::to_ascii_lowercase)
        .collect();
    match word.as_slice() {
        b"int" | b"integer" => "(int)",
        b"bool" | b"boolean" => "(bool)",
        b"float" | b"double" | b"real" => "(float)",
        b"string" | b"binary" => "(string)",
        b"array" => "(array)",
        b"object" => "(object)",
        b"void" => "(void)",
        _ => "(unset)",
    }
}

/// Whether a name written as a type is one of the four php's parser
/// makes a `ZEND_AST_TYPE` of, which prints lowercased.
fn builtin_type(name: &[u8]) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_slice() {
        b"array" => Some("array"),
        b"callable" => Some("callable"),
        b"static" => Some("static"),
        b"mixed" => Some("mixed"),
        _ => None,
    }
}

impl<'a> Emit<'a> {
    /// `zend_ast_export("assert(", arg, ")")`: the description
    /// `zend_compile_assert` gives an assertion written without one, the
    /// argument as php prints it, with its name where it is named and
    /// its `...` where it is unpacked (`ZEND_AST_NAMED_ARG`,
    /// `ZEND_AST_UNPACK`). An argument the compiler made is none a
    /// program wrote, and has no text.
    ///
    /// ```php
    /// function t($f) { try { $f(); } catch (AssertionError $e) { echo $e->getMessage(), "\n"; } }
    /// t(fn () => assert(false && (#[A] static fn (): (X&Y)|null => $a?->b(name: 1, ...$c))));
    /// t(fn () => assert(false && [$a, [, 'k' => &$b]] = $c));
    /// t(fn () => assert(false && #[A(1)] static function () {}));
    /// t(fn () => assert(assertion: false && \A\B::c(...$d) && namespace\e()));
    /// t(fn () => assert(false && $f()() && A::$m() && $o->{'m' . 1}() && ('str' . 'len')('x')));
    /// ```
    /// ```expect
    /// assert(false && (#[A] static fn(): X&Y|null => $a?->b(name: 1, ...$c)))
    /// assert(false && ([$a, [, 'k' => &$b]] = $c))
    /// assert(false && #[A(1)] static function () {
    /// })
    /// assert(assertion: false && \A\B::c(...$d) && namespace\e())
    /// assert(false && $f()() && A::$m() && $o->m1() && \strlen('x'))
    /// ```
    ///
    /// An unpacked assertion is printed as written, and the description
    /// after it is the positional argument php refuses there:
    ///
    /// ```php
    /// assert(...[false]);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use positional argument after argument unpacking in %s on line 2
    /// ```
    pub(super) fn assertion_text(&mut self, only: &Arg) -> Result<Vec<u8>, CompileError> {
        let mut out = b"assert(".to_vec();
        let value = match only {
            Arg::Positional(value) => *value,
            Arg::Named(name, value) => {
                out.extend(name.iter());
                out.extend(b": ");
                *value
            }
            Arg::Unpack(value) => {
                out.extend(b"...");
                Source::Expr(*value)
            }
        };
        let Some(value) = value.tree() else {
            return Err(self.fatal("an assertion the compiler holds"));
        };
        self.export(&mut out, value, 0, 0)?;
        out.push(b')');
        Ok(out)
    }

    /// Whether a closure or an arrow function was written `static`: the
    /// first token after the attribute groups written before it.
    fn written_static(&self, range: TextRange, attrs: &[NodeId<tree::AttributeGroup>]) -> bool {
        let start = attrs
            .last()
            .map_or(range.start, |last| self.parsed.tree[*last].range.end());
        self.first_kind(TextRange::new(start, range.end())) == Some(SyntaxKind::STATIC_KW)
    }

    /// `zend_ast_export_ex` over one expression at `priority`.
    #[allow(clippy::too_many_lines)]
    fn export(
        &mut self,
        out: &mut Vec<u8>,
        expr: tree::Expr,
        priority: i32,
        indent: usize,
    ) -> Result<(), CompileError> {
        self.check_stack()?;
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        // An arrow function written in parentheses keeps them, the one
        // mark php's `'(' expr ')'` leaves (`ZEND_PARENTHESIZED_ARROW_FUNC`).
        let parenthesized = matches!(expr, tree::Expr::ParenExpr(_));
        let expr = self.peel(expr);
        match expr {
            tree::Expr::ParenExpr(_) => Ok(()),
            tree::Expr::LiteralExpr(_) | tree::Expr::NowdocExpr(_) => self.export_value(out, expr),
            tree::Expr::InterpolatedStringExpr(id) => {
                if self.is_written_value(expr) {
                    return self.export_value(out, expr);
                }
                out.push(b'"');
                self.export_encaps(
                    out,
                    b'"',
                    tree.list(tree[id].parts),
                    Holder::DoubleQuoted(&tree[id]),
                    indent,
                )?;
                out.push(b'"');
                Ok(())
            }
            tree::Expr::HeredocExpr(id) => {
                if self.is_written_value(expr) {
                    return self.export_value(out, expr);
                }
                out.push(b'"');
                self.export_encaps(
                    out,
                    b'"',
                    tree.list(tree[id].parts),
                    Holder::Heredoc(&tree[id]),
                    indent,
                )?;
                out.push(b'"');
                Ok(())
            }
            tree::Expr::ShellExecExpr(id) => {
                out.push(b'`');
                self.export_encaps(
                    out,
                    b'`',
                    tree.list(tree[id].parts),
                    Holder::ShellExec(&tree[id]),
                    indent,
                )?;
                out.push(b'`');
                Ok(())
            }
            tree::Expr::MagicConstExpr(id) => {
                if let Some(token) = tree[id].token {
                    out.extend(text(parsed, token).to_ascii_uppercase());
                }
                Ok(())
            }
            tree::Expr::SimpleVarExpr(id) => {
                if let Some(var) = tree[id].var {
                    out.extend(text(parsed, var));
                }
                Ok(())
            }
            tree::Expr::VarVarExpr(id) => {
                out.push(b'$');
                match tree[id].inner {
                    Some(inner) => self.export_var(out, Named::Node(inner), indent),
                    None => Ok(()),
                }
            }
            tree::Expr::DynamicVarExpr(id) => {
                out.push(b'$');
                match tree[id].inner {
                    Some(inner) => self.export_var(out, Named::Node(inner), indent),
                    None => Ok(()),
                }
            }
            tree::Expr::NameRef(token) => {
                out.extend(text(parsed, token));
                Ok(())
            }
            tree::Expr::ArrayExpr(id) => {
                self.export_array(out, tree.list(tree[id].array_elements), indent)
            }
            tree::Expr::ListExpr(id) => {
                self.export_array(out, tree.list(tree[id].array_elements), indent)
            }
            tree::Expr::UnaryExpr(id) => {
                let node = &tree[id];
                let op = match node.op.map(|op| kind(parsed, op)) {
                    Some(SyntaxKind::PLUS) => "+",
                    Some(SyntaxKind::MINUS) => "-",
                    Some(SyntaxKind::BANG) => "!",
                    _ => "~",
                };
                self.prefix(out, op, node.operand, priority, indent)
            }
            tree::Expr::PrefixIncDecExpr(id) => {
                let node = &tree[id];
                let op = if node.op.map(|op| kind(parsed, op)) == Some(SyntaxKind::PLUS_PLUS) {
                    "++"
                } else {
                    "--"
                };
                self.prefix(out, op, node.operand, priority, indent)
            }
            tree::Expr::PostfixIncDecExpr(id) => {
                let node = &tree[id];
                let op = if node.op.map(|op| kind(parsed, op)) == Some(SyntaxKind::PLUS_PLUS) {
                    "++"
                } else {
                    "--"
                };
                if priority > 240 {
                    out.push(b'(');
                }
                self.export(out, node.operand, 241, indent)?;
                out.extend(op.as_bytes());
                if priority > 240 {
                    out.push(b')');
                }
                Ok(())
            }
            tree::Expr::CastExpr(id) => {
                let node = &tree[id];
                let op = node.op.map_or("(unset)", |op| cast(text(parsed, op)));
                self.prefix(out, op, node.operand, priority, indent)
            }
            tree::Expr::ErrorSuppressExpr(id) => {
                self.prefix(out, "@", tree[id].operand, priority, indent)
            }
            tree::Expr::PrintExpr(id) => {
                if priority > 60 {
                    out.push(b'(');
                }
                out.extend(b"print ");
                if let Some(operand) = tree[id].operand {
                    self.export(out, operand, 61, indent)?;
                }
                if priority > 60 {
                    out.push(b')');
                }
                Ok(())
            }
            tree::Expr::ThrowExpr(id) => {
                out.extend(b"throw");
                if let Some(operand) = tree[id].operand {
                    out.push(b' ');
                    self.export(out, operand, 0, indent)?;
                }
                Ok(())
            }
            tree::Expr::YieldFromExpr(id) => {
                self.prefix(out, "yield from ", tree[id].source, priority, indent)
            }
            tree::Expr::YieldExpr(id) => {
                let node = &tree[id];
                if priority > 70 {
                    out.push(b'(');
                }
                out.extend(b"yield ");
                if let Some(value) = node.value {
                    if let Some(key) = node.key {
                        self.export(out, tree[key].key, 70, indent)?;
                        out.extend(b" => ");
                    }
                    self.export(out, value, 70, indent)?;
                }
                if priority > 70 {
                    out.push(b')');
                }
                Ok(())
            }
            tree::Expr::IncludeExpr(id) => {
                let node = &tree[id];
                let op = match node.op.map(|op| kind(parsed, op)) {
                    Some(SyntaxKind::INCLUDE_ONCE_KW) => "include_once",
                    Some(SyntaxKind::REQUIRE_KW) => "require",
                    Some(SyntaxKind::REQUIRE_ONCE_KW) => "require_once",
                    _ => "include",
                };
                self.func_op(out, op, node.operand, indent)
            }
            tree::Expr::EvalExpr(id) => self.func_op(out, "eval", tree[id].code, indent),
            tree::Expr::EmptyExpr(id) => self.func_op(out, "empty", tree[id].target, indent),
            // `isset_variables`: one `ZEND_AST_ISSET` a variable, joined
            // by `ZEND_AST_AND` from the left.
            tree::Expr::IssetExpr(id) => {
                let targets = tree.list(tree[id].targets);
                let joined = targets.len() > 1;
                if joined && priority > 130 {
                    out.push(b'(');
                }
                for (at, target) in targets.iter().enumerate() {
                    if at > 0 {
                        out.extend(b" && ");
                    }
                    self.func_op(out, "isset", Some(*target), indent)?;
                }
                if joined && priority > 130 {
                    out.push(b')');
                }
                Ok(())
            }
            tree::Expr::BinaryExpr(id) => {
                let node = &tree[id];
                if node.op.map(|op| kind(parsed, op)) == Some(SyntaxKind::DOT)
                    && self.written(expr)?.is_some()
                {
                    return self.export_value(out, expr);
                }
                let Some((op, p, pl, pr)) = node.op.and_then(|op| binary_op(kind(parsed, op)))
                else {
                    return Ok(());
                };
                self.export_binary(out, (node.lhs, node.rhs), op, (p, pl, pr), priority, indent)
            }
            tree::Expr::AssignExpr(id) => {
                let node = &tree[id];
                let op = node.op.map_or(" = ", |op| {
                    assign_op(kind(parsed, op), node.byref.is_some())
                });
                self.export_binary(
                    out,
                    (node.lhs, node.rhs),
                    op,
                    (90, 91, 90),
                    priority,
                    indent,
                )
            }
            tree::Expr::InstanceofExpr(id) => {
                let node = &tree[id];
                self.export(out, node.lhs, 0, indent)?;
                out.extend(b" instanceof ");
                match node.rhs {
                    Some(rhs) => self.export(out, rhs, 0, indent),
                    None => Ok(()),
                }
            }
            tree::Expr::TernaryExpr(id) => {
                let node = &tree[id];
                if priority > 100 {
                    out.push(b'(');
                }
                self.export(out, node.cond, 100, indent)?;
                match node.then {
                    Some(then) => {
                        out.extend(b" ? ");
                        self.export(out, then, 101, indent)?;
                        out.extend(b" : ");
                    }
                    None => out.extend(b" ?: "),
                }
                if let Some(else_branch) = node.else_branch {
                    self.export(out, else_branch, 101, indent)?;
                }
                if priority > 100 {
                    out.push(b')');
                }
                Ok(())
            }
            tree::Expr::MatchExpr(id) => {
                let node = &tree[id];
                out.extend(b"match (");
                if let Some(subject) = node.subject {
                    self.export(out, subject, 0, indent)?;
                }
                out.extend(b") {\n");
                for arm in self.parsed.tree.list(node.match_arms) {
                    let arm = &tree[*arm];
                    indent_to(out, indent + 1);
                    match arm.conds {
                        Some(conds) => {
                            self.export_list(out, tree.list(tree[conds].exprs), 0, indent + 1)?;
                            out.extend(b" => ");
                        }
                        None => out.extend(b"default => "),
                    }
                    if let Some(body) = arm.body {
                        self.export(out, body, 0, 0)?;
                    }
                    out.extend(b",\n");
                }
                indent_to(out, indent);
                out.push(b'}');
                Ok(())
            }
            tree::Expr::CloneExpr(id) => {
                let node = &tree[id];
                out.extend(b"\\clone(");
                match (node.args, node.operand) {
                    (Some(args), _) => self.export_args(out, args, indent)?,
                    (None, Some(operand)) => self.export(out, operand, 20, indent)?,
                    (None, None) => {}
                }
                out.push(b')');
                Ok(())
            }
            tree::Expr::ExitExpr(id) => {
                out.extend(b"\\exit(");
                if let Some(args) = tree[id].args {
                    self.export_args(out, args, indent)?;
                }
                out.push(b')');
                Ok(())
            }
            tree::Expr::NewExpr(id) => {
                let node = &tree[id];
                out.extend(b"new ");
                if let Some(anon) = node.anon {
                    let class = &tree[anon];
                    self.export_attributes(out, self.parsed.tree.list(class.attrs), indent, false)?;
                    out.extend(b"class");
                    if let Some(args) = class.args
                        && !tree[args].args.is_empty()
                    {
                        out.push(b'(');
                        self.export_args(out, args, indent)?;
                        out.push(b')');
                    }
                    let extends = class.extends.map(|e| tree.list(tree[e].types));
                    let implements = class.implements.map(|i| tree.list(tree[i].types));
                    return self
                        .export_class_no_header(out, extends, implements, class.body, indent);
                }
                if let Some(ctor) = node.ctor {
                    self.export(out, ctor, 0, indent)?;
                }
                out.push(b'(');
                if let Some(args) = node.args {
                    self.export_args(out, args, indent)?;
                }
                out.push(b')');
                Ok(())
            }
            tree::Expr::CallExpr(id) => {
                let node = &tree[id];
                let callee = self.peel(node.callee);
                let wrapped = matches!(
                    callee,
                    tree::Expr::ClosureExpr(_) | tree::Expr::ArrowFnExpr(_)
                );
                if wrapped {
                    out.push(b'(');
                }
                match callee {
                    tree::Expr::ScopedAccessExpr(scoped) => {
                        self.export_static_call(out, scoped, indent)?;
                    }
                    tree::Expr::MemberAccessExpr(access) => {
                        let access = &tree[access];
                        self.export(out, access.obj, 0, indent)?;
                        out.extend(self.arrow(access.op));
                        self.export_member(out, access.member, true, indent)?;
                    }
                    // `zend_ast_export_ns_name` over a string the parser
                    // made a `ZEND_AST_ZVAL` of, whose `attr` is
                    // `ZEND_NAME_FQ`: `'strlen'('x')` is `\strlen('x')`.
                    _ => {
                        let value = self.written(callee)?;
                        match value.as_ref().and_then(|value| value.as_str()) {
                            Some(name) => {
                                out.push(b'\\');
                                out.extend(name.bytes());
                            }
                            None => self.export(out, callee, 0, indent)?,
                        }
                    }
                }
                if wrapped {
                    out.push(b')');
                }
                out.push(b'(');
                self.export_args(out, node.args, indent)?;
                out.push(b')');
                Ok(())
            }
            tree::Expr::MemberAccessExpr(id) => {
                let node = &tree[id];
                self.export(out, node.obj, 0, indent)?;
                out.extend(self.arrow(node.op));
                self.export_member(out, node.member, false, indent)
            }
            tree::Expr::ScopedAccessExpr(id) => {
                let node = &tree[id];
                self.export(out, node.scope, 0, indent)?;
                match node.member {
                    Some(tree::ScopedSelector::ClassKw(_)) => {
                        out.extend(b"::class");
                        Ok(())
                    }
                    Some(tree::ScopedSelector::MemberName(name)) => {
                        out.extend(b"::");
                        if let Some(name) = tree[name].name {
                            out.extend(text(parsed, name));
                        }
                        Ok(())
                    }
                    Some(tree::ScopedSelector::BracedMember(braced)) => {
                        out.extend(b"::");
                        match tree[braced].inner {
                            Some(inner) => self.export_name(out, Named::Node(inner), indent),
                            None => Ok(()),
                        }
                    }
                    // `static_member`: `simple_variable` as it stands,
                    // after `::$`.
                    Some(tree::ScopedSelector::SimpleVarExpr(var)) => {
                        out.extend(b"::$");
                        let name = tree[var].var.map_or(&[][..], |var| text(parsed, var));
                        let name = name.strip_prefix(b"$").unwrap_or(name);
                        self.export_var(out, Named::Bare(name), indent)
                    }
                    Some(tree::ScopedSelector::VarVarExpr(var)) => {
                        out.extend(b"::$");
                        match tree[var].inner {
                            Some(inner) => self.export_var(out, Named::Node(inner), indent),
                            None => Ok(()),
                        }
                    }
                    Some(tree::ScopedSelector::DynamicVarExpr(var)) => {
                        out.extend(b"::$");
                        match tree[var].inner {
                            Some(inner) => self.export_var(out, Named::Node(inner), indent),
                            None => Ok(()),
                        }
                    }
                    None => Ok(()),
                }
            }
            tree::Expr::SubscriptExpr(id) => {
                let node = &tree[id];
                self.export(out, node.base, 260, indent)?;
                out.push(b'[');
                if let Some(index) = node.index {
                    self.export(out, index, 0, indent)?;
                }
                out.push(b']');
                Ok(())
            }
            tree::Expr::ClosureExpr(id) => {
                let node = &tree[id];
                let is_static = self.written_static(node.range, self.parsed.tree.list(node.attrs));
                let decl = Decl {
                    kind: DeclKind::Closure,
                    attrs: self.parsed.tree.list(node.attrs),
                    flags: Flags {
                        is_static,
                        ..Flags::default()
                    },
                    by_ref: node.byref.is_some(),
                    name: &[],
                    params: node.params,
                    uses: node.uses,
                    ret: node.ret,
                    body: node.body.map_or(DeclBody::None, DeclBody::Block),
                };
                self.export_decl(out, &decl, false, indent)
            }
            tree::Expr::ArrowFnExpr(id) => {
                let node = &tree[id];
                let is_static = self.written_static(node.range, self.parsed.tree.list(node.attrs));
                let decl = Decl {
                    kind: DeclKind::Arrow,
                    attrs: self.parsed.tree.list(node.attrs),
                    flags: Flags {
                        is_static,
                        ..Flags::default()
                    },
                    by_ref: node.byref.is_some(),
                    name: &[],
                    params: node.params,
                    uses: None,
                    ret: node.ret,
                    body: node.body.map_or(DeclBody::None, DeclBody::Arrow),
                };
                self.export_decl(out, &decl, parenthesized, indent)
            }
        }
    }

    /// A value php's parser made a `ZEND_AST_ZVAL` of, printed by
    /// `zend_ast_export_zval`.
    fn export_value(&mut self, out: &mut Vec<u8>, expr: tree::Expr) -> Result<(), CompileError> {
        let Some(value) = self.written(expr)? else {
            return Ok(());
        };
        if let Some(n) = value.as_long() {
            out.extend(n.to_string().as_bytes());
        } else if let Some(d) = value.as_double() {
            export_double(out, d, self.precision);
        } else if let Some(s) = value.as_str() {
            export_quoted_str(out, s.bytes());
        }
        Ok(())
    }

    /// `PREFIX_OP`.
    fn prefix(
        &mut self,
        out: &mut Vec<u8>,
        op: &str,
        operand: Option<tree::Expr>,
        priority: i32,
        indent: usize,
    ) -> Result<(), CompileError> {
        let (p, pl) = match op {
            "print " => (60, 61),
            "yield from " => (85, 86),
            _ => (240, 241),
        };
        if priority > p {
            out.push(b'(');
        }
        out.extend(op.as_bytes());
        if let Some(operand) = operand {
            self.export(out, operand, pl, indent)?;
        }
        if priority > p {
            out.push(b')');
        }
        Ok(())
    }

    /// `BINARY_OP`.
    fn export_binary(
        &mut self,
        out: &mut Vec<u8>,
        (lhs, rhs): (tree::Expr, Option<tree::Expr>),
        op: &str,
        (p, pl, pr): (i32, i32, i32),
        priority: i32,
        indent: usize,
    ) -> Result<(), CompileError> {
        if priority > p {
            out.push(b'(');
        }
        self.export(out, lhs, pl, indent)?;
        out.extend(op.as_bytes());
        if let Some(rhs) = rhs {
            self.export(out, rhs, pr, indent)?;
        }
        if priority > p {
            out.push(b')');
        }
        Ok(())
    }

    /// `FUNC_OP`.
    fn func_op(
        &mut self,
        out: &mut Vec<u8>,
        op: &str,
        operand: Option<tree::Expr>,
        indent: usize,
    ) -> Result<(), CompileError> {
        out.extend(op.as_bytes());
        out.push(b'(');
        if let Some(operand) = operand {
            self.export(out, operand, 0, indent)?;
        }
        out.push(b')');
        Ok(())
    }

    /// `->` or `?->`.
    fn arrow(&self, op: Option<TokenId>) -> &'static [u8] {
        if op.map(|op| kind(self.parsed, op)) == Some(SyntaxKind::NULLSAFE_ARROW) {
            b"?->"
        } else {
            b"->"
        }
    }

    /// A member after `->`, php's `property_name` or `member_name`: a
    /// name as it is written, a variable wrapped in the `ZEND_AST_VAR`
    /// the parser gives a `simple_variable` there, a braced expression as
    /// it stands; `zend_ast_export_var` over it.
    fn export_member(
        &mut self,
        out: &mut Vec<u8>,
        member: Option<tree::MemberSelector>,
        _call: bool,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        match member {
            Some(tree::MemberSelector::MemberName(name)) => {
                let name = tree[name].name.map_or(&[][..], |name| text(parsed, name));
                self.export_var(out, Named::Bare(name), indent)
            }
            Some(tree::MemberSelector::SimpleVarExpr(var)) => {
                self.export(out, tree::Expr::SimpleVarExpr(var), 0, indent)
            }
            Some(tree::MemberSelector::VarVarExpr(var)) => {
                self.export(out, tree::Expr::VarVarExpr(var), 0, indent)
            }
            Some(tree::MemberSelector::DynamicVarExpr(var)) => {
                self.export(out, tree::Expr::DynamicVarExpr(var), 0, indent)
            }
            Some(tree::MemberSelector::BracedMember(braced)) => match tree[braced].inner {
                Some(inner) => self.export_var(out, Named::Node(inner), indent),
                None => Ok(()),
            },
            None => Ok(()),
        }
    }

    /// `ZEND_AST_STATIC_CALL`'s class and method: the class as a name,
    /// `::`, and the method through `zend_ast_export_var`, a variable
    /// wrapped in the `ZEND_AST_VAR` `member_name` gives it.
    fn export_static_call(
        &mut self,
        out: &mut Vec<u8>,
        scoped: NodeId<tree::ScopedAccessExpr>,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        let node = &tree[scoped];
        self.export(out, node.scope, 0, indent)?;
        out.extend(b"::");
        match node.member {
            Some(tree::ScopedSelector::MemberName(name)) => {
                let name = tree[name].name.map_or(&[][..], |name| text(parsed, name));
                self.export_var(out, Named::Bare(name), indent)
            }
            Some(tree::ScopedSelector::SimpleVarExpr(var)) => {
                self.export(out, tree::Expr::SimpleVarExpr(var), 0, indent)
            }
            Some(tree::ScopedSelector::VarVarExpr(var)) => {
                self.export(out, tree::Expr::VarVarExpr(var), 0, indent)
            }
            Some(tree::ScopedSelector::DynamicVarExpr(var)) => {
                self.export(out, tree::Expr::DynamicVarExpr(var), 0, indent)
            }
            Some(tree::ScopedSelector::BracedMember(braced)) => match tree[braced].inner {
                Some(inner) => self.export_var(out, Named::Node(inner), indent),
                None => Ok(()),
            },
            Some(tree::ScopedSelector::ClassKw(_)) => {
                out.extend(b"class");
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Whether php's node for an expression is `ZEND_AST_VAR`.
    fn is_var(&self, expr: tree::Expr) -> bool {
        matches!(
            self.peel(expr),
            tree::Expr::SimpleVarExpr(_)
                | tree::Expr::VarVarExpr(_)
                | tree::Expr::DynamicVarExpr(_)
        )
    }

    /// `zend_ast_export_var`: a string that is a valid variable name as
    /// it stands, a `ZEND_AST_VAR` as itself, and anything else in
    /// braces through `zend_ast_export_name`.
    fn export_var(
        &mut self,
        out: &mut Vec<u8>,
        named: Named<'_>,
        indent: usize,
    ) -> Result<(), CompileError> {
        match named {
            Named::Bare(name) if valid_var_name(name) => {
                out.extend(name);
                return Ok(());
            }
            Named::Node(expr) => {
                if let Some(value) = self.written(expr)?
                    && let Some(s) = value.as_str()
                    && valid_var_name(s.bytes())
                {
                    out.extend(s.bytes());
                    return Ok(());
                }
                if self.is_var(expr) {
                    return self.export(out, expr, 0, indent);
                }
            }
            Named::Bare(_) => {}
        }
        out.push(b'{');
        self.export_name(out, named, indent)?;
        out.push(b'}');
        Ok(())
    }

    /// `zend_ast_export_name`: a string as its bytes, anything else as
    /// the expression it is.
    fn export_name(
        &mut self,
        out: &mut Vec<u8>,
        named: Named<'_>,
        indent: usize,
    ) -> Result<(), CompileError> {
        match named {
            Named::Bare(name) => {
                out.extend(name);
                Ok(())
            }
            Named::Node(expr) => {
                if let Some(value) = self.written(expr)?
                    && let Some(s) = value.as_str()
                {
                    out.extend(s.bytes());
                    return Ok(());
                }
                self.export(out, expr, 0, indent)
            }
        }
    }

    /// `zend_ast_export_list` at `priority`, `, ` between.
    fn export_list(
        &mut self,
        out: &mut Vec<u8>,
        exprs: &[tree::Expr],
        priority: i32,
        indent: usize,
    ) -> Result<(), CompileError> {
        for (at, expr) in exprs.iter().enumerate() {
            if at > 0 {
                out.extend(b", ");
            }
            self.export(out, *expr, priority, indent)?;
        }
        Ok(())
    }

    /// `ZEND_AST_ARG_LIST`, or the `...` of `ZEND_AST_CALLABLE_CONVERT`.
    fn export_args(
        &mut self,
        out: &mut Vec<u8>,
        args: NodeId<tree::ArgList>,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        for (at, arg) in tree.list(tree[args].args).iter().enumerate() {
            if at > 0 {
                out.extend(b", ");
            }
            match *arg {
                tree::Arg::Expr(expr) => self.export(out, expr, 20, indent)?,
                tree::Arg::NamedArg(named) => {
                    let named = &tree[named];
                    out.extend(text(parsed, named.name));
                    out.extend(b": ");
                    if let Some(value) = named.value {
                        self.export(out, value, 0, indent)?;
                    }
                }
                tree::Arg::SpreadArg(spread) => {
                    out.extend(b"...");
                    if let Some(value) = tree[spread].value {
                        self.export(out, value, 0, indent)?;
                    }
                }
                tree::Arg::VariadicPlaceholder(_) => out.extend(b"..."),
            }
        }
        Ok(())
    }

    /// `ZEND_AST_ARRAY` of `ZEND_AST_ARRAY_ELEM`s, `[…]` whatever was
    /// written; an element left out stands as nothing between commas.
    fn export_array(
        &mut self,
        out: &mut Vec<u8>,
        elements: &[NodeId<tree::ArrayElement>],
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        out.push(b'[');
        for (at, element) in elements.iter().enumerate() {
            if at > 0 {
                out.extend(b", ");
            }
            let element = &tree[*element];
            if element.spread.is_some() {
                out.extend(b"...");
                if let Some(value) = element.value {
                    self.export(out, value, 0, indent)?;
                }
                continue;
            }
            if let Some(key) = element.key {
                self.export(out, tree[key].key, 80, indent)?;
                out.extend(b" => ");
            }
            if element.byref.is_some() {
                out.push(b'&');
            }
            if let Some(value) = element.value {
                self.export(out, value, 80, indent)?;
            }
        }
        out.push(b']');
        Ok(())
    }

    /// `zend_ast_export_encaps_list`: the written bytes escaped for
    /// `quote`, a variable bare where what follows cannot be read as part
    /// of its name, and every other part in braces.
    fn export_encaps(
        &mut self,
        out: &mut Vec<u8>,
        quote: u8,
        parts: &[tree::StringPart],
        holder: Holder<'_>,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        // php's list: the written runs, each one value, and the parts.
        let mut list: Vec<Result<Vec<u8>, tree::StringPart>> = Vec::new();
        for part in parts {
            if let tree::StringPart::LiteralStringPart(id) = *part {
                let bytes = match tree[id]
                    .text
                    .and_then(|t| TokenValue::of(parsed, holder, t))
                {
                    Some(TokenValue::Bytes(bytes)) => bytes,
                    _ => Vec::new(),
                };
                if let Some(Ok(run)) = list.last_mut() {
                    run.extend(bytes);
                } else {
                    list.push(Ok(bytes));
                }
            } else {
                list.push(Err(*part));
            }
        }
        for (at, piece) in list.iter().enumerate() {
            let part = match piece {
                Ok(bytes) => {
                    export_qstr(out, quote, bytes);
                    continue;
                }
                Err(part) => *part,
            };
            let next_needs_braces = match list.get(at + 1) {
                Some(Ok(bytes)) => bytes
                    .first()
                    .is_some_and(|c| *c == b'[' || valid_var_char(*c)),
                _ => false,
            };
            // `${'…'}`: a `ZEND_AST_VAR` over the value written, bare as
            // a name is.
            let written_var = match part {
                tree::StringPart::DollarBraceInterpolation(id) if tree[id].name.is_none() => {
                    match tree[id].inner {
                        Some(inner) => self.written(inner)?.is_some(),
                        None => false,
                    }
                }
                _ => false,
            };
            if written_var && !next_needs_braces {
                self.export_part(out, part, indent)?;
                continue;
            }
            match part {
                // A bare `ZEND_AST_VAR` over a name.
                tree::StringPart::SimpleInterpolation(id)
                    if tree[id].suffix.is_none() && !next_needs_braces =>
                {
                    out.extend(text(parsed, tree[id].var));
                }
                tree::StringPart::DollarBraceInterpolation(id)
                    if tree[id].name.is_some() && tree[id].dim.is_none() && !next_needs_braces =>
                {
                    out.push(b'$');
                    out.extend(tree[id].name.map_or(&[][..], |name| text(parsed, name)));
                }
                tree::StringPart::BracedInterpolation(id)
                    if tree[id].inner.is_some_and(|inner| {
                        matches!(self.peel(inner), tree::Expr::SimpleVarExpr(_))
                    }) && !next_needs_braces =>
                {
                    if let Some(inner) = tree[id].inner {
                        self.export(out, inner, 0, indent)?;
                    }
                }
                _ => {
                    out.push(b'{');
                    self.export_part(out, part, indent)?;
                    out.push(b'}');
                }
            }
        }
        Ok(())
    }

    /// One variable part of a string as php's `encaps_var` builds it:
    /// `$a`, `$a[…]` over `encaps_var_offset`, `$a->b`, `${name}`,
    /// `${name[…]}`, `${expr}` and `{$expr}`.
    fn export_part(
        &mut self,
        out: &mut Vec<u8>,
        part: tree::StringPart,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        match part {
            tree::StringPart::LiteralStringPart(_) => Ok(()),
            tree::StringPart::SimpleInterpolation(id) => {
                let node = &tree[id];
                out.extend(text(parsed, node.var));
                match node.suffix {
                    Some(tree::SimpleInterpolationSuffix::SimpleDim(dim)) => {
                        self.export_offset(out, dim);
                    }
                    Some(tree::SimpleInterpolationSuffix::SimpleProp(prop)) => {
                        let prop = &tree[prop];
                        out.extend(self.arrow(prop.op));
                        out.extend(text(parsed, prop.name));
                    }
                    None => {}
                }
                Ok(())
            }
            tree::StringPart::DollarBraceInterpolation(id) => {
                let node = &tree[id];
                match (node.name, node.inner) {
                    (Some(name), _) => {
                        out.push(b'$');
                        out.extend(text(parsed, name));
                        if let Some(dim) = node.dim {
                            out.push(b'[');
                            if let Some(index) = tree[dim].index {
                                self.export_offset_value(out, dim, index);
                            }
                            out.push(b']');
                        }
                        Ok(())
                    }
                    (None, Some(inner)) => {
                        out.push(b'$');
                        self.export_var(out, Named::Node(inner), indent)
                    }
                    (None, None) => Ok(()),
                }
            }
            tree::StringPart::BracedInterpolation(id) => match tree[id].inner {
                Some(inner) => self.export(out, inner, 0, indent),
                None => Ok(()),
            },
        }
    }

    /// `[…]` of `encaps_var_offset`.
    fn export_offset(&self, out: &mut Vec<u8>, dim: NodeId<tree::SimpleDim>) {
        out.push(b'[');
        if let Some(index) = self.parsed.tree[dim].index {
            self.export_offset_value(out, dim, index);
        }
        out.push(b']');
    }

    /// The offset itself: a bare word as the string it is, a variable as
    /// itself, and a number as `T_NUM_STRING`'s value, negated by
    /// `zend_negate_num_string` under a `-`.
    fn export_offset_value(&self, out: &mut Vec<u8>, dim: NodeId<tree::SimpleDim>, index: TokenId) {
        let parsed = self.parsed;
        let node = &parsed.tree[dim];
        match kind(parsed, index) {
            SyntaxKind::VARIABLE => out.extend(text(parsed, index)),
            SyntaxKind::IDENT => export_quoted_str(out, text(parsed, index)),
            _ => match (
                TokenValue::of(parsed, Holder::Offset(node), index),
                node.neg.is_some(),
            ) {
                (Some(TokenValue::Int(0)), true) => out.extend(b"'-0'"),
                (Some(TokenValue::Int(n)), neg) => {
                    out.extend((if neg { -n } else { n }).to_string().as_bytes());
                }
                (Some(TokenValue::Bytes(digits)), neg) => {
                    let mut bytes = if neg { vec![b'-'] } else { Vec::new() };
                    bytes.extend(digits);
                    export_quoted_str(out, &bytes);
                }
                _ => {}
            },
        }
    }

    /// `zend_ast_export_attributes`: each group `#[…]`, then a newline
    /// at `indent` or a space.
    fn export_attributes(
        &mut self,
        out: &mut Vec<u8>,
        groups: &[NodeId<tree::AttributeGroup>],
        indent: usize,
        newlines: bool,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        for group in groups {
            out.extend(b"#[");
            for (at, attribute) in tree.list(tree[*group].attributes).iter().enumerate() {
                if at > 0 {
                    out.extend(b", ");
                }
                let attribute = &tree[*attribute];
                out.extend(text(parsed, attribute.name));
                if let Some(args) = attribute.args {
                    out.push(b'(');
                    self.export_args(out, args, indent)?;
                    out.push(b')');
                }
            }
            out.push(b']');
            if newlines {
                out.push(b'\n');
                indent_to(out, indent);
            } else {
                out.push(b' ');
            }
        }
        Ok(())
    }

    /// `zend_ast_export_type`: a union by `|`, an intersection by `&`,
    /// php's `?` before a nullable one, and a name as written, the four
    /// php keeps as `ZEND_AST_TYPE` lowercased. A parenthesis of a
    /// disjunctive type is no node of php's, so none is printed.
    fn export_type(&self, out: &mut Vec<u8>, ty: tree::TypeExpr) {
        let parsed = self.parsed;
        let tree = &parsed.tree;
        match ty {
            tree::TypeExpr::NamedType(id) => {
                let name = text(parsed, tree[id].name);
                match builtin_type(name) {
                    Some(builtin) => out.extend(builtin.as_bytes()),
                    None => out.extend(name),
                }
            }
            tree::TypeExpr::NullableType(id) => {
                out.push(b'?');
                if let Some(inner) = tree[id].ty {
                    self.export_type(out, inner);
                }
            }
            tree::TypeExpr::UnionType(id) => {
                for (at, inner) in tree.list(tree[id].types).iter().enumerate() {
                    if at > 0 {
                        out.push(b'|');
                    }
                    self.export_type(out, *inner);
                }
            }
            tree::TypeExpr::IntersectionType(id) => {
                for (at, inner) in tree.list(tree[id].types).iter().enumerate() {
                    if at > 0 {
                        out.push(b'&');
                    }
                    self.export_type(out, *inner);
                }
            }
            tree::TypeExpr::ParenType(id) => {
                if let Some(inner) = tree[id].ty {
                    self.export_type(out, inner);
                }
            }
        }
    }

    /// The `ZEND_AST_FUNC_DECL`, `ZEND_AST_CLOSURE`,
    /// `ZEND_AST_ARROW_FUNC` and `ZEND_AST_METHOD` arm.
    fn export_decl(
        &mut self,
        out: &mut Vec<u8>,
        decl: &Decl<'_>,
        parenthesized: bool,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        let arrow = decl.kind == DeclKind::Arrow;
        if arrow && parenthesized {
            out.push(b'(');
        }
        let inline = matches!(decl.kind, DeclKind::Closure | DeclKind::Arrow);
        self.export_attributes(out, decl.attrs, indent, !inline)?;
        decl.flags.visibility(out, false);
        if decl.flags.is_static {
            out.extend(b"static ");
        }
        if decl.flags.is_abstract {
            out.extend(b"abstract ");
        }
        if decl.flags.is_final {
            out.extend(b"final ");
        }
        out.extend(if arrow { &b"fn"[..] } else { &b"function "[..] });
        if decl.by_ref {
            out.push(b'&');
        }
        if !inline {
            out.extend(decl.name);
        }
        out.push(b'(');
        self.export_params(out, decl.params, indent)?;
        out.push(b')');
        if let Some(uses) = decl.uses {
            out.extend(b" use(");
            for (at, used) in tree.list(tree[uses].closure_use_vars).iter().enumerate() {
                if at > 0 {
                    out.extend(b", ");
                }
                let used = &tree[*used];
                if used.byref.is_some() {
                    out.push(b'&');
                }
                out.extend(text(parsed, used.var));
            }
            out.push(b')');
        }
        if let Some(ty) = decl.ret.and_then(|ret| tree[ret].ty) {
            out.extend(b": ");
            self.export_type(out, ty);
        }
        match decl.body {
            DeclBody::Arrow(body) => {
                out.extend(b" => ");
                self.export(out, body, 0, indent)?;
                if parenthesized {
                    out.push(b')');
                }
            }
            DeclBody::Block(block) => {
                out.extend(b" {\n");
                self.export_stmts(out, tree.list(tree[block].stmts), indent + 1)?;
                indent_to(out, indent);
                out.push(b'}');
                if !inline {
                    out.push(b'\n');
                }
            }
            DeclBody::None => out.extend(b";\n"),
        }
        Ok(())
    }

    /// `ZEND_AST_PARAM_LIST` of `ZEND_AST_PARAM`s.
    fn export_params(
        &mut self,
        out: &mut Vec<u8>,
        params: NodeId<tree::ParamList>,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        for (at, param) in tree.list(tree[params].params).iter().enumerate() {
            if at > 0 {
                out.extend(b", ");
            }
            let param = &tree[*param];
            self.export_attributes(out, self.parsed.tree.list(param.attrs), indent, false)?;
            let flags = Flags::of(parsed, self.parsed.tree.list(param.modifiers), false);
            flags.visibility(out, true);
            if flags.is_final {
                out.extend(b"final ");
            }
            if let Some(ty) = param.ty {
                self.export_type(out, ty);
                out.push(b' ');
            }
            if param.byref.is_some() {
                out.push(b'&');
            }
            if param.variadic.is_some() {
                out.extend(b"...");
            }
            out.extend(text(parsed, param.var));
            if let Some(default) = param.default {
                out.extend(b" = ");
                self.export(out, default, 0, indent)?;
            }
            if let Some(hooks) = param.hooks {
                self.export_hooks(out, hooks, indent)?;
            }
        }
        Ok(())
    }

    /// `zend_ast_export_hook_list`.
    fn export_hooks(
        &mut self,
        out: &mut Vec<u8>,
        hooks: NodeId<tree::PropertyHookList>,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        out.extend(b" {\n");
        let indent = indent + 1;
        indent_to(out, indent);
        let list = tree.list(tree[hooks].property_hooks);
        for (at, hook) in list.iter().enumerate() {
            let hook = &tree[*hook];
            let flags = Flags::of(parsed, self.parsed.tree.list(hook.modifiers), false);
            flags.visibility(out, true);
            if flags.is_final {
                out.extend(b"final ");
            }
            out.extend(text(parsed, hook.name));
            if let Some(arrow) = hook.arrow {
                out.extend(b" => ");
                if let Some(value) = tree[arrow].value {
                    self.export(out, value, 0, indent)?;
                }
                out.push(b';');
            } else if let Some(body) = hook.body {
                out.extend(b" {\n");
                self.export_stmts(out, tree.list(tree[body].stmts), indent + 1)?;
                indent_to(out, indent);
                out.push(b'}');
            } else {
                out.push(b';');
            }
            if at + 1 < list.len() {
                out.push(b'\n');
                indent_to(out, indent);
            }
        }
        out.push(b'\n');
        indent_to(out, indent - 1);
        out.push(b'}');
        Ok(())
    }

    /// `zend_ast_export_stmt` over a statement list.
    pub(super) fn export_stmts(
        &mut self,
        out: &mut Vec<u8>,
        stmts: &[Stmt],
        indent: usize,
    ) -> Result<(), CompileError> {
        for stmt in stmts {
            self.export_stmt(out, *stmt, indent)?;
        }
        Ok(())
    }

    /// `ZEND_AST_NAMESPACE`: the name where one is written, then the
    /// braces' statements a level in, or the `;` of the unbracketed form.
    fn export_namespace(
        &mut self,
        out: &mut Vec<u8>,
        id: NodeId<tree::NamespaceDef>,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let node = &parsed.tree[id];
        indent_to(out, indent);
        out.extend(b"namespace");
        if let Some(name) = node.name {
            out.push(b' ');
            out.extend(text(parsed, name));
        }
        match node.body {
            Some(body) => {
                out.extend(b" {\n");
                self.export_stmts(out, parsed.tree.list(parsed.tree[body].stmts), indent + 1)?;
                indent_to(out, indent);
                out.extend(b"}\n");
            }
            None => out.push(b';'),
        }
        out.push(b'\n');
        Ok(())
    }

    /// `ZEND_AST_USE`: `use ` and each name, ` as ` its alias where one is
    /// written, without the leading backslash the parser drops. php writes no
    /// `function` or `const`, since the node's `attr` is the symbol kind
    /// and not the token its `T_FUNCTION` test reads.
    fn export_use(&self, out: &mut Vec<u8>, id: NodeId<tree::UseStmt>) {
        let parsed = self.parsed;
        let tree = &parsed.tree;
        let node = &tree[id];
        let unprefixed = |name: &'a [u8]| name.strip_prefix(b"\\").unwrap_or(name);
        out.extend(b"use ");
        let mut items: Vec<(&[u8], Option<&[u8]>)> = Vec::new();
        let prefix = node
            .use_group
            .map(|group| unprefixed(text(parsed, tree[group].prefix)));
        match node.use_group {
            Some(group) => {
                for item in tree.list(tree[group].use_group_items) {
                    let item = &tree[*item];
                    items.push((
                        text(parsed, item.name),
                        item.alias.map(|a| text(parsed, tree[a].name)),
                    ));
                }
            }
            None => {
                for clause in self.parsed.tree.list(node.use_clauses) {
                    let clause = &tree[*clause];
                    let alias = clause.alias.map(|a| text(parsed, tree[a].name));
                    items.push((unprefixed(text(parsed, clause.name)), alias));
                }
            }
        }
        if let Some(prefix) = prefix {
            out.extend(prefix);
            out.extend(b"\\{");
        }
        for (at, (name, alias)) in items.into_iter().enumerate() {
            if at > 0 {
                out.extend(b", ");
            }
            out.extend(name);
            if let Some(alias) = alias {
                out.extend(b" as ");
                out.extend(alias);
            }
        }
        if prefix.is_some() {
            out.push(b'}');
        }
    }

    /// One php statement: the indent, the node, `;` unless the node is
    /// one of those that end in a block, and the newline.
    fn line(
        &mut self,
        out: &mut Vec<u8>,
        indent: usize,
        semicolon: bool,
        body: impl FnOnce(&mut Self, &mut Vec<u8>) -> Result<(), CompileError>,
    ) -> Result<(), CompileError> {
        indent_to(out, indent);
        body(self, out)?;
        if semicolon {
            out.push(b';');
        }
        out.push(b'\n');
        Ok(())
    }

    /// `zend_ast_export_stmt` over one statement of the tree, which may
    /// be several of php's.
    #[allow(clippy::too_many_lines)]
    fn export_stmt(
        &mut self,
        out: &mut Vec<u8>,
        stmt: Stmt,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        match stmt {
            Stmt::Block(id) => self.export_stmts(out, tree.list(tree[id].stmts), indent),
            // An empty statement is no node of php's.
            Stmt::EmptyStmt(_) => Ok(()),
            Stmt::NamespaceDef(id) => self.export_namespace(out, id, indent),
            Stmt::UseStmt(id) => self.line(out, indent, true, |this, out| {
                this.export_use(out, id);
                Ok(())
            }),
            Stmt::ExprStmt(id) => {
                let expr = tree[id].expr;
                self.line(out, indent, true, |s, out| s.export(out, expr, 0, indent))
            }
            Stmt::InlineHtmlStmt(id) => {
                let body = tree[id].body.map_or(&[][..], |body| text(parsed, body));
                self.line(out, indent, true, |_, out| {
                    out.extend(b"echo ");
                    export_quoted_str(out, body);
                    Ok(())
                })
            }
            Stmt::HaltCompilerStmt(_) => self.line(out, indent, true, |_, out| {
                out.extend(b"__HALT_COMPILER()");
                Ok(())
            }),
            Stmt::EchoStmt(id) => {
                for expr in tree.list(tree[id].exprs) {
                    self.line(out, indent, true, |s, out| {
                        out.extend(b"echo ");
                        s.export(out, *expr, 0, indent)
                    })?;
                }
                Ok(())
            }
            Stmt::GlobalStmt(id) => {
                for var in tree.list(tree[id].vars) {
                    self.line(out, indent, true, |s, out| {
                        out.extend(b"global ");
                        s.export(out, *var, 0, indent)
                    })?;
                }
                Ok(())
            }
            Stmt::StaticVarStmt(id) => {
                for declarator in tree.list(tree[id].static_var_declarators) {
                    let declarator = &tree[*declarator];
                    self.line(out, indent, true, |s, out| {
                        out.extend(b"static ");
                        out.extend(text(parsed, declarator.var));
                        if let Some(init) = declarator.init {
                            out.extend(b" = ");
                            s.export(out, init, 0, indent)?;
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            }
            Stmt::UnsetStmt(id) => {
                for target in tree.list(tree[id].targets) {
                    self.line(out, indent, true, |s, out| {
                        s.func_op(out, "unset", Some(*target), indent)
                    })?;
                }
                Ok(())
            }
            Stmt::IfStmt(id) => {
                self.line(out, indent, false, |s, out| s.export_if(out, id, indent))
            }
            Stmt::WhileStmt(id) => {
                let node = &tree[id];
                self.line(out, indent, false, |s, out| {
                    out.extend(b"while (");
                    if let Some(cond) = node.cond {
                        s.export(out, cond, 0, indent)?;
                    }
                    out.extend(b") {\n");
                    s.export_body(out, node.body, node.stmt_list, indent + 1)?;
                    indent_to(out, indent);
                    out.push(b'}');
                    Ok(())
                })
            }
            Stmt::DoWhileStmt(id) => {
                let node = &tree[id];
                self.line(out, indent, true, |s, out| {
                    out.extend(b"do {\n");
                    s.export_body(out, node.body, None, indent + 1)?;
                    indent_to(out, indent);
                    out.extend(b"} while (");
                    if let Some(cond) = node.cond {
                        s.export(out, cond, 0, indent)?;
                    }
                    out.push(b')');
                    Ok(())
                })
            }
            Stmt::ForStmt(id) => {
                let node = &tree[id];
                self.line(out, indent, false, |s, out| {
                    out.extend(b"for (");
                    if let Some(init) = node.init {
                        s.export_list(out, tree.list(tree[init].exprs), 20, indent)?;
                    }
                    out.push(b';');
                    if let Some(cond) = node.cond.filter(|c| !tree[*c].exprs.is_empty()) {
                        out.push(b' ');
                        s.export_list(out, tree.list(tree[cond].exprs), 20, indent)?;
                    }
                    out.push(b';');
                    if let Some(step) = node.step.filter(|c| !tree[*c].exprs.is_empty()) {
                        out.push(b' ');
                        s.export_list(out, tree.list(tree[step].exprs), 20, indent)?;
                    }
                    out.extend(b") {\n");
                    s.export_body(out, node.body, node.stmt_list, indent + 1)?;
                    indent_to(out, indent);
                    out.push(b'}');
                    Ok(())
                })
            }
            Stmt::ForeachStmt(id) => {
                let node = &tree[id];
                self.line(out, indent, false, |s, out| {
                    out.extend(b"foreach (");
                    if let Some(iterable) = node.iterable {
                        s.export(out, iterable, 0, indent)?;
                    }
                    out.extend(b" as ");
                    if let Some(key) = node.key {
                        s.export(out, tree[key].key, 0, indent)?;
                        out.extend(b" => ");
                    }
                    if let Some(target) = node.target {
                        let target = &tree[target];
                        if target.byref.is_some() {
                            out.push(b'&');
                        }
                        if let Some(value) = target.target {
                            s.export(out, value, 0, indent)?;
                        }
                    }
                    out.extend(b") {\n");
                    s.export_body(out, node.body, node.stmt_list, indent + 1)?;
                    indent_to(out, indent);
                    out.push(b'}');
                    Ok(())
                })
            }
            Stmt::SwitchStmt(id) => {
                let node = &tree[id];
                self.line(out, indent, false, |s, out| {
                    out.extend(b"switch (");
                    if let Some(subject) = node.subject {
                        s.export(out, subject, 0, indent)?;
                    }
                    out.extend(b") {\n");
                    for arm in self.parsed.tree.list(node.switch_arms) {
                        let arm = &tree[*arm];
                        indent_to(out, indent + 1);
                        match arm.value {
                            Some(value) => {
                                out.extend(b"case ");
                                s.export(out, value, 0, indent + 1)?;
                                out.extend(b":\n");
                            }
                            None => out.extend(b"default:\n"),
                        }
                        if let Some(list) = arm.stmt_list {
                            s.export_stmts(out, tree.list(tree[list].stmts), indent + 2)?;
                        }
                    }
                    indent_to(out, indent);
                    out.push(b'}');
                    Ok(())
                })
            }
            Stmt::BreakStmt(id) => {
                let level = tree[id].level;
                self.line(out, indent, true, |s, out| {
                    s.keyword(out, "break", level, indent)
                })
            }
            Stmt::ContinueStmt(id) => {
                let level = tree[id].level;
                self.line(out, indent, true, |s, out| {
                    s.keyword(out, "continue", level, indent)
                })
            }
            Stmt::ReturnStmt(id) => {
                let value = tree[id].value;
                self.line(out, indent, true, |s, out| {
                    s.keyword(out, "return", value, indent)
                })
            }
            Stmt::GotoStmt(id) => {
                let label = tree[id].label;
                self.line(out, indent, true, |_, out| {
                    out.extend(b"goto ");
                    if let Some(label) = label {
                        out.extend(text(parsed, label));
                    }
                    Ok(())
                })
            }
            Stmt::LabelStmt(id) => {
                let name = tree[id].name;
                self.line(out, indent, false, |_, out| {
                    out.extend(text(parsed, name));
                    out.push(b':');
                    Ok(())
                })
            }
            Stmt::TryStmt(id) => {
                let node = &tree[id];
                self.line(out, indent, false, |s, out| {
                    out.extend(b"try {\n");
                    s.export_stmts(out, tree.list(tree[node.body].stmts), indent + 1)?;
                    indent_to(out, indent);
                    for clause in self.parsed.tree.list(node.catch_clauses) {
                        let clause = &tree[*clause];
                        out.extend(b"} catch (");
                        if let Some(types) = clause.types {
                            for (at, ty) in tree.list(tree[types].types).iter().enumerate() {
                                if at > 0 {
                                    out.push(b'|');
                                }
                                out.extend(text(parsed, *ty));
                            }
                        }
                        if let Some(var) = clause.var {
                            out.push(b' ');
                            out.extend(text(parsed, var));
                        }
                        out.extend(b") {\n");
                        if let Some(body) = clause.body {
                            s.export_stmts(out, tree.list(tree[body].stmts), indent + 1)?;
                        }
                        indent_to(out, indent);
                    }
                    if let Some(finally) = node.finally_clause {
                        out.extend(b"} finally {\n");
                        if let Some(body) = tree[finally].body {
                            s.export_stmts(out, tree.list(tree[body].stmts), indent + 1)?;
                        }
                        indent_to(out, indent);
                    }
                    out.push(b'}');
                    Ok(())
                })
            }
            Stmt::DeclareStmt(id) => {
                let node = &tree[id];
                self.line(out, indent, false, |s, out| {
                    out.extend(b"declare(");
                    for (at, directive) in self
                        .parsed
                        .tree
                        .list(node.declare_directives)
                        .iter()
                        .enumerate()
                    {
                        if at > 0 {
                            out.extend(b", ");
                        }
                        let directive = &tree[*directive];
                        out.extend(text(parsed, directive.name));
                        if let Some(value) = directive.value {
                            out.extend(b" = ");
                            s.export(out, value, 0, indent)?;
                        }
                    }
                    out.push(b')');
                    if node.body.is_some() || node.stmt_list.is_some() {
                        out.extend(b" {\n");
                        s.export_body(out, node.body, node.stmt_list, indent + 1)?;
                        indent_to(out, indent);
                        out.push(b'}');
                    } else {
                        out.push(b';');
                    }
                    Ok(())
                })
            }
            Stmt::ConstStmt(id) => {
                let node = &tree[id];
                self.line(out, indent, true, |s, out| {
                    s.export_attributes(out, self.parsed.tree.list(node.attrs), indent, true)?;
                    out.extend(b"const ");
                    s.export_const_declarators(
                        out,
                        self.parsed.tree.list(node.const_declarators),
                        indent,
                    )
                })
            }
            Stmt::FunctionDecl(id) => {
                let node = &tree[id];
                let decl = Decl {
                    kind: DeclKind::Function,
                    attrs: self.parsed.tree.list(node.attrs),
                    flags: Flags::default(),
                    by_ref: node.byref.is_some(),
                    name: text(parsed, node.name),
                    params: node.params,
                    uses: None,
                    ret: node.ret,
                    body: node.body.map_or(DeclBody::None, DeclBody::Block),
                };
                self.line(out, indent, false, |s, out| {
                    s.export_decl(out, &decl, false, indent)
                })
            }
            Stmt::ClassDecl(id) => {
                let node = &tree[id];
                let class = ClassLike {
                    attrs: self.parsed.tree.list(node.attrs),
                    keyword: "class ",
                    modifiers: Flags::of(parsed, self.parsed.tree.list(node.modifiers), false),
                    name: node.name.map_or(&[][..], |name| text(parsed, name)),
                    backing: None,
                    extends: node.extends.map(|e| tree.list(tree[e].types)),
                    implements: node.implements.map(|i| tree.list(tree[i].types)),
                    body: node.body,
                };
                self.line(out, indent, false, |s, out| {
                    s.export_class(out, &class, indent)
                })
            }
            Stmt::InterfaceDecl(id) => {
                let node = &tree[id];
                // `interface_extends_list` stands where a class's
                // `implements` does, and is printed as one.
                let class = ClassLike {
                    attrs: self.parsed.tree.list(node.attrs),
                    keyword: "interface ",
                    modifiers: Flags::default(),
                    name: node.name.map_or(&[][..], |name| text(parsed, name)),
                    backing: None,
                    extends: None,
                    implements: node.extends.map(|e| tree.list(tree[e].types)),
                    body: node.body,
                };
                self.line(out, indent, false, |s, out| {
                    s.export_class(out, &class, indent)
                })
            }
            Stmt::TraitDecl(id) => {
                let node = &tree[id];
                let class = ClassLike {
                    attrs: self.parsed.tree.list(node.attrs),
                    keyword: "trait ",
                    modifiers: Flags::default(),
                    name: node.name.map_or(&[][..], |name| text(parsed, name)),
                    backing: None,
                    extends: None,
                    implements: None,
                    body: node.body,
                };
                self.line(out, indent, false, |s, out| {
                    s.export_class(out, &class, indent)
                })
            }
            Stmt::EnumDecl(id) => {
                let node = &tree[id];
                let class = ClassLike {
                    attrs: self.parsed.tree.list(node.attrs),
                    keyword: "enum ",
                    modifiers: Flags::default(),
                    name: node.name.map_or(&[][..], |name| text(parsed, name)),
                    backing: node.backing.and_then(|b| tree[b].ty),
                    extends: None,
                    implements: node.implements.map(|i| tree.list(tree[i].types)),
                    body: node.body,
                };
                self.line(out, indent, false, |s, out| {
                    s.export_class(out, &class, indent)
                })
            }
        }
    }

    /// `APPEND_NODE_1`: the keyword, and the operand after a space.
    fn keyword(
        &mut self,
        out: &mut Vec<u8>,
        word: &str,
        operand: Option<tree::Expr>,
        indent: usize,
    ) -> Result<(), CompileError> {
        out.extend(word.as_bytes());
        if let Some(operand) = operand {
            out.push(b' ');
            self.export(out, operand, 0, indent)?;
        }
        Ok(())
    }

    /// A loop's or a `declare`'s body: one statement, or the list of the
    /// alternative syntax.
    fn export_body(
        &mut self,
        out: &mut Vec<u8>,
        body: Option<Stmt>,
        list: Option<NodeId<tree::StmtList>>,
        indent: usize,
    ) -> Result<(), CompileError> {
        if let Some(body) = body {
            self.export_stmt(out, body, indent)?;
        }
        if let Some(list) = list {
            let parsed: &'a Parsed = self.parsed;
            self.export_stmts(out, parsed.tree.list(parsed.tree[list].stmts), indent)?;
        }
        Ok(())
    }

    /// `zend_ast_export_if_stmt`: each arm of the chain, an `else if`
    /// written as two words continuing it.
    fn export_if(
        &mut self,
        out: &mut Vec<u8>,
        id: NodeId<tree::IfStmt>,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        let mut chain = Some(id);
        while let Some(id) = chain.take() {
            let node = &tree[id];
            let mut arms: Vec<IfArm> = vec![(node.cond, node.then, node.stmt_list)];
            for clause in self.parsed.tree.list(node.elseif_clauses) {
                let clause = &tree[*clause];
                arms.push((clause.cond, clause.body, None));
            }
            for clause in self.parsed.tree.list(node.alt_elseif_clauses) {
                let clause = &tree[*clause];
                arms.push((clause.cond, None, clause.stmt_list));
            }
            for (at, (cond, body, list)) in arms.into_iter().enumerate() {
                if at == 0 {
                    out.extend(b"if (");
                } else {
                    indent_to(out, indent);
                    out.extend(b"} elseif (");
                }
                if let Some(cond) = cond {
                    self.export(out, cond, 0, indent)?;
                }
                out.extend(b") {\n");
                self.export_body(out, body, list, indent + 1)?;
            }
            let (body, list) = match (node.else_clause, node.alt_else_clause) {
                (Some(clause), _) => (tree[clause].body, None),
                (None, Some(clause)) => (None, tree[clause].stmt_list),
                (None, None) => break,
            };
            indent_to(out, indent);
            out.extend(b"} else ");
            if let Some(Stmt::IfStmt(next)) = body {
                chain = Some(next);
                continue;
            }
            out.extend(b"{\n");
            self.export_body(out, body, list, indent + 1)?;
        }
        indent_to(out, indent);
        out.push(b'}');
        Ok(())
    }

    /// `ZEND_AST_CLASS`: attributes, the modifiers, the keyword and the
    /// name, an enum's backing type, and the rest after the header.
    fn export_class(
        &mut self,
        out: &mut Vec<u8>,
        class: &ClassLike<'_>,
        indent: usize,
    ) -> Result<(), CompileError> {
        self.export_attributes(out, class.attrs, indent, true)?;
        if class.keyword == "class " {
            if class.modifiers.is_abstract {
                out.extend(b"abstract ");
            }
            if class.modifiers.is_final {
                out.extend(b"final ");
            }
            if class.modifiers.is_readonly {
                out.extend(b"readonly ");
            }
        }
        out.extend(class.keyword.as_bytes());
        out.extend(class.name);
        if let Some(backing) = class.backing {
            out.extend(b": ");
            self.export_type(out, backing);
        }
        self.export_class_no_header(out, class.extends, class.implements, class.body, indent)?;
        out.push(b'\n');
        Ok(())
    }

    /// `zend_ast_export_class_no_header`: ` extends`, ` implements`, and
    /// the members at one more indent.
    fn export_class_no_header(
        &mut self,
        out: &mut Vec<u8>,
        extends: Option<&[TokenId]>,
        implements: Option<&[TokenId]>,
        body: NodeId<tree::ClassBody>,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        if let Some(extends) = extends {
            out.extend(b" extends ");
            self.export_names(out, extends);
        }
        if let Some(implements) = implements {
            out.extend(b" implements ");
            self.export_names(out, implements);
        }
        out.extend(b" {\n");
        for member in tree.list(tree[body].class_members) {
            self.export_member_decl(out, *member, indent + 1)?;
        }
        indent_to(out, indent);
        out.push(b'}');
        Ok(())
    }

    /// `zend_ast_export_name_list`: names as written, `, ` between.
    fn export_names(&self, out: &mut Vec<u8>, names: &[TokenId]) {
        for (at, name) in names.iter().enumerate() {
            if at > 0 {
                out.extend(b", ");
            }
            out.extend(text(self.parsed, *name));
        }
    }

    /// `ZEND_AST_CONST_ELEM`s: the name, ` = ` and the value.
    fn export_const_declarators(
        &mut self,
        out: &mut Vec<u8>,
        declarators: &[NodeId<tree::ConstDeclarator>],
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        for (at, declarator) in declarators.iter().enumerate() {
            if at > 0 {
                out.extend(b", ");
            }
            let declarator = &tree[*declarator];
            if let Some(name) = tree[declarator.name].name {
                out.extend(text(parsed, name));
            }
            if let Some(value) = declarator.value {
                out.extend(b" = ");
                self.export(out, value, 0, indent)?;
            }
        }
        Ok(())
    }

    /// One member of a class body as a statement of its list.
    #[allow(clippy::too_many_lines)]
    fn export_member_decl(
        &mut self,
        out: &mut Vec<u8>,
        member: tree::ClassMember,
        indent: usize,
    ) -> Result<(), CompileError> {
        let parsed: &'a Parsed = self.parsed;
        let tree = &parsed.tree;
        match member {
            tree::ClassMember::ClassConstDecl(id) => {
                let node = &tree[id];
                self.line(out, indent, true, |s, out| {
                    s.export_attributes(out, self.parsed.tree.list(node.attrs), indent, true)?;
                    Flags::of(parsed, self.parsed.tree.list(node.modifiers), true)
                        .visibility(out, false);
                    out.extend(b"const ");
                    if let Some(ty) = node.ty {
                        s.export_type(out, ty);
                        out.push(b' ');
                    }
                    s.export_const_declarators(
                        out,
                        self.parsed.tree.list(node.const_declarators),
                        indent,
                    )
                })
            }
            tree::ClassMember::PropertyDecl(id) => {
                let node = &tree[id];
                self.line(out, indent, node.hooks.is_none(), |s, out| {
                    s.export_attributes(out, self.parsed.tree.list(node.attrs), indent, true)?;
                    let flags = Flags::of(parsed, self.parsed.tree.list(node.modifiers), true);
                    flags.visibility(out, true);
                    if flags.is_static {
                        out.extend(b"static ");
                    }
                    if flags.is_readonly {
                        out.extend(b"readonly ");
                    }
                    if let Some(ty) = node.ty {
                        s.export_type(out, ty);
                        out.push(b' ');
                    }
                    for (at, declarator) in self
                        .parsed
                        .tree
                        .list(node.property_declarators)
                        .iter()
                        .enumerate()
                    {
                        if at > 0 {
                            out.extend(b", ");
                        }
                        let declarator = &tree[*declarator];
                        out.extend(text(parsed, declarator.var));
                        if let Some(init) = declarator.init {
                            out.extend(b" = ");
                            s.export(out, init, 0, indent + 1)?;
                        }
                    }
                    if let Some(hooks) = node.hooks {
                        s.export_hooks(out, hooks, indent)?;
                    }
                    Ok(())
                })
            }
            tree::ClassMember::MethodDecl(id) => {
                let node = &tree[id];
                let decl = Decl {
                    kind: DeclKind::Method,
                    attrs: self.parsed.tree.list(node.attrs),
                    flags: Flags::of(parsed, self.parsed.tree.list(node.modifiers), true),
                    by_ref: node.byref.is_some(),
                    name: node
                        .name
                        .and_then(|name| tree[name].name)
                        .map_or(&[][..], |name| text(parsed, name)),
                    params: node.params,
                    uses: None,
                    ret: node.ret,
                    body: node.body.map_or(DeclBody::None, DeclBody::Block),
                };
                self.line(out, indent, false, |s, out| {
                    s.export_decl(out, &decl, false, indent)
                })
            }
            tree::ClassMember::TraitUseDecl(id) => {
                let node = &tree[id];
                self.line(out, indent, false, |s, out| {
                    out.extend(b"use ");
                    s.export_names(out, self.parsed.tree.list(node.traits));
                    let Some(adaptations) = node.adaptations else {
                        out.push(b';');
                        return Ok(());
                    };
                    out.extend(b" {\n");
                    for adaptation in tree.list(tree[adaptations].trait_adaptations) {
                        s.line(out, indent + 1, true, |s, out| {
                            s.export_adaptation(out, *adaptation);
                            Ok(())
                        })?;
                    }
                    indent_to(out, indent);
                    out.push(b'}');
                    Ok(())
                })
            }
            tree::ClassMember::EnumCaseDecl(id) => {
                let node = &tree[id];
                self.line(out, indent, true, |s, out| {
                    s.export_attributes(out, self.parsed.tree.list(node.attrs), indent, true)?;
                    out.extend(b"case ");
                    if let Some(name) = node.name.and_then(|name| tree[name].name) {
                        out.extend(text(parsed, name));
                    }
                    if let Some(value) = node.value {
                        out.extend(b" = ");
                        s.export(out, value, 0, indent)?;
                    }
                    Ok(())
                })
            }
        }
    }

    /// `ZEND_AST_TRAIT_PRECEDENCE` and `ZEND_AST_TRAIT_ALIAS`.
    fn export_adaptation(&self, out: &mut Vec<u8>, adaptation: tree::TraitAdaptation) {
        let parsed = self.parsed;
        let tree = &parsed.tree;
        let method_ref = |out: &mut Vec<u8>, id: NodeId<tree::TraitMethodRef>| {
            let node = &tree[id];
            if let Some(name) = node.r#trait {
                out.extend(text(parsed, name));
                out.extend(b"::");
            }
            if let Some(method) = node.method.and_then(|method| tree[method].name) {
                out.extend(text(parsed, method));
            }
        };
        match adaptation {
            tree::TraitAdaptation::InsteadofAdaptation(id) => {
                let node = &tree[id];
                method_ref(out, node.r#ref);
                out.extend(b" insteadof ");
                self.export_names(out, self.parsed.tree.list(node.excluded));
            }
            tree::TraitAdaptation::AsAdaptation(id) => {
                let node = &tree[id];
                method_ref(out, node.r#ref);
                let alias = node.alias.and_then(|alias| tree[alias].name);
                match node.vis.map(|vis| kind(parsed, vis)) {
                    Some(SyntaxKind::PUBLIC_KW) => out.extend(b" as public"),
                    Some(SyntaxKind::PROTECTED_KW) => out.extend(b" as protected"),
                    Some(SyntaxKind::PRIVATE_KW) => out.extend(b" as private"),
                    _ if alias.is_some() => out.extend(b" as"),
                    _ => {}
                }
                if let Some(alias) = alias {
                    out.push(b' ');
                    out.extend(text(parsed, alias));
                }
            }
        }
    }
}
