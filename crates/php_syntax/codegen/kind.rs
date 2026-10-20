//! `SyntaxKind` emitted from the parsed schema into `OUT_DIR/syntax_kind.rs`
//! (design §4.9, ADR-0003): punctuation from [`PUNCT_TABLE`], keywords from
//! the schema's alphabetic literals, the token classes from
//! [`TOKEN_CLASSES`], the kinds the schema cannot express from [`EXTRAS`],
//! then one kind per struct node in schema order; and on the enum, three
//! discriminant-range predicates and `from_keyword`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::ungram::{Error, Grammar};

/// The token classes the "TOKEN CLASSES" block of `php.ungram`'s header
/// declares, in header order.
///
/// That block is prose inside a comment, since the DSL has no declaration
/// form for a token class, so the list is mirrored here and held to the
/// `<class>` references the rules make, in both directions
/// ([`check_token_classes`]): a class used in a rule but missing here
/// fails the build (the schema grew and this list did not), and a class
/// listed here but used nowhere fails it too (a stale entry, or a rule
/// dropped without revisiting the set). Editing this list is the one act
/// the cross-check demands.
pub const TOKEN_CLASSES: &[&str] = &[
    "ident",
    "member_ident",
    "variable",
    "name_qualified",
    "name_fully_qualified",
    "name_relative",
    "int",
    "float",
    "single_quoted",
    "string_text",
    "heredoc_start",
    "heredoc_end",
    "encaps_int",
    "magic_const",
    "cast",
    "cast_removed",
    "inline_html",
    "halt_data",
    "open_tag_echo",
];

/// The symbol to its variant name (convention A8 says a kind is
/// SCREAMING, and a symbol has no mechanical spelling). The build fails on
/// a symbol the table does not name, so new punctuation is named by hand,
/// and on an entry the grammar no longer uses.
const PUNCT_TABLE: &[(&str, &str)] = &[
    ("!", "BANG"),
    ("!=", "BANG_EQ"),
    ("!==", "BANG_EQ_EQ"),
    ("\"", "DOUBLE_QUOTE"),
    ("#[", "ATTRIBUTE_OPEN"),
    ("$", "DOLLAR"),
    ("${", "DOLLAR_L_CURLY"),
    ("%", "PERCENT"),
    ("%=", "PERCENT_EQ"),
    ("&", "AMP"),
    ("&&", "AMP_AMP"),
    ("&=", "AMP_EQ"),
    ("(", "L_PAREN"),
    (")", "R_PAREN"),
    ("*", "STAR"),
    ("**", "STAR_STAR"),
    ("**=", "STAR_STAR_EQ"),
    ("*=", "STAR_EQ"),
    ("+", "PLUS"),
    ("++", "PLUS_PLUS"),
    ("+=", "PLUS_EQ"),
    ("-", "MINUS"),
    ("--", "MINUS_MINUS"),
    ("-=", "MINUS_EQ"),
    ("->", "ARROW"),
    (".", "DOT"),
    ("...", "DOT_DOT_DOT"),
    (".=", "DOT_EQ"),
    ("/", "SLASH"),
    ("/=", "SLASH_EQ"),
    (":", "COLON"),
    ("::", "COLON_COLON"),
    (";", "SEMICOLON"),
    ("<", "LT"),
    ("<<", "SHL"),
    ("<<=", "SHL_EQ"),
    ("<=", "LT_EQ"),
    ("<=>", "SPACESHIP"),
    ("<>", "LT_GT"),
    ("=", "EQ"),
    ("==", "EQ_EQ"),
    ("===", "EQ_EQ_EQ"),
    ("=>", "FAT_ARROW"),
    (">", "GT"),
    (">=", "GT_EQ"),
    (">>", "SHR"),
    (">>=", "SHR_EQ"),
    ("?", "QUESTION"),
    ("?->", "NULLSAFE_ARROW"),
    ("??", "QUESTION_QUESTION"),
    ("??=", "QUESTION_QUESTION_EQ"),
    ("@", "AT"),
    ("[", "L_BRACK"),
    ("\\\\", "BACKSLASH"),
    ("]", "R_BRACK"),
    ("^", "CARET"),
    ("^=", "CARET_EQ"),
    ("`", "BACKQUOTE"),
    ("{", "L_CURLY"),
    ("|", "PIPE"),
    ("|=", "PIPE_EQ"),
    ("|>", "PIPE_GT"),
    ("||", "PIPE_PIPE"),
    ("}", "R_CURLY"),
    ("~", "TILDE"),
];

/// The variant name of a symbol, shared with the typed-node emitter so the
/// two spell one `SyntaxKind` the same way.
pub fn punct_variant(symbol: &str) -> Option<&'static str> {
    PUNCT_TABLE
        .iter()
        .find(|(s, _)| *s == symbol)
        .map(|(_, v)| *v)
}

/// The `,*` / `,+` list operators imply a `,` token in the tree even though
/// `','` is never quoted in the schema, so `COMMA` is emitted whenever the
/// grammar uses a comma list; it is not one of the [`EXTRAS`], since the
/// grammar does express it, through the operator.
const COMMA: (&str, &str) = (",", "COMMA");

/// The kinds the schema cannot express: whitespace and comments, which
/// convention A6 keeps out of the schema; the open and close tags, which
/// are structural tokens the parser owns (A6); and `ERROR` and `EOF`. None
/// occurs as a rule atom, and [`collect_kinds`] fails the build the day the
/// grammar expresses one, so the entry is removed here.
///
/// The order carries the third column: `is_trivia` is emitted as one
/// discriminant range, so every entry marked `true` must sit in one block.
/// [`trivia_extras_contiguous`] holds that; an entry marked `true` appended
/// after `EOF` fails the build rather than widening the range in silence.
const EXTRAS: &[(&str, &str, bool)] = &[
    (
        "WHITESPACE",
        "Whitespace run between tokens (trivia, convention A6).",
        true,
    ),
    (
        "LINE_COMMENT",
        "`// ...` or `# ...` comment; terminates at newline or `?>` (trivia).",
        true,
    ),
    ("BLOCK_COMMENT", "`/* ... */` comment (trivia).", true),
    (
        "DOC_COMMENT",
        "`/** ... */` docblock (trivia; surfaced via the `HasDocComment` mixin, convention A5).",
        true,
    ),
    (
        "HEREDOC_INDENT",
        "Flexible-heredoc indentation strip record (trivia; preserves G2 losslessness).",
        true,
    ),
    (
        "SHEBANG",
        "A file's `#!` first line the scanner's `SHEBANG` state passes over when the host sets `CG(skip_shebang)` (trivia).",
        true,
    ),
    (
        "OPEN_TAG",
        "`<?php` / `<?` open tag — structural token owned by parser glue (convention A6).",
        false,
    ),
    (
        "CLOSE_TAG",
        "`?>` close tag; also acts as an implicit `;` via parser glue (convention A6).",
        false,
    ),
    (
        "ERROR",
        "Token or node produced by lexer/parser error recovery.",
        false,
    ),
    ("EOF", "End of input (cursor sentinel).", false),
];

/// How many kinds of each section the emitter writes, for the floors
/// `tests/schema.rs` holds the schema to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindCounts {
    pub nodes: usize,
    pub keywords: usize,
    pub puncts: usize,
    pub classes: usize,
    pub extras: usize,
}

impl KindCounts {
    pub fn total(self) -> usize {
        self.nodes + self.keywords + self.puncts + self.classes + self.extras
    }
}

struct Kinds {
    /// `(symbol, variant)` sorted by variant name.
    puncts: Vec<(String, String)>,
    /// `(keyword text, variant)` sorted by variant name.
    keywords: Vec<(String, String)>,
    /// `(class name, variant)` in declared (header) order.
    classes: Vec<(String, String)>,
    /// `(variant, doc, is_trivia)` in [`EXTRAS`] order.
    extras: Vec<(&'static str, &'static str, bool)>,
    /// `(CamelCase node name, variant)` in schema order.
    nodes: Vec<(String, String)>,
}

impl Kinds {
    fn counts(&self) -> KindCounts {
        KindCounts {
            nodes: self.nodes.len(),
            keywords: self.keywords.len(),
            puncts: self.puncts.len(),
            classes: self.classes.len(),
            extras: self.extras.len(),
        }
    }
}

/// `ClassDecl` to `CLASS_DECL` (convention A8).
pub fn screaming_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for ch in name.chars() {
        if ch.is_ascii_uppercase() && !out.is_empty() {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}

/// `ClassDecl` to `class_decl`: the spelling of a typed node's field
/// accessor, for the typed-node emitter that follows this one.
#[must_use]
pub fn snake_case(name: &str) -> String {
    screaming_snake(name).to_ascii_lowercase()
}

/// `'function'` to `FUNCTION_KW`; `'__halt_compiler'` to `HALT_COMPILER_KW`.
/// The leading underscores leave the variant name alone; `from_keyword`
/// still matches the full spelling.
pub fn keyword_variant(text: &str) -> String {
    let mut out = text.trim_start_matches('_').to_ascii_uppercase();
    out.push_str("_KW");
    out
}

/// Hold the declared token-class list to the `<class>` references the
/// rules make, in both directions (see [`TOKEN_CLASSES`]).
pub fn check_token_classes(grammar: &Grammar, declared: &[&str]) -> Vec<Error> {
    let used = grammar.token_classes_used();
    let mut errors = Vec::new();
    for (name, line) in &used {
        if !declared.contains(&name.as_str()) {
            errors.push(Error::at(
                *line,
                format!(
                    "token class `<{name}>` is used in rules but not declared in build.rs's \
                     TOKEN_CLASSES list (mirror of the php.ungram TOKEN CLASSES header block)"
                ),
            ));
        }
    }
    for name in declared {
        if !used.contains_key(*name) {
            errors.push(Error::global(format!(
                "token class `<{name}>` is declared in build.rs's TOKEN_CLASSES list but never \
                 used in php.ungram rules (stale entry?)"
            )));
        }
    }
    errors
}

/// Whether the entries of an extras list marked `true` form one block. The
/// emitted `is_trivia` is a first-to-last discriminant range over them
/// ([`emit_range_predicate`]), so a gap, such as an entry appended after
/// `EOF`, would widen the range over every entry in between. The enum's
/// rustdoc says the generator guarantees the grouping; this is that
/// guarantee.
fn trivia_extras_contiguous(extras: &[(&str, &str, bool)]) -> bool {
    let mut runs = 0;
    let mut prev = false;
    for &(_, _, trivia) in extras {
        if trivia && !prev {
            runs += 1;
        }
        prev = trivia;
    }
    runs <= 1
}

fn collect_kinds(grammar: &Grammar) -> Result<Kinds, Vec<Error>> {
    let mut errors = check_token_classes(grammar, TOKEN_CLASSES);

    if !trivia_extras_contiguous(EXTRAS) {
        errors.push(Error::global(
            "trivia-flagged EXTRAS entries must form one contiguous run: `is_trivia` is a \
             contiguous-discriminant range check, and a gap would silently widen it to \
             include the non-trivia extras in between — reorder EXTRAS so all trivia \
             entries are adjacent",
        ));
    }

    let symbols = grammar.symbol_tokens();
    let mut puncts: Vec<(String, String)> = Vec::new();
    for (symbol, line) in &symbols {
        match PUNCT_TABLE.iter().find(|(s, _)| s == symbol) {
            Some((_, variant)) => puncts.push((symbol.clone(), (*variant).to_owned())),
            None => errors.push(Error::at(
                *line,
                format!(
                    "symbol literal `'{symbol}'` has no entry in build.rs's punctuation name \
                     table; name it deliberately in PUNCT_TABLE"
                ),
            )),
        }
    }
    for (symbol, _) in PUNCT_TABLE {
        if !symbols.contains_key(*symbol) {
            errors.push(Error::global(format!(
                "punctuation table entry `'{symbol}'` is no longer used in php.ungram \
                 (stale entry?)"
            )));
        }
    }
    if grammar.has_comma_lists() {
        puncts.push((COMMA.0.to_owned(), COMMA.1.to_owned()));
    }
    puncts.sort_by(|a, b| a.1.cmp(&b.1));

    let keywords: Vec<(String, String)> = {
        let mut kws: Vec<(String, String)> = grammar
            .keyword_tokens()
            .into_iter()
            .map(|text| {
                let variant = keyword_variant(&text);
                (text, variant)
            })
            .collect();
        kws.sort_by(|a, b| a.1.cmp(&b.1));
        kws
    };

    let classes: Vec<(String, String)> = TOKEN_CLASSES
        .iter()
        .map(|name| ((*name).to_owned(), name.to_ascii_uppercase()))
        .collect();

    let nodes: Vec<(String, String)> = grammar
        .struct_nodes()
        .iter()
        .map(|def| (def.name.clone(), screaming_snake(&def.name)))
        .collect();

    // Global uniqueness check across every section, including EXTRAS. This
    // is also what enforces "only add as extras what is genuinely absent":
    // the moment the grammar expresses (say) OPEN_TAG, the extra collides
    // and codegen fails until the EXTRAS entry is removed.
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    let sections: [(&str, Vec<&str>); 5] = [
        (
            "punctuation",
            puncts.iter().map(|(_, v)| v.as_str()).collect(),
        ),
        (
            "keyword",
            keywords.iter().map(|(_, v)| v.as_str()).collect(),
        ),
        (
            "token class",
            classes.iter().map(|(_, v)| v.as_str()).collect(),
        ),
        ("extra", EXTRAS.iter().map(|(v, _, _)| *v).collect()),
        ("node", nodes.iter().map(|(_, v)| v.as_str()).collect()),
    ];
    for (section, variants) in &sections {
        for variant in variants {
            if let Some(first) = seen.insert(variant, section) {
                errors.push(Error::global(format!(
                    "SyntaxKind name collision: `{variant}` is generated both as a {first} \
                     and as a {section}"
                )));
            }
        }
    }

    if errors.is_empty() {
        Ok(Kinds {
            puncts,
            keywords,
            classes,
            extras: EXTRAS.to_vec(),
            nodes,
        })
    } else {
        Err(errors)
    }
}

/// The text of `syntax_kind.rs` for `grammar`, or every reason it cannot
/// be written. Nothing formats it: it is written to `OUT_DIR` and included,
/// never committed.
pub fn generate(grammar: &Grammar) -> Result<String, Vec<Error>> {
    let kinds = collect_kinds(grammar)?;
    let mut buf = String::new();
    emit_header(&mut buf);
    emit_enum(&mut buf, &kinds);
    emit_impl(&mut buf, &kinds);
    Ok(buf)
}

/// The counts of the kinds [`generate`] would emit.
pub fn kind_counts(grammar: &Grammar) -> Result<KindCounts, Vec<Error>> {
    collect_kinds(grammar).map(|kinds| kinds.counts())
}

fn emit_header(buf: &mut String) {
    buf.push_str(
        "// Generated by build.rs from php.ungram; never committed (design §10).\n\
         // Edit the schema, not this file.\n\
         \n",
    );
}

fn emit_enum(buf: &mut String, kinds: &Kinds) {
    buf.push_str(
        "/// The kind of every token of the token stream and every node of the\n\
         /// tree (design §4.9), one variant per punctuation symbol, keyword,\n\
         /// token class, extra and struct node of `php.ungram`.\n\
         ///\n\
         /// Variants are `SCREAMING_SNAKE` per schema convention A8. The layout\n\
         /// is punctuation, keywords, token classes, extras, then nodes; the\n\
         /// `is_*` predicates are discriminant ranges over that grouping, which\n\
         /// the generator guarantees.\n\
         #[allow(non_camel_case_types)]\n\
         #[allow(clippy::upper_case_acronyms)]\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
         #[repr(u16)]\n\
         pub enum SyntaxKind {\n",
    );
    buf.push_str("    // --- punctuation ---\n");
    for (symbol, variant) in &kinds.puncts {
        let shown = if symbol == "," {
            "`,` (implied by the schema's `,*` / `,+` list operators)".to_owned()
        } else if symbol.contains('`') {
            // A bare backtick inside `...` would open a markdown code fence
            // (and rustdoc would treat the rest of the doc as a doctest).
            format!("`` {symbol} `` (backquote)")
        } else {
            format!("`{symbol}`")
        };
        writeln!(buf, "    /// {shown}\n    {variant},").ok();
    }
    buf.push_str("    // --- keywords (case-insensitive; see `from_keyword`) ---\n");
    for (text, variant) in &kinds.keywords {
        writeln!(buf, "    /// `{text}`\n    {variant},").ok();
    }
    buf.push_str("    // --- token classes ---\n");
    for (name, variant) in &kinds.classes {
        if name == "member_ident" {
            buf.push_str(
                "    /// Token class `<member_ident>`: `<ident>` plus a semi-reserved\n    \
                 /// keyword in member-name position. A parse-time classification:\n    \
                 /// the lexer emits `IDENT` or a keyword kind, and the grammar's\n    \
                 /// contextual-keyword rules decide.\n",
            );
        } else {
            writeln!(buf, "    /// Token class `<{name}>`.").ok();
        }
        writeln!(buf, "    {variant},").ok();
    }
    buf.push_str("    // --- extras: kinds the schema cannot express (see header) ---\n");
    for (variant, doc, _) in &kinds.extras {
        writeln!(buf, "    /// {doc}\n    {variant},").ok();
    }
    buf.push_str("    // --- nodes (schema order) ---\n");
    for (name, variant) in &kinds.nodes {
        writeln!(buf, "    /// `{name}`\n    {variant},").ok();
    }
    buf.push_str("}\n\n");
}

fn emit_impl(buf: &mut String, kinds: &Kinds) {
    buf.push_str("impl SyntaxKind {\n");
    emit_range_predicate(
        buf,
        "is_keyword",
        "Is this kind a PHP keyword token?",
        &kinds
            .keywords
            .iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>(),
    );
    emit_range_predicate(
        buf,
        "is_punct",
        "Is this kind a punctuation or operator token?",
        &kinds
            .puncts
            .iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>(),
    );
    let trivia: Vec<&str> = kinds
        .extras
        .iter()
        .filter(|(_, _, t)| *t)
        .map(|(v, _, _)| *v)
        .collect();
    emit_range_predicate(
        buf,
        "is_trivia",
        "Is this kind whitespace, a comment or a heredoc indentation record: present in the token stream, hidden from the parser's cursor and from the tree (convention A6)?",
        &trivia,
    );
    emit_from_keyword(buf, kinds);
    buf.push_str("}\n");
}

fn emit_range_predicate(buf: &mut String, name: &str, doc: &str, variants: &[&str]) {
    writeln!(buf, "    /// {doc}").ok();
    if let (Some(first), Some(last)) = (variants.first(), variants.last()) {
        writeln!(
            buf,
            "    pub fn {name}(self) -> bool {{\n        \
             (Self::{first} as u16..=Self::{last} as u16).contains(&(self as u16))\n    \
             }}"
        )
        .ok();
    } else {
        writeln!(buf, "    pub fn {name}(self) -> bool {{ false }}").ok();
    }
}

fn emit_from_keyword(buf: &mut String, kinds: &Kinds) {
    buf.push_str(
        "    /// The keyword kind of an identifier, if it is one. PHP keywords\n    \
         /// match case-insensitively (`php.ungram`'s header: \"the lexer is\n    \
         /// case-insensitive\"). The lexer asks once per name, so the\n    \
         /// name's length and first letter choose the few keywords it is\n    \
         /// compared with, and nothing is copied.\n",
    );
    buf.push_str("    pub fn from_keyword(ident: &[u8]) -> Option<Self> {\n");
    if kinds.keywords.is_empty() {
        buf.push_str("        let _ = ident;\n        None\n    }\n");
        return;
    }
    buf.push_str(
        "        let first = ident.first()?.to_ascii_lowercase();\n        \
         let kind = match (ident.len(), first) {\n",
    );
    let mut by_text: Vec<(&str, &str)> = kinds
        .keywords
        .iter()
        .map(|(t, v)| (t.as_str(), v.as_str()))
        .collect();
    by_text.sort_by(|a, b| a.0.cmp(b.0));
    for (text, variant) in by_text {
        let first = text.as_bytes().first().copied().unwrap_or(b'_') as char;
        writeln!(
            buf,
            "            ({}, b'{first}') if ident.eq_ignore_ascii_case(b\"{text}\") => Self::{variant},",
            text.len()
        )
        .ok();
    }
    buf.push_str("            _ => return None,\n        };\n        Some(kind)\n    }\n");
}

#[cfg(test)]
mod tests {
    use super::super::ungram::parse;
    use super::*;

    #[test]
    fn screaming_snake_conversion() {
        assert_eq!(screaming_snake("ClassDecl"), "CLASS_DECL");
        assert_eq!(screaming_snake("InlineHtmlStmt"), "INLINE_HTML_STMT");
        assert_eq!(screaming_snake("IfStmt"), "IF_STMT");
        assert_eq!(
            screaming_snake("DollarBraceInterpolation"),
            "DOLLAR_BRACE_INTERPOLATION"
        );
    }

    /// `snake_case` agrees with the open-coded loop it replaced in Mahout,
    /// whose separator guard was `i != 0` where [`screaming_snake`]'s is
    /// `!out.is_empty()`: every iteration of either pushes at least one
    /// character, so the accumulator is empty exactly when the index is 0.
    #[test]
    fn snake_case_agrees_with_the_open_coded_loop_it_replaced() {
        fn open_coded(name: &str) -> String {
            let mut out = String::new();
            for (i, c) in name.chars().enumerate() {
                if c.is_ascii_uppercase() {
                    if i != 0 {
                        out.push('_');
                    }
                    out.push(c.to_ascii_lowercase());
                } else {
                    out.push(c);
                }
            }
            out
        }
        // MIR union spec names, ungram rule names, and the shapes that
        // would separate the two guards if anything could: a leading
        // lowercase, an all-caps run, a single letter, and the empty
        // string.
        for name in [
            "ClsOperand",
            "CallTarget",
            "ClassDecl",
            "IfStmt",
            "InlineHtmlStmt",
            "DollarBraceInterpolation",
            "A",
            "ABC",
            "a",
            "aB",
            "Mixed1Digit2",
            "already_snake",
            "",
        ] {
            assert_eq!(snake_case(name), open_coded(name), "input {name:?}");
        }
    }

    #[test]
    fn keyword_variant_conversion() {
        assert_eq!(keyword_variant("function"), "FUNCTION_KW");
        assert_eq!(keyword_variant("include_once"), "INCLUDE_ONCE_KW");
        // Leading underscores are stripped from the variant name only.
        assert_eq!(keyword_variant("__halt_compiler"), "HALT_COMPILER_KW");
    }

    #[test]
    fn token_class_cross_check_used_but_not_declared() {
        let g = parse("Foo = <mystery>\n").expect("parses");
        let errors = check_token_classes(&g, &["ident"]);
        // <mystery> used but not declared (line-numbered), and `ident`
        // declared but unused.
        assert_eq!(errors.len(), 2);
        let used = errors
            .iter()
            .find(|e| e.message.contains("mystery"))
            .expect("error for <mystery>");
        assert_eq!(used.line, Some(1));
        assert!(used.message.contains("not declared"), "{}", used.message);
    }

    #[test]
    fn token_class_cross_check_declared_but_unused() {
        let g = parse("Foo = <ident>\n").expect("parses");
        let errors = check_token_classes(&g, &["ident", "stale_class"]);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("stale_class"),
            "{}",
            errors[0].message
        );
        assert!(errors[0].line.is_none());
    }

    #[test]
    fn trivia_extras_contiguity() {
        // The real list must satisfy the invariant `collect_kinds` enforces.
        assert!(trivia_extras_contiguous(EXTRAS));
        // Edge cases: empty, all-trivia, and a non-prefix (but contiguous)
        // run are all fine — only a gap between trivia entries is fatal.
        assert!(trivia_extras_contiguous(&[]));
        assert!(trivia_extras_contiguous(&[
            ("A", "", true),
            ("B", "", true)
        ]));
        assert!(trivia_extras_contiguous(&[
            ("A", "", false),
            ("B", "", true),
            ("C", "", true),
            ("D", "", false),
        ]));
        // The mistake the check exists for: a maintainer appends a new
        // trivia-flagged extra after EOF. `is_trivia`'s first..=last range
        // would then swallow OPEN_TAG/CLOSE_TAG/ERROR/EOF.
        assert!(!trivia_extras_contiguous(&[
            ("WHITESPACE", "", true),
            ("EOF", "", false),
            ("NEW_TRIVIA", "", true),
        ]));
    }

    #[test]
    fn unknown_symbol_literal_fails_with_line() {
        let g = parse("Foo = <ident>\nBar = '<~>'\n").expect("parses");
        let errors = generate(&g).expect_err("unknown punct must fail");
        let punct_error = errors
            .iter()
            .find(|e| e.message.contains("punctuation name table"))
            .expect("punct table error");
        assert_eq!(punct_error.line, Some(2));
    }
}
