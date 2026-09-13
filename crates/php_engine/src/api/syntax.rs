//! The syntax group (design §4.4, §4.9): re-exports of the token stream,
//! the interface `token_get_all`, `highlight_string` and
//! `__COMPILER_HALT_OFFSET__` consume, so an extension reads the lexer's
//! kinds and php's names for them from the one lexer there is. What
//! `ext/tokenizer` decides on its own, the `(set)` fusion, `yield from`,
//! `T_BAD_CHARACTER` and the undedented heredoc, stays with it.

pub use php_compile::syntax::{
    FileId, LexOptions, Lexed, SyntaxKind, Token, amp_name, cast_name, int_overflows, lex,
    magic_const_name, operator_name,
};
