//! The syntax group (design §4.4, §4.9): re-exports of the token stream
//! and the tree, the interface `token_get_all`, `highlight_string` and
//! `__COMPILER_HALT_OFFSET__` consume, so an extension reads the lexer's
//! kinds and php's names for them from the one lexer there is, and
//! `token_get_all` under `TOKEN_PARSE` reads the one parser's shape and
//! diagnostics where php's `tokenize_parse` reads the parser's events.
//! What `ext/tokenizer` decides on its own, the `(set)` fusion, `yield
//! from`, `T_BAD_CHARACTER`, the undedented heredoc and which of the
//! tree's names php's grammar takes as an identifier, stays with it.

pub use php_compile::syntax::{
    Diagnostic, Element, FileId, Level, LexOptions, Lexed, Options, Parsed, SyntaxKind, Token,
    TokenId, Tree, amp_name, cast_name, int_overflows, lex, magic_const_name, operator_name, parse,
};
