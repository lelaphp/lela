use super::*;
use crate::lexer::{LexOptions, Token, lex};
use crate::line_index::LineIndex;
use crate::tree;

fn tokens(kinds: &[(SyntaxKind, u32)]) -> Vec<Token> {
    let mut offset = 0;
    kinds
        .iter()
        .map(|&(kind, len)| {
            let token = Token { kind, offset, len };
            offset += len;
            token
        })
        .collect()
}

#[test]
fn marker_precede_chains() {
    // The Pratt core: INT as LITERAL_EXPR, then wrapped into BINARY_EXPR.
    let text = b"1+2";
    let raw = tokens(&[
        (SyntaxKind::INT, 1),
        (SyntaxKind::PLUS, 1),
        (SyntaxKind::INT, 1),
    ]);
    let input = Input::new(text, &raw);
    let lines = LineIndex::new(text);
    let mut p = Parser::new(&input, FileId(0), &lines);

    let m = p.start();
    p.bump(SyntaxKind::INT);
    let lhs = m.complete(&mut p, SyntaxKind::LITERAL_EXPR);
    let wrapper = lhs.precede(&mut p);
    p.bump(SyntaxKind::PLUS);
    let rhs = p.start();
    p.bump(SyntaxKind::INT);
    rhs.complete(&mut p, SyntaxKind::LITERAL_EXPR);
    wrapper.complete(&mut p, SyntaxKind::BINARY_EXPR);

    let events = p.finish();
    // The first event carries the forward link to its wrapper.
    assert!(
        matches!(
            &events[0],
            Event::Start {
                forward_parent: Some(3),
                ..
            }
        ),
        "expected Start with forward_parent 3, got {:?}",
        events[0]
    );
    let built = tree::build(&raw, &LineIndex::new(text), events);
    assert_eq!(
        tree::dump(&built.tree, &raw, text),
        "BINARY_EXPR@0..3\n  LITERAL_EXPR@0..1\n    INT@0..1 \"1\"\n  PLUS@1..2 \"+\"\n  LITERAL_EXPR@2..3\n    INT@2..3 \"2\"\n"
    );
}

#[test]
fn at_reads_the_current_token_and_a_bump_moves_on() {
    let text = b"$x;";
    let raw = tokens(&[(SyntaxKind::VARIABLE, 2), (SyntaxKind::SEMICOLON, 1)]);
    let input = Input::new(text, &raw);
    let lines = LineIndex::new(text);
    let mut p = Parser::new(&input, FileId(0), &lines);
    assert!(!p.at(SyntaxKind::COMMA));
    assert!(!p.at_any(&[SyntaxKind::R_PAREN, SyntaxKind::COMMA]));
    assert!(!p.at(SyntaxKind::IDENT));
    assert!(p.at(SyntaxKind::VARIABLE));
    p.bump(SyntaxKind::VARIABLE);
    assert!(!p.at(SyntaxKind::VARIABLE));
    assert!(p.eat(SyntaxKind::SEMICOLON));
    assert!(p.at_eof());
    assert_eq!(p.current(), SyntaxKind::EOF);
}

#[test]
fn unexpected_here_spells_php() {
    let text = b"<?php $x = (1];";
    let lexed = lex(FileId(0), text, &LexOptions::default());
    let input = Input::new(text, &lexed.tokens);
    let lines = LineIndex::new(text);
    let mut p = Parser::new(&input, FileId(0), &lines);
    p.bump(SyntaxKind::OPEN_TAG);
    p.bump(SyntaxKind::VARIABLE);
    assert_eq!(
        p.unexpected_here().message,
        "syntax error, unexpected token \"=\""
    );
    assert_eq!(
        p.unexpected_expecting("\";\"").message,
        "syntax error, unexpected token \"=\", expecting \";\""
    );
    p.bump(SyntaxKind::EQ);
    p.bump(SyntaxKind::L_PAREN);
    p.bump(SyntaxKind::INT);
    assert_eq!(p.current(), SyntaxKind::R_BRACK);
    assert_eq!(
        p.unexpected_here().message,
        "Unclosed '(' does not match ']'"
    );
    assert_eq!(
        p.unexpected_here_expecting("\")\"").message,
        "Unclosed '(' does not match ']'"
    );
    p.bump(SyntaxKind::R_BRACK);
    p.bump(SyntaxKind::SEMICOLON);
    assert!(p.at_eof());
    assert_eq!(p.unexpected_here().message, "Unclosed '('");
    assert_eq!(p.unexpected_here_expecting("\";\"").message, "Unclosed '('");
    // Nothing open at the end: the clause is spelled onto end of file.
    let text = b"<?php $x = 1";
    let lexed = lex(FileId(0), text, &LexOptions::default());
    let input = Input::new(text, &lexed.tokens);
    let lines = LineIndex::new(text);
    let mut p = Parser::new(&input, FileId(0), &lines);
    while !p.at_eof() {
        p.bump_any();
    }
    assert_eq!(
        p.unexpected_here().message,
        "syntax error, unexpected end of file"
    );
    assert_eq!(
        p.unexpected_here_expecting("\";\"").message,
        "syntax error, unexpected end of file, expecting \";\""
    );
    // A closer with nothing open, and an opener on an earlier line.
    let text = b"<?php $x = 1);\n\n$y = (\n1];";
    let lexed = lex(FileId(0), text, &LexOptions::default());
    let input = Input::new(text, &lexed.tokens);
    let lines = LineIndex::new(text);
    let mut p = Parser::new(&input, FileId(0), &lines);
    while !p.at(SyntaxKind::R_PAREN) {
        p.bump_any();
    }
    assert_eq!(p.unexpected_here().message, "Unmatched ')'");
    while !p.at(SyntaxKind::R_BRACK) {
        p.bump_any();
    }
    assert_eq!(
        p.unexpected_here().message,
        "Unclosed '(' on line 3 does not match ']'"
    );
    assert!(p.eat(SyntaxKind::R_BRACK));
    assert!(p.eat(SyntaxKind::SEMICOLON));
    assert_eq!(p.unexpected_here().message, "Unclosed '(' on line 3");
}

/// The wordings measured under the pin at 8.5.9 (the docs of `php_token_name`).
#[test]
fn php_token_name_as_zend_yytnamerr() {
    let name = |kind, text: &str| php_token_name(kind, text.as_bytes());
    assert_eq!(name(SyntaxKind::DIE_KW, "DIE"), "token \"exit\"");
    assert_eq!(name(SyntaxKind::FOREACH_KW, "FOREACH"), "token \"foreach\"");
    assert_eq!(name(SyntaxKind::CAST, "(   float  )"), "token \"(float)\"");
    assert_eq!(name(SyntaxKind::CAST, "(INTEGER)"), "token \"(int)\"");
    assert_eq!(name(SyntaxKind::CAST, "(binary)"), "token \"(string)\"");
    assert_eq!(name(SyntaxKind::LT_GT, "<>"), "token \"!=\"");
    assert_eq!(
        name(SyntaxKind::MAGIC_CONST, "__class__"),
        "token \"__CLASS__\""
    );
    assert_eq!(name(SyntaxKind::BACKSLASH, "\\"), "token \"\\\"");
    assert_eq!(name(SyntaxKind::ATTRIBUTE_OPEN, "#["), "token \"#[\"");
    assert_eq!(name(SyntaxKind::CLOSE_TAG, "?>\n"), "token \";\"");
    assert_eq!(
        name(SyntaxKind::HALT_COMPILER_KW, "__halt_compiler"),
        "token \"__halt_compiler\""
    );
    assert_eq!(name(SyntaxKind::DOUBLE_QUOTE, "\""), "double-quote mark");
    assert_eq!(name(SyntaxKind::ERROR, "\0"), "character 0x00");
    assert_eq!(name(SyntaxKind::IDENT, "Ünïcödé"), "identifier \"Ünïcödé\"");
    assert_eq!(name(SyntaxKind::INT, "0x1"), "integer \"0x1\"");
    assert_eq!(
        name(SyntaxKind::FLOAT, ".5"),
        "floating-point number \".5\""
    );
    assert_eq!(
        name(SyntaxKind::NAME_QUALIFIED, "A\\B"),
        "namespaced name \"A\\B\""
    );
    assert_eq!(
        name(SyntaxKind::NAME_FULLY_QUALIFIED, "\\A\\B"),
        "fully qualified name \"\\A\\B\""
    );
    assert_eq!(
        name(SyntaxKind::NAME_RELATIVE, "namespace\\A"),
        "namespace-relative name \"namespace\\A\""
    );
    assert_eq!(
        name(SyntaxKind::SINGLE_QUOTED, "'abc'"),
        "single-quoted string \"abc\""
    );
    assert_eq!(
        name(SyntaxKind::SINGLE_QUOTED, "''"),
        "single-quoted string \"\""
    );
    assert_eq!(
        name(SyntaxKind::SINGLE_QUOTED, "'\"'"),
        "single-quoted string \"\"\""
    );
    assert_eq!(
        name(SyntaxKind::SINGLE_QUOTED, "'a\nb'"),
        "single-quoted string \"a\""
    );
    assert_eq!(
        name(SyntaxKind::SINGLE_QUOTED, "B'abc'"),
        "quoted string \"B'abc\""
    );
    assert_eq!(name(SyntaxKind::STRING_TEXT, "ab"), "string content \"ab\"");
    assert_eq!(
        name(SyntaxKind::HEREDOC_START, "<<<EOT\n"),
        "heredoc start \"<<<EOT\""
    );
    assert_eq!(
        name(SyntaxKind::HEREDOC_START, "<<<'EOT'\n"),
        "heredoc start \"<<<'EOT\""
    );
    assert_eq!(
        name(SyntaxKind::HEREDOC_START, "<<<  EOT\n"),
        "heredoc start \"<<<  EOT\""
    );
    assert_eq!(
        name(
            SyntaxKind::SINGLE_QUOTED,
            "'abcdefghijklmnopqrstuvwxyz012345'"
        ),
        "single-quoted string \"abcdefghijklmnopqrstuvwxyz012345\""
    );
    assert_eq!(
        name(SyntaxKind::VARIABLE, "$abcdefghijklmnopqrstuvwxyz0123456"),
        "variable \"$abcdefghijklmnopqrstuvwxyz012...\""
    );
}

fn parenthesised(depth: usize) -> Vec<u8> {
    let mut src = b"<?php $x = ".to_vec();
    src.extend(std::iter::repeat_n(b'(', depth));
    src.push(b'1');
    src.extend(std::iter::repeat_n(b')', depth));
    src.extend_from_slice(b";\n");
    src
}

/// The source through the grammar's parenthesised path, one level of
/// recursion per `(`.
fn parse_nested(src: &[u8]) -> tree::Built {
    let lexed = lex(FileId(0), src, &LexOptions::default());
    assert!(lexed.diagnostics.is_empty(), "the lexer has no depth");
    let input = Input::new(src, &lexed.tokens);
    let lines = LineIndex::new(src);
    let mut p = Parser::new(&input, FileId(0), &lines);
    crate::grammar::source_file(&mut p);
    tree::build(&lexed.tokens, &lines, p.finish())
}

/// The nesting cap: past [`MAX_DEPTH`] the construct is consumed flat, no
/// stack overflows at any depth, the tree still holds every token, and
/// exactly one diagnostic is raised, php's `memory exhausted`. Parsed on
/// a thread carrying [`MIN_THREAD_STACK`], as every thread that parses
/// does.
#[test]
fn nesting_depth_cap() {
    let handle = std::thread::Builder::new()
        .stack_size(MIN_THREAD_STACK)
        .spawn(|| {
            for depth in [600_usize, 10_000] {
                let src = parenthesised(depth);
                let built = parse_nested(&src);
                let messages: Vec<&str> = built
                    .diagnostics
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect();
                assert_eq!(
                    messages,
                    ["memory exhausted"],
                    "one refusal at depth {depth}"
                );
                let tokens = built
                    .tree
                    .elements()
                    .iter()
                    .filter(|e| matches!(e, tree::Element::Token { .. }))
                    .count();
                // `<?php`, `$x`, `=`, the parens, `1`, `;`.
                assert_eq!(tokens, 2 * depth + 5, "every token once at depth {depth}");
            }
            // Under the cap: silent.
            let built = parse_nested(&parenthesised(100));
            assert!(built.diagnostics.is_empty(), "depth 100 parses clean");
        });
    let joined = handle.map(std::thread::JoinHandle::join);
    assert!(
        matches!(joined, Ok(Ok(()))),
        "the parse thread carrying MIN_THREAD_STACK ran to its end"
    );
}
