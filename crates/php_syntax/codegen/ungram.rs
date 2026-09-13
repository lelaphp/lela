//! The reader of the schema DSL `php.ungram` is written in (design §4.9).
//!
//! This is not vanilla ungrammar. The dialect is specified in the schema
//! file's own header, the "DSL SYNTAX" and "CONVENTIONS" sections, and that
//! header, not any ungrammar documentation, is the authority for what this
//! reader accepts:
//!
//! - `Node = Rule` — struct node (gets a `SyntaxKind`, convention A1)
//! - `Node = A | B | C` — alternation-only definition: a typed enum when at
//!   least one alternative is a node reference (mixed token/node alternations
//!   such as `Modifier` included), or a token-set alias when every
//!   alternative is a token (`BinaryOp`, `NameRef`). Neither form gets its
//!   own `SyntaxKind` (convention A1; see [`classify`]).
//! - `trait Name = Rule` — mixin, including the empty-bodied form
//!   (`HasDocComment` declares no children, convention A5)
//! - `Node : TraitA TraitB = Rule` — mixin opt-in
//! - juxtaposition = sequence; `(...)` grouping; `?` optional; `*` repetition
//! - `R,*` / `R,+` — comma-separated lists (trailing comma ok); the comma
//!   token itself is implied by the operator, never written as `','`
//! - `'tok'` quoted tokens, `<class>` token classes, `label:Rule` accessor
//!   labels, and the `R!` required marker (convention A2); in
//!   `php.ungram`, `!` only ever follows node and token-class references,
//!   and that is the only position this reader accepts
//! - `//` line comments everywhere, including trailing on rule lines
//!
//! Errors are line-numbered against the input.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A line-numbered error from lexing, parsing or validating the DSL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// 1-based line in the schema file, when the error is attributable.
    pub line: Option<u32>,
    /// Human-readable description.
    pub message: String,
}

impl Error {
    pub(crate) fn at(line: u32, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            message: message.into(),
        }
    }

    pub(crate) fn global(message: impl Into<String>) -> Self {
        Self {
            line: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    /// `'...'` quoted token; the payload is the text between the quotes.
    Literal(String),
    /// `<...>` token-class reference; the payload is the class name.
    Class(String),
    Eq,
    Pipe,
    LParen,
    RParen,
    Question,
    Star,
    Plus,
    Bang,
    Colon,
    Comma,
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Ident(name) => format!("identifier `{name}`"),
        Tok::Literal(text) => format!("token literal `'{text}'`"),
        Tok::Class(name) => format!("token class `<{name}>`"),
        Tok::Eq => "`=`".to_owned(),
        Tok::Pipe => "`|`".to_owned(),
        Tok::LParen => "`(`".to_owned(),
        Tok::RParen => "`)`".to_owned(),
        Tok::Question => "`?`".to_owned(),
        Tok::Star => "`*`".to_owned(),
        Tok::Plus => "`+`".to_owned(),
        Tok::Bang => "`!`".to_owned(),
        Tok::Colon => "`:`".to_owned(),
        Tok::Comma => "`,`".to_owned(),
    }
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    line: u32,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Single-character DSL punctuation.
fn punct_tok(b: u8) -> Option<Tok> {
    match b {
        b'=' => Some(Tok::Eq),
        b'|' => Some(Tok::Pipe),
        b'(' => Some(Tok::LParen),
        b')' => Some(Tok::RParen),
        b'?' => Some(Tok::Question),
        b'*' => Some(Tok::Star),
        b'+' => Some(Tok::Plus),
        b'!' => Some(Tok::Bang),
        b':' => Some(Tok::Colon),
        b',' => Some(Tok::Comma),
        _ => None,
    }
}

/// Tokenizes DSL source. Line comments (`//` to end of line) are stripped
/// here, which is what makes them legal everywhere, including trailing.
fn lex(text: &str) -> Result<Vec<Token>, Vec<Error>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut errors = Vec::new();
    let mut line: u32 = 1;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(tok) = punct_tok(b) {
            tokens.push(Token { tok, line });
            i += 1;
            continue;
        }
        match b {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'\'' => {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b'\'' && bytes[j] != b'\n' {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'\'') {
                    let content = &text[i + 1..j];
                    if content.is_empty() {
                        errors.push(Error::at(line, "empty token literal `''`"));
                    } else {
                        tokens.push(Token {
                            tok: Tok::Literal(content.to_owned()),
                            line,
                        });
                    }
                    i = j + 1;
                } else {
                    errors.push(Error::at(line, "unterminated token literal"));
                    i = j;
                }
            }
            b'<' => {
                let mut j = i + 1;
                while j < bytes.len() && is_ident_continue(bytes[j]) {
                    j += 1;
                }
                if j > i + 1 && bytes.get(j) == Some(&b'>') {
                    tokens.push(Token {
                        tok: Tok::Class(text[i + 1..j].to_owned()),
                        line,
                    });
                    i = j + 1;
                } else {
                    errors.push(Error::at(
                        line,
                        "malformed token class (expected `<name>` with a non-empty name)",
                    ));
                    i += 1;
                }
            }
            _ if is_ident_start(b) => {
                let mut j = i + 1;
                while j < bytes.len() && is_ident_continue(bytes[j]) {
                    j += 1;
                }
                tokens.push(Token {
                    tok: Tok::Ident(text[i..j].to_owned()),
                    line,
                });
                i = j;
            }
            _ => {
                errors.push(Error::at(
                    line,
                    format!("unexpected character `{}`", char::from(b)),
                ));
                i += 1;
            }
        }
    }
    if errors.is_empty() {
        Ok(tokens)
    } else {
        Err(errors)
    }
}

// ---------------------------------------------------------------------------
// Rule / definition model
// ---------------------------------------------------------------------------

/// One element of a rule body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// Reference to another definition (struct node, enum or token-set
    /// alias). The `line` is where the reference occurs, for diagnostics.
    Node { name: String, line: u32 },
    /// Quoted token, `'function'` or `';'`.
    Token { text: String, line: u32 },
    /// Token-class reference, `<ident>`.
    Class { name: String, line: u32 },
    /// `label:Rule` accessor label.
    Labeled { label: String, rule: Box<Rule> },
    /// Juxtaposition.
    Seq(Vec<Rule>),
    /// `A | B | C`.
    Alt(Vec<Rule>),
    /// `R?`.
    Opt(Box<Rule>),
    /// `R*`.
    Rep(Box<Rule>),
    /// `R,*` (`nonempty: false`) or `R,+` (`nonempty: true`). A trailing
    /// comma is the PHP parser's concern; the operator implies the `,` token.
    CommaList { rule: Box<Rule>, nonempty: bool },
    /// `R!`: a required child, held by the tree builder (convention A2).
    Required(Box<Rule>),
}

/// Walks `rule` and every nested rule, depth-first, calling `f` on each.
pub fn walk(rule: &Rule, f: &mut impl FnMut(&Rule)) {
    f(rule);
    match rule {
        Rule::Labeled { rule, .. }
        | Rule::Opt(rule)
        | Rule::Rep(rule)
        | Rule::CommaList { rule, .. }
        | Rule::Required(rule) => walk(rule, f),
        Rule::Seq(rules) | Rule::Alt(rules) => {
            for rule in rules {
                walk(rule, f);
            }
        }
        Rule::Node { .. } | Rule::Token { .. } | Rule::Class { .. } => {}
    }
}

/// A top-level definition: node, alternation or trait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    /// Definition name (`ClassDecl`, `Expr`, `HasAttributes`, ...).
    pub name: String,
    /// 1-based line of the definition header.
    pub line: u32,
    /// `trait Name = ...` mixin definitions (convention A5).
    pub is_trait: bool,
    /// Mixin opt-ins from the `Node : TraitA TraitB = ...` form.
    pub traits: Vec<String>,
    /// `None` only for an empty-bodied trait (`HasDocComment`).
    pub rule: Option<Rule>,
}

/// What a definition generates (convention A1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefClass {
    /// `Node = Rule`: gets a `SyntaxKind` plus typed accessors.
    StructNode,
    /// Alternation-only with at least one node-ref alternative (`Expr`,
    /// `Stmt`, and the mixed forms `Modifier` and `ScopedSelector`): a typed
    /// enum over the listed alternatives. Per convention A1 the enum gets no
    /// `SyntaxKind` of its own; at any tree position the variant node's kind
    /// appears instead. A1's words: "Every struct node maps 1:1 to a
    /// `SyntaxKind`. Alternation-only definitions generate enums ... whose
    /// variants are the listed nodes".
    Enum,
    /// Token-only alternation (`BinaryOp`, `AssignOp`, `NameRef`): a token
    /// accessor and a kind predicate, and no `SyntaxKind` of its own; the
    /// concrete token kinds appear in the tree.
    TokenAlt,
    /// `trait Name = ...`: generates a Rust trait with blanket accessors.
    Trait,
}

/// Classifies a definition per convention A1. A definition is
/// "alternation-only" when its top-level rule is an alternation in which
/// every alternative is a single atom (node ref, quoted token or token
/// class); anything else, including an alternation of sequences such as
/// `ArrayExpr`, is a struct node.
pub fn classify(def: &Def) -> DefClass {
    if def.is_trait {
        return DefClass::Trait;
    }
    if let Some(Rule::Alt(alts)) = &def.rule {
        let mut atomic = true;
        let mut all_tokens = true;
        for alt in alts {
            match alt {
                Rule::Token { .. } | Rule::Class { .. } => {}
                Rule::Node { .. } => all_tokens = false,
                _ => atomic = false,
            }
        }
        if atomic {
            return if all_tokens {
                DefClass::TokenAlt
            } else {
                DefClass::Enum
            };
        }
    }
    DefClass::StructNode
}

/// A parsed schema file.
#[derive(Debug, Clone)]
pub struct Grammar {
    /// Definitions in source order.
    pub defs: Vec<Def>,
}

impl Grammar {
    /// Struct-node definitions (the ones that get a `SyntaxKind`), in source
    /// order.
    pub fn struct_nodes(&self) -> Vec<&Def> {
        self.defs
            .iter()
            .filter(|def| classify(def) == DefClass::StructNode)
            .collect()
    }

    /// Every quoted token whose text starts alphabetically (or with `_`):
    /// the keyword set. PHP keywords are case-insensitive; the schema
    /// writes them lowercase (DSL header).
    pub fn keyword_tokens(&self) -> BTreeSet<String> {
        let mut keywords = BTreeSet::new();
        self.for_each_rule(&mut |rule| {
            if let Rule::Token { text, .. } = rule
                && text.as_bytes().first().copied().is_some_and(is_ident_start)
            {
                keywords.insert(text.clone());
            }
        });
        keywords
    }

    /// Every quoted token that is not a keyword (punctuation), mapped to
    /// the first line each occurs on (for diagnostics).
    pub fn symbol_tokens(&self) -> BTreeMap<String, u32> {
        let mut symbols = BTreeMap::new();
        self.for_each_rule(&mut |rule| {
            if let Rule::Token { text, line } = rule
                && !text.as_bytes().first().copied().is_some_and(is_ident_start)
            {
                symbols.entry(text.clone()).or_insert(*line);
            }
        });
        symbols
    }

    /// Every `<class>` reference, mapped to the first line each occurs on.
    pub fn token_classes_used(&self) -> BTreeMap<String, u32> {
        let mut classes = BTreeMap::new();
        self.for_each_rule(&mut |rule| {
            if let Rule::Class { name, line } = rule {
                classes.entry(name.clone()).or_insert(*line);
            }
        });
        classes
    }

    /// Whether any rule uses the `,*` / `,+` comma-list operators. The
    /// operator implies a `,` token in the tree even though `','` is never
    /// quoted.
    pub fn has_comma_lists(&self) -> bool {
        let mut found = false;
        self.for_each_rule(&mut |rule| {
            if matches!(rule, Rule::CommaList { .. }) {
                found = true;
            }
        });
        found
    }

    fn for_each_rule(&self, f: &mut impl FnMut(&Rule)) {
        for def in &self.defs {
            if let Some(rule) = &def.rule {
                walk(rule, f);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Reads DSL text into a [`Grammar`]. On failure, every error found, each
/// with its 1-based line; the reader re-synchronizes at the next
/// definition header.
pub fn parse(text: &str) -> Result<Grammar, Vec<Error>> {
    let tokens = lex(text)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        depth: 0,
    };
    let mut defs = Vec::new();
    let mut errors = Vec::new();
    while !parser.at_eof() {
        match parser.parse_def() {
            Ok(def) => defs.push(def),
            Err(error) => {
                errors.push(error);
                parser.recover_to_next_def();
            }
        }
    }
    if errors.is_empty() {
        Ok(Grammar { defs })
    } else {
        Err(errors)
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Parenthesis nesting depth; definition-start lookahead only applies at
    /// depth zero.
    depth: u32,
}

impl Parser {
    fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn tok(&self, i: usize) -> Option<&Tok> {
        self.tokens.get(i).map(|t| &t.tok)
    }

    fn peek(&self) -> Option<&Tok> {
        self.tok(self.pos)
    }

    fn cur_line(&self) -> u32 {
        self.tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .map_or(1, |t| t.line)
    }

    fn bump(&mut self) {
        self.pos += 1;
    }

    fn eat(&mut self, expected: &Tok) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &Tok, context: &str) -> Result<(), Error> {
        if self.eat(expected) {
            Ok(())
        } else {
            let found = self
                .peek()
                .map_or_else(|| "end of file".to_owned(), describe);
            Err(Error::at(
                self.cur_line(),
                format!("expected {} {context}, found {found}", describe(expected)),
            ))
        }
    }

    fn expect_ident(&mut self, context: &str) -> Result<(String, u32), Error> {
        let line = self.cur_line();
        if let Some(Tok::Ident(name)) = self.peek() {
            let name = name.clone();
            self.bump();
            Ok((name, line))
        } else {
            let found = self
                .peek()
                .map_or_else(|| "end of file".to_owned(), describe);
            Err(Error::at(
                line,
                format!("expected {context}, found {found}"),
            ))
        }
    }

    /// True when the token at `i` begins a new top-level definition:
    /// `Name =`, `Name : Trait... =`, or `trait Name =`. This lookahead is
    /// what terminates a rule body: the DSL has no end-of-definition
    /// delimiter, and a bare `=` cannot occur inside a rule, since the
    /// assignment operators are quoted, `'='`.
    ///
    /// A definition header is one line: the name, the mixin list if any,
    /// and the `=`. That is the disambiguation: a rule ending in
    /// `label:Node` followed by a definition `Name = ...` on the next line
    /// would otherwise scan as a mixin header `label : Node Name =`. Every
    /// header in `php.ungram`, and every one its DSL SYNTAX section shows,
    /// is one line, with rule continuations indented below it.
    fn is_def_start(&self, i: usize) -> bool {
        let Some(first) = self.tokens.get(i) else {
            return false;
        };
        let Tok::Ident(name) = &first.tok else {
            return false;
        };
        let line = first.line;
        let same_line = |j: usize| self.tokens.get(j).is_some_and(|t| t.line == line);
        if name == "trait" {
            return same_line(i + 2)
                && matches!(self.tok(i + 1), Some(Tok::Ident(_)))
                && matches!(self.tok(i + 2), Some(Tok::Eq));
        }
        match self.tok(i + 1) {
            Some(Tok::Eq) if same_line(i + 1) => true,
            Some(Tok::Colon) if same_line(i + 1) => {
                let mut j = i + 2;
                let mut saw_trait = false;
                while same_line(j) && matches!(self.tok(j), Some(Tok::Ident(_))) {
                    saw_trait = true;
                    j += 1;
                }
                saw_trait && same_line(j) && matches!(self.tok(j), Some(Tok::Eq))
            }
            _ => false,
        }
    }

    fn recover_to_next_def(&mut self) {
        self.depth = 0;
        self.bump();
        while !self.at_eof() && !self.is_def_start(self.pos) {
            self.bump();
        }
    }

    fn parse_def(&mut self) -> Result<Def, Error> {
        let line = self.cur_line();
        if let Some(Tok::Ident(name)) = self.peek()
            && name == "trait"
        {
            self.bump();
            let (name, _) = self.expect_ident("trait name after `trait`")?;
            self.expect(&Tok::Eq, "after trait name")?;
            // The empty-bodied trait (`HasDocComment`) is legal: its
            // rule ends immediately at the next definition (or EOF).
            let rule = if self.at_eof() || self.is_def_start(self.pos) {
                None
            } else {
                Some(self.parse_alt()?)
            };
            return Ok(Def {
                name,
                line,
                is_trait: true,
                traits: Vec::new(),
                rule,
            });
        }
        let (name, _) = self.expect_ident("definition name")?;
        let mut traits = Vec::new();
        if self.eat(&Tok::Colon) {
            while let Some(Tok::Ident(trait_name)) = self.peek() {
                traits.push(trait_name.clone());
                self.bump();
            }
            if traits.is_empty() {
                return Err(Error::at(
                    line,
                    format!("expected at least one mixin trait after `:` in `{name}`"),
                ));
            }
        }
        self.expect(&Tok::Eq, &format!("in definition of `{name}`"))?;
        if self.at_eof() || self.is_def_start(self.pos) {
            return Err(Error::at(
                line,
                format!("definition `{name}` has an empty rule body (only traits may be empty)"),
            ));
        }
        let rule = self.parse_alt()?;
        Ok(Def {
            name,
            line,
            is_trait: false,
            traits,
            rule: Some(rule),
        })
    }

    fn parse_alt(&mut self) -> Result<Rule, Error> {
        let mut branches = vec![self.parse_seq()?];
        while self.eat(&Tok::Pipe) {
            branches.push(self.parse_seq()?);
        }
        if branches.len() == 1 {
            Ok(branches.remove(0))
        } else {
            Ok(Rule::Alt(branches))
        }
    }

    fn parse_seq(&mut self) -> Result<Rule, Error> {
        let mut elems = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::Pipe | Tok::RParen) => break,
                Some(Tok::Ident(_)) if self.depth == 0 && self.is_def_start(self.pos) => break,
                Some(_) => {}
            }
            elems.push(self.parse_element()?);
        }
        match elems.len() {
            0 => Err(Error::at(self.cur_line(), "expected a rule element")),
            1 => Ok(elems.remove(0)),
            _ => Ok(Rule::Seq(elems)),
        }
    }

    fn parse_element(&mut self) -> Result<Rule, Error> {
        // `label:Rule`. A quoted `':'` lexes as a literal, so a bare colon
        // after an identifier is unambiguously a label (definition headers
        // were already excluded by the caller's lookahead).
        if matches!(self.tok(self.pos), Some(Tok::Ident(_)))
            && matches!(self.tok(self.pos + 1), Some(Tok::Colon))
        {
            let (label, _) = self.expect_ident("label")?;
            self.bump(); // ':'
            let rule = self.parse_postfixed()?;
            return Ok(Rule::Labeled {
                label,
                rule: Box::new(rule),
            });
        }
        self.parse_postfixed()
    }

    fn parse_postfixed(&mut self) -> Result<Rule, Error> {
        let line = self.cur_line();
        let atom = self.parse_atom()?;
        if self.eat(&Tok::Bang) {
            // In php.ungram, `!` only ever follows node references and
            // token-class references (verified by survey); convention A2's
            // semantics ("construction anchored on the child") make it
            // meaningless on groups, and contradictory with `?`/`*`/`,*`.
            if !matches!(atom, Rule::Node { .. } | Rule::Class { .. }) {
                return Err(Error::at(
                    line,
                    "`!` (required marker) may only follow a node or token-class reference",
                ));
            }
            if matches!(self.peek(), Some(Tok::Question | Tok::Star | Tok::Comma)) {
                return Err(Error::at(
                    line,
                    "`!` cannot be combined with `?`, `*`, or `,*`/`,+`",
                ));
            }
            return Ok(Rule::Required(Box::new(atom)));
        }
        if self.eat(&Tok::Question) {
            return Ok(Rule::Opt(Box::new(atom)));
        }
        if self.eat(&Tok::Star) {
            return Ok(Rule::Rep(Box::new(atom)));
        }
        if self.eat(&Tok::Comma) {
            if self.eat(&Tok::Star) {
                return Ok(Rule::CommaList {
                    rule: Box::new(atom),
                    nonempty: false,
                });
            }
            if self.eat(&Tok::Plus) {
                return Ok(Rule::CommaList {
                    rule: Box::new(atom),
                    nonempty: true,
                });
            }
            return Err(Error::at(
                line,
                "expected `*` or `+` after `,` (comma-list operators are `R,*` and `R,+`)",
            ));
        }
        Ok(atom)
    }

    fn parse_atom(&mut self) -> Result<Rule, Error> {
        let line = self.cur_line();
        match self.peek().cloned() {
            Some(Tok::Literal(text)) => {
                self.bump();
                Ok(Rule::Token { text, line })
            }
            Some(Tok::Class(name)) => {
                self.bump();
                Ok(Rule::Class { name, line })
            }
            Some(Tok::Ident(name)) => {
                self.bump();
                Ok(Rule::Node { name, line })
            }
            Some(Tok::LParen) => {
                self.bump();
                self.depth += 1;
                let inner = self.parse_alt()?;
                self.depth -= 1;
                if self.eat(&Tok::RParen) {
                    Ok(inner)
                } else {
                    Err(Error::at(line, "unclosed `(` in rule"))
                }
            }
            Some(other) => Err(Error::at(
                line,
                format!("expected a rule atom, found {}", describe(&other)),
            )),
            None => Err(Error::at(line, "unexpected end of file inside a rule")),
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Structural validation: every referenced node and trait is defined, a
/// trait is used in mixin position only, and no definition is duplicated.
/// The token-class cross-check lives in [`super::kind`], since the declared
/// class list is the emitter's, not the DSL's.
pub fn validate(grammar: &Grammar) -> Vec<Error> {
    let mut errors = Vec::new();
    let mut by_name: BTreeMap<&str, &Def> = BTreeMap::new();
    for def in &grammar.defs {
        if let Some(first) = by_name.get(def.name.as_str()) {
            errors.push(Error::at(
                def.line,
                format!(
                    "duplicate definition of `{}` (first defined on line {})",
                    def.name, first.line
                ),
            ));
        } else {
            by_name.insert(&def.name, def);
        }
    }
    for def in &grammar.defs {
        for trait_name in &def.traits {
            match by_name.get(trait_name.as_str()) {
                None => errors.push(Error::at(
                    def.line,
                    format!("`{}` opts into undefined trait `{trait_name}`", def.name),
                )),
                Some(target) if !target.is_trait => errors.push(Error::at(
                    def.line,
                    format!(
                        "`{}` opts into `{trait_name}`, which is not a trait",
                        def.name
                    ),
                )),
                Some(_) => {}
            }
        }
        if let Some(rule) = &def.rule {
            walk(rule, &mut |r| {
                if let Rule::Node { name, line } = r {
                    match by_name.get(name.as_str()) {
                        None => errors.push(Error::at(
                            *line,
                            format!("reference to undefined rule `{name}`"),
                        )),
                        Some(target) if target.is_trait => errors.push(Error::at(
                            *line,
                            format!(
                                "trait `{name}` referenced in rule position (traits may only \
                                 appear after `:` in a definition header)"
                            ),
                        )),
                        Some(_) => {}
                    }
                }
            });
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// Tests — one per DSL construct, plus failure cases asserting line numbers.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> Grammar {
        match parse(text) {
            Ok(grammar) => grammar,
            Err(errors) => panic!("expected successful parse, got {errors:?}"),
        }
    }

    fn first_error(text: &str) -> Error {
        match parse(text) {
            Ok(_) => panic!("expected a parse error"),
            Err(errors) => errors.into_iter().next().expect("at least one error"),
        }
    }

    #[test]
    fn struct_node_with_sequence_and_tokens() {
        let g = parse_ok("Foo = 'function' name:<ident> ';'\n");
        assert_eq!(g.defs.len(), 1);
        assert_eq!(classify(&g.defs[0]), DefClass::StructNode);
        assert_eq!(g.keyword_tokens().len(), 1);
        assert!(g.symbol_tokens().contains_key(";"));
    }

    #[test]
    fn alternation_of_node_refs_is_enum_without_kind() {
        let g = parse_ok("E = A | B\nA = 'a'\nB = 'b'\n");
        assert_eq!(classify(&g.defs[0]), DefClass::Enum);
        // Convention A1: alternation-only definitions do not get a
        // SyntaxKind, so they are not struct nodes.
        let nodes: Vec<&str> = g.struct_nodes().iter().map(|d| d.name.as_str()).collect();
        assert_eq!(nodes, ["A", "B"]);
    }

    #[test]
    fn token_only_alternation_is_token_alias() {
        let g = parse_ok("Op = '+' | '-' | <cast>\nUse = x:Op\n");
        assert_eq!(classify(&g.defs[0]), DefClass::TokenAlt);
        assert_eq!(classify(&g.defs[1]), DefClass::StructNode);
    }

    #[test]
    fn mixed_token_and_node_alternation_is_enum() {
        // Modeled on `Modifier` / `ScopedSelector` in php.ungram.
        let g = parse_ok("M = 'public' | N | 'static'\nN = 'x'\n");
        assert_eq!(classify(&g.defs[0]), DefClass::Enum);
    }

    #[test]
    fn alternation_of_sequences_is_struct_node() {
        // Modeled on `ArrayExpr`: alternatives are not single atoms.
        let g = parse_ok("A = ( 'array' '(' ')' | '[' ']' )\n");
        assert_eq!(classify(&g.defs[0]), DefClass::StructNode);
    }

    #[test]
    fn traits_including_empty_bodied() {
        let g = parse_ok(
            "trait HasA = attrs:B*\ntrait HasDoc =   // trivia-backed\nFoo : HasA HasDoc = 'x'\nB = 'y'\n",
        );
        assert!(g.defs[0].is_trait);
        assert!(g.defs[0].rule.is_some());
        assert!(g.defs[1].is_trait);
        assert!(g.defs[1].rule.is_none(), "empty-bodied trait has no rule");
        assert_eq!(g.defs[2].traits, ["HasA", "HasDoc"]);
        assert!(validate(&g).is_empty());
    }

    #[test]
    fn grouping_optional_and_repetition() {
        let g = parse_ok("Foo = 'a' ( 'b' | <cast> )? Bar* x:Bar?\nBar = 'z'\n");
        let Some(Rule::Seq(elems)) = &g.defs[0].rule else {
            panic!("expected sequence");
        };
        assert_eq!(elems.len(), 4);
        assert!(matches!(&elems[1], Rule::Opt(inner) if matches!(**inner, Rule::Alt(_))));
        assert!(matches!(&elems[2], Rule::Rep(_)));
        assert!(
            matches!(&elems[3], Rule::Labeled { label, rule } if label == "x" && matches!(**rule, Rule::Opt(_)))
        );
    }

    #[test]
    fn comma_lists_zero_and_one_or_more() {
        let g = parse_ok("Foo = '(' Bar,* ')'\nBaz = Bar,+ ';'\nBar = 'x'\n");
        assert!(g.has_comma_lists());
        let Some(Rule::Seq(elems)) = &g.defs[0].rule else {
            panic!("expected sequence");
        };
        assert!(matches!(
            &elems[1],
            Rule::CommaList {
                nonempty: false,
                ..
            }
        ));
        let Some(Rule::Seq(elems)) = &g.defs[1].rule else {
            panic!("expected sequence");
        };
        assert!(matches!(&elems[0], Rule::CommaList { nonempty: true, .. }));
    }

    #[test]
    fn labels_and_required_marker() {
        let g = parse_ok("Foo = name:<ident>! body:Bar\nBar = 'x'\n");
        let Some(Rule::Seq(elems)) = &g.defs[0].rule else {
            panic!("expected sequence");
        };
        assert!(matches!(
            &elems[0],
            Rule::Labeled { label, rule } if label == "name" && matches!(**rule, Rule::Required(_))
        ));
    }

    #[test]
    fn required_marker_on_unlabeled_node_ref() {
        let g = parse_ok("Foo = Bar! ';'\nBar = 'x'\n");
        let Some(Rule::Seq(elems)) = &g.defs[0].rule else {
            panic!("expected sequence");
        };
        assert!(matches!(&elems[0], Rule::Required(_)));
    }

    #[test]
    fn comments_standalone_and_trailing() {
        let g = parse_ok(
            "// leading comment\nFoo = 'a'   // trailing comment with 'quotes' and = signs\n// done\n",
        );
        assert_eq!(g.defs.len(), 1);
        assert!(matches!(
            g.defs[0].rule,
            Some(Rule::Token { ref text, .. }) if text == "a"
        ));
    }

    #[test]
    fn multiline_alternation_continues_until_next_def() {
        let g = parse_ok("M = 'a'\n  | 'b'\n  | N\nN = 'c'\n");
        let Some(Rule::Alt(alts)) = &g.defs[0].rule else {
            panic!("expected alternation");
        };
        assert_eq!(alts.len(), 3);
    }

    #[test]
    fn error_undefined_reference_has_line() {
        let g = parse_ok("Foo = 'x'\nBaz = Foo Missing\n");
        let errors = validate(&g);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, Some(2));
        assert!(
            errors[0].message.contains("Missing"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn error_duplicate_definition_has_line() {
        let g = parse_ok("Foo = 'x'\n\nFoo = 'y'\n");
        let errors = validate(&g);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, Some(3));
        assert!(
            errors[0].message.contains("line 1"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn error_trait_in_rule_position() {
        let g = parse_ok("trait HasA = x:B\nFoo = HasA\nB = 'b'\n");
        let errors = validate(&g);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, Some(2));
    }

    #[test]
    fn error_undefined_mixin_trait() {
        let g = parse_ok("Foo : Nope = 'x'\n");
        let errors = validate(&g);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, Some(1));
        assert!(errors[0].message.contains("Nope"), "{}", errors[0].message);
    }

    #[test]
    fn error_unterminated_literal_has_line() {
        let e = first_error("Foo = 'a'\nBar = 'unterminated\n");
        assert_eq!(e.line, Some(2));
        assert!(e.message.contains("unterminated"), "{}", e.message);
    }

    #[test]
    fn error_malformed_token_class_has_line() {
        let e = first_error("Foo = 'a'\n\nBar = <oops\n");
        assert_eq!(e.line, Some(3));
        assert!(e.message.contains("token class"), "{}", e.message);
    }

    #[test]
    fn error_unclosed_group_has_line() {
        let e = first_error("Foo = ( 'a' | 'b'\n");
        assert_eq!(e.line, Some(1));
        assert!(e.message.contains("unclosed"), "{}", e.message);
    }

    #[test]
    fn error_bare_comma_has_line() {
        let e = first_error("Foo = 'a'\nBar = Foo, Foo\n");
        assert_eq!(e.line, Some(2));
        assert!(e.message.contains("comma-list"), "{}", e.message);
    }

    #[test]
    fn error_required_marker_on_literal() {
        let e = first_error("Foo = 'x'!\n");
        assert_eq!(e.line, Some(1));
        assert!(e.message.contains("required marker"), "{}", e.message);
    }

    #[test]
    fn error_empty_node_rule() {
        let e = first_error("Foo =\nBar = 'x'\n");
        assert_eq!(e.line, Some(1));
        assert!(e.message.contains("empty rule body"), "{}", e.message);
    }

    #[test]
    fn error_recovery_reports_multiple_definitions() {
        let Err(errors) = parse("Foo = (\nBar = 'ok'\nBaz = ,\n") else {
            panic!("expected errors")
        };
        assert!(errors.len() >= 2, "parser should resynchronize: {errors:?}");
    }

    #[test]
    fn keyword_and_symbol_split() {
        let g = parse_ok("Foo = '__halt_compiler' '#[' 'fn' '...' ';'\n");
        let keywords = g.keyword_tokens();
        assert!(keywords.contains("__halt_compiler"));
        assert!(keywords.contains("fn"));
        let symbols = g.symbol_tokens();
        assert!(symbols.contains_key("#["));
        assert!(symbols.contains_key("..."));
        assert!(symbols.contains_key(";"));
    }

    #[test]
    fn label_at_rule_end_not_confused_with_mixin_header() {
        // `operand:Expr` followed by `PrintExpr =` on the NEXT line must not
        // scan as a mixin header `operand : Expr PrintExpr =` (definition
        // headers are single-line; see `is_def_start`). Regression test for
        // the real php.ungram layout.
        let g = parse_ok(
            "CloneExpr = 'clone' operand:Expr\nPrintExpr = 'print' operand:Expr\nExpr = CloneExpr | PrintExpr\n",
        );
        assert_eq!(g.defs.len(), 3);
        assert!(validate(&g).is_empty());
        let Some(Rule::Seq(elems)) = &g.defs[0].rule else {
            panic!("expected sequence");
        };
        assert_eq!(elems.len(), 2);
    }

    #[test]
    fn mixin_header_not_confused_with_label() {
        // `ParamList = '(' P,* ')'` followed by `P : T =` — the parser must
        // treat `P : T =` as a new definition, not a label inside ParamList.
        let g = parse_ok("trait T = x:B\nParamList = '(' P,* ')'\nP : T = 'p'\nB = 'b'\n");
        assert_eq!(g.defs.len(), 4);
        assert_eq!(g.defs[2].traits, ["T"]);
        assert!(validate(&g).is_empty());
    }
}
