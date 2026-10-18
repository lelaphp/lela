---
status: accepted
date: 2026-09-05
supersedes: third-engine-design.r2.md §6.1 ("the lossless syntax tree remains a month-two slimming candidate"); departs from Mahout's ADR-0001 representation
---

# The syntax tree is typed nodes in a safe arena over a lossless token stream, not a green/red tree

Mahout's hand-rolled green/red tree (its ADR-0001) buys position-independent nodes and structural sharing. Both paid only for incremental reparse and salsa backdating, and the third engine has neither. What the representation costs a compile-once engine is an `Arc` allocation per navigation step, an offset recomputed by summing preceding siblings, a linear rescan of children per generated accessor, and a heap copy of every token's bytes beside the retained source; Mahout's one speed-sensitive consumer bypassed the typed layer and walked green nodes by hand. The third engine generates one struct per `php.ungram` node kind with named child fields, resolves the fields once from the parser's event stream, stores nodes in an arena the caller lends the parse and addressed by index, keeps no parent links, keeps trivia in the lossless token stream with doc comments attached by the builder as a token index, and deletes node interning. Zero `unsafe` is kept; the constraint of Mahout's ADR-0001 survives, its representation does not.

*Amended 2026-10-18 by #2886: the sentence read "stores nodes in an arena owned by the parse"; since #2810 the arena is the caller's, one per request and reset after each compile as php's CG(ast_arena) is given up, and the parse and its tree borrow it. The decision is untouched: typed nodes resolved once, addressed by index, no parent links, zero `unsafe` in php_syntax. bumpalo's own `unsafe` is a dependency's, as std's is; php_syntax keeps `#![forbid(unsafe_code)]`.*

## Considered options

- **Keep the green/red tree and slim it later.** Revision 2's position. Rejected: the tree is the compiler's input on day 3, and a representation change after the compiler exists is a rewrite of the compiler's walk.
- **A homogeneous arena of `(kind, range, children)` with scanning accessor views.** Allocation-free but every accessor remains a scan. Rejected in favour of resolving fields once, which is `zend_ast`'s positional-children shape and what a compiler after `zend_compile.c` expects.
- **Keep `NodeCache` behind a measurement.** Rejected: no engine consumer depends on interning semantics, so there is nothing to measure but a memory saving on repeated tokens; the parse-throughput bar (≥ 25 MB/s, Mahout's figure) is the regression check instead.

## Consequences

- Required markers in `php.ungram` become non-`Option` fields; Mahout's ADR-0004 discipline (`!` only where construction anchors on the child) and its required-children soak carry over unchanged.
- 253 of the 549 inherited snapshot tests survive with trivia lines removed, the 296 of `fixtures_validation.rs` going to `php_compile` with the validator; the dump renders every fixture, so it lands with the arena and each grammar commit regenerates the fixture files it brings, once each. The dump format is kept. *Amended 2026-09-13 by #166 (design revision 12): the bullet read "the 549 inherited snapshot tests survive with trivia lines removed in one regeneration"; `data/syntax-extraction.md` §3.9 measured 549 as the `#[test]` count of `mahout_parser/tests/*.rs` at the pin, of which 296 follow the validator to `php_compile`, and §6.11 found the regeneration is per fixture file. The decision is untouched; a figure in Consequences follows the measurement.*
- Upward navigation does not exist. A construct that needs its parent is a compiler-scope gap and is fixed there.
- `token_get_all`, `highlight_string` and `__COMPILER_HALT_OFFSET__` read the token stream, which remains byte-lossless.
