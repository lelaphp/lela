# php_syntax extraction report — the inherited syntax crates at Mahout `1b3104a8`

What the four Mahout syntax crates and their generator hold, measured file by file, so the copy into
`crates/php_syntax` can be cut into tickets. Every number below was counted in the file it names.
Design references are `third-engine-design.md` §4.9 (`php_syntax`: the tree), §6.1 (the compiler and
its oracle), the `php_syntax` row of the §4.1 crate table and the first row of the §13 inheritance
manifest; the representation decision is ADR-0003; the ceiling is `seams.toml` `[[crate]] php_syntax
budget = 26000`.

## 0. What was read, and where the pin lives

§13 pins Mahout at `1b3104a8` (walked at `d6460ef0`). That commit is **not in `~/Projects/mahout`**:
that clone carries 139 commits ending at `934e65074aac47d5a31f9be1e8b97de92a72f83b`, 2026-07-02, and
none of `1b3104a8`, `d6460ef0` or `8f119938` is an object in it. The pinned commit is in
`~/Projects/mahout_base` and `~/Projects/mahout_php` (3,831 commits each), where it resolves to:

| ref in §13 | full sha | date | present in |
|---|---|---|---|
| `1b3104a8` | `1b3104a85e1d4f6ab4bb7c7c1fdf350d70b0ae7b` | 2026-09-05 | `mahout_base`, `mahout_php` |
| `d6460ef0` | `d6460ef09981d66d84d65ac2dfeb6a078cb830d7` | 2026-09-05 | `mahout_base`, `mahout_php` |
| `~/Projects/mahout` HEAD | `934e65074aac47d5a31f9be1e8b97de92a72f83b` | 2026-07-02 | `mahout` only |

The two lineages differ where it matters for the slicing. At `1b3104a8` the parser's validate is
twelve files under `src/validate/`; in `~/Projects/mahout` it is one 1,824-line `validate.rs`. At
`1b3104a8` `mahout_source` holds `magic_method.rs` and `builtin_attr.rs` and `mahout_lexer` holds
`php_names.rs`, the three files §4.9 and §13 name; **none of the three exists in `~/Projects/mahout`**.
Every number in this record is read at `1b3104a8`, which is what §13 pins and what the design's own
figures reproduce (see §6 below); where `~/Projects/mahout` HEAD differs materially the row says so.

**How lines are counted.** `xtask budget` counts, per crate, the git-tracked `.rs` files under the
crate path, dropping any path with a `tests` or `tests.rs` component and the top-level `tests`,
`benches`, `examples` and `fixtures` directories (`xtask/src/sources.rs:18-39`), and counts each
through `rust::code_lines(text, true)`: non-blank, non-comment, with `#[cfg(test)]` items skipped
(`xtask/src/lines.rs:11-20`, `xtask/src/rust.rs:92-112`). Every "ceiling" number below is that
function's count, obtained by running that exact function — `xtask/src/rust.rs` copied verbatim into a
scratch binary outside the repository — over each Mahout file. Raw `wc -l` is given where the design
quotes a raw figure. Files under a `tests` component are listed apart: they cost the ceiling nothing.

## 1. The crates behind `php_syntax::parse`

### 1.1 Totals

| crate | files | raw `wc -l` | ceiling-counted src | test files (off-ceiling) |
|---|---|---|---|---|
| `mahout_lexer` | 7 src, 9 under `src/tests/` | 4,298 | 1,616 | 1,771 |
| `mahout_parser` | 24 src, 9 under `tests/` | 34,122 | 11,739 | 16,557 |
| `mahout_syntax` | 9 src, 1 under `tests/` | 10,631 | 8,642 | 156 |
| `mahout_source` | 8 src | 2,505 | 885 | 0 |
| **four crates** | | **51,556** | **22,882** | **18,484** |
| generator (`xtask/src/{ungram,codegen_ast,codegen}.rs`) | 3 | 3,175 | 2,296 | 97 (`xtask/tests/real_grammar.rs`, 123 raw) |

None of the four crates depends on anything outside the workspace. `mahout_lexer` depends on
`mahout_source` and `mahout_syntax`; `mahout_syntax` on `mahout_source`; `mahout_source` on nothing
(`crates/*/Cargo.toml`). `mahout_parser` lists four: `mahout_source` and `mahout_syntax`, plus
`mahout_code_meta` and `mahout_declty`, with `expect-test` and `mahout_lexer` as dev-dependencies
only. The last two are read at 31 sites, every one of them inside `validate/`
(`validate/types.rs` 28, `validate/extras.rs` 3), which this copy drops: neither crate travels, and
what lands behind `parse` is the four crates of this record. The copy adds no third-party dependency
but `expect-test`.

### 1.2 `mahout_lexer` — the mode machine

| file | raw | ceiling | disposition |
|---|---|---|---|
| `src/lexer.rs` | 434 | 313 | keep (the mode stack, `emit`, the halt tracker) |
| `src/scripting.rs` | 482 | 388 | keep |
| `src/heredoc.rs` | 367 | 267 | keep |
| `src/strings.rs` | 365 | 266 | keep |
| `src/numbers.rs` | 222 | 161 | keep |
| `src/php_names.rs` | 185 | 135 | keep (the `T_*` tables, §3.8 below) |
| `src/lib.rs` | 184 | 86 | keep, less the `Checkpoint` surface |
| `src/tests/*` (9 files, 119 `#[test]`) | 2,059 | 1,771 | keep, less `checkpoints.rs` |

### 1.3 `mahout_parser` — the driver, the grammar, validate

| file | raw | ceiling | disposition |
|---|---|---|---|
| `src/grammar/mod.rs` | 1,386 | 1,110 | keep |
| `src/grammar/expr.rs` | 1,321 | 991 | keep |
| `src/grammar/decl.rs` | 828 | 666 | keep |
| `src/grammar/types.rs` | 142 | 106 | keep |
| `src/grammar/strings.rs` | 140 | 106 | keep |
| `src/grammar/ckw.rs` | 146 | 35 | keep |
| `src/parser.rs` | 912 | 512 | keep (the 37-method driver) |
| `src/input.rs` | 204 | 108 | keep (the trivia-hiding cursor) |
| `src/lib.rs` | 107 | 65 | keep, re-shaped as `parse` |
| `src/event.rs` | 36 | 15 | keep |
| `src/tree.rs` | 363 | 166 | rewrite (events → arena, ADR-0003) |
| `src/reparse.rs` | 251 | 133 | **drop** |
| `src/validate/` (12 files) | 10,284 | 7,726 | **drop** (§6.1: re-homed as compiler guards) |
| `tests/fixtures_validation.rs` (296 `#[test]`) | 10,228 | 9,516 | **drop from this crate** (→ `php_compile` fixtures) |
| `tests/*` other (8 files, 253 `#[test]`) | 7,774 | 7,041 | keep |

The grammar is 116 functions over six files, 3,014 ceiling lines. `validate/` at the pin:
`expr.rs` 1,321, `extras.rs` 1,304, `members_decls.rs` 959, `stmts.rs` 865, `structure.rs` 679,
`types.rs` 573, `params.rs` 426, `members_hooks.rs` 386, `class_likes.rs` 355, `mod.rs` 296,
`removed.rs` 267, `attributes.rs` 295 — ceiling lines, summing to 7,726.

### 1.4 `mahout_syntax` — the tree, almost all of it generated

| file | raw | ceiling | disposition |
|---|---|---|---|
| `src/generated/ast.rs` | 8,039 | 7,172 | regenerate: 6,255 of node views, 917 of the visitor block |
| `src/generated/syntax_kind.rs` | 777 | 402 | regenerate (306 variants, 4 predicates) |
| `src/generated/display.rs` | 186 | 177 | regenerate (167 arms) |
| `src/generated/mod.rs` | 10 | 4 | regenerate |
| `src/red.rs` | 648 | 424 | **drop** (the red cursors; `dump` at :287-324 is reimplemented) |
| `src/ast.rs` | 321 | 216 | split: 128 replace (`:1-194`, `AstCast`, `AstNode`, `AstChildren`, `support::*` scans), 88 **drop** (`:195-321`, the property-hook helpers, read only by `validate/`) |
| `src/builder.rs` | 266 | 119 | **drop** (`NodeCache`, `GreenNodeBuilder`, `Checkpoint`) |
| `src/green.rs` | 177 | 117 | **drop** (the green nodes) |
| `src/lib.rs` | 22 | 11 | replace |
| `tests/typed_ast.rs` (8 `#[test]`) | 185 | 156 | keep, rewritten against the arena |

`src/generated/` cannot be copied as it stands: `xtask budget` refuses any tracked file under a crate
tree with a `generated` path component or an `@generated` marker
(`xtask/src/budget.rs:352-373`), and AGENTS.md and the §4.1 crate table put the emission in
`build.rs`. The four generated files are written into `OUT_DIR` and cost the ceiling nothing.

### 1.5 `mahout_source` — spans, diagnostics and three tables

| file | raw | ceiling | disposition |
|---|---|---|---|
| `src/codes.rs` | 1,124 | 231 | **drop** (§6.1: "There is no code number on a diagnostic") |
| `src/diag.rs` | 380 | 176 | keep, rewritten without `CodeInfo` |
| `src/line_index.rs` | 316 | 153 | keep (the line table §4.1 names) |
| `src/lib.rs` | 214 | 113 | keep (`FileId`, `Span`, `TextRange`, `load_bytes`) |
| `src/magic_method.rs` | 192 | 82 | §13 routes it here; **no consumer in the lexer or the grammar** (§6.5) |
| `src/builtin_attr.rs` | 102 | 59 | same |
| `src/root.rs` | 111 | 44 | **drop** (`SourceRoot`, the salsa-era file map) |
| `src/level.rs` | 66 | 27 | **drop** (`LanguageLevel`, validate's only reader) |

### 1.6 What §4.9 drops, priced

| drop | where | ceiling lines | raw |
|---|---|---|---|
| validate | `mahout_parser/src/validate/` (12 files) | 7,726 | 10,284 |
| the green/red tree | `mahout_syntax/src/{green,red}.rs` | 541 | 825 |
| `NodeCache` and the green builder | `mahout_syntax/src/builder.rs` | 119 | 266 |
| reparse | `mahout_parser/src/reparse.rs` | 133 | 251 |
| `codes.rs` | `mahout_source/src/codes.rs` | 231 | 1,124 |
| `root.rs` | `mahout_source/src/root.rs` | 44 | 111 |
| `level.rs` | `mahout_source/src/level.rs` | 27 | 66 |
| lexer checkpoints | `mahout_lexer/src/lib.rs:84-101,146-184` (35) plus the recorder in `lexer.rs` (10: the `checkpoints` field, the `record_checkpoints` parameter, its init, the `run` return and the push at `:191-196`) | 45 | 67 |
| the property-hook helpers | `mahout_syntax/src/ast.rs:195-321` (`parent_hook_call`, `hook_references_this_prop`, `hook_references_parent_hook`; 11 call sites, every one in `validate/`) | 88 | 127 |
| **dropped against the ceiling** | | **8,954** | **13,121** |
| the unused generated lines | `generated/ast.rs:6976-8039` | 917 | 1,064 |
| the validation fixtures | `mahout_parser/tests/fixtures_validation.rs` | 9,516 | 10,228 |
| the checkpoint tests | `mahout_lexer/src/tests/checkpoints.rs` (11 `#[test]`) | 112 | 130 |

Parent links have no line of their own: upward navigation is the `parent: Option<(SyntaxNode, u32)>`
field of `red.rs:22-28` and the nine call sites inside `red.rs` that implement it. They go with the
red cursors.

## 2. The generator

### 2.1 What `php.ungram` is

`docs/specs/php.ungram`, 542 raw lines — 209 of rule text, 258 of comment, 75 blank — is the one
statement of the tree's shape. Its own header is the authority for the dialect (`xtask/src/ungram.rs:1-26`): it is not
vanilla ungrammar. `Node = Rule` is a struct node with its own `SyntaxKind`; `Node = A | B | C` is a
typed enum when any alternative is a node and a token-set alias when all are tokens; `trait Name = Rule`
is a mixin; `Node : TraitA = Rule` opts in; `'kw'` is a literal token, `<class>` one of 19 declared
token classes, `label:Rule` names the accessor, `R!` marks a required child, and `R,*`/`R,+` are
comma-separated lists whose comma is implied. The file carries 156 definitions — 153 node and enum
rules plus the 3 traits — and 30 `VALIDATE` markers naming what the tree admits and php does not. The
19 classes are the rows of `codegen_ast.rs`'s `CLASS_DISPLAY` and the set `xtask/tests/real_grammar.rs`
holds the schema to.

Convention A2 is the one ADR-0003 turns into a field: default is `Option`, `!` only where construction
anchors on the child. Convention A4 is the one that makes named fields possible at all — several
children of one type are resolved positionally against anchor tokens (`TernaryExpr::then` is the `Expr`
between `?` and `:`), and where optionality would make that ambiguous the schema introduces a wrapper
node (`ForInit`, `ForCond`, `ForStep`, `ForeachKey`, `ArrayKey`, `YieldKey`).

### 2.2 What it reads and emits today

`cargo xtask codegen` (`xtask/src/main.rs:324-330`) runs `run_codegen` (`:969-1013`), which parses the
schema once and drives three emitters from it, listed in the target table at `:998-1002`. The same
command also regenerates Mahout's MIR, kernel and tier-0 files from other inputs; those are not the
schema's and do not come along.

| emitter | source | into | raw | ceiling |
|---|---|---|---|---|
| `codegen::generate_unformatted` | `xtask/src/codegen.rs` (803 raw, 543) | `crates/mahout_syntax/src/generated/syntax_kind.rs` | 777 | 402 |
| `codegen_ast::generate_ast_unformatted` | `xtask/src/codegen_ast.rs` (1,262 raw, 1,102) | `generated/ast.rs` | 8,039 | 7,172 |
| `codegen_ast::generate_display_unformatted` | same file, `:1196-1262` | `generated/display.rs` | 186 | 177 |

`xtask/src/ungram.rs` (1,110 raw, 651) is the DSL parser under all three: `Rule`, `Def`, `Grammar`,
`classify` and `validate`. Output is passed through `codegen::rustfmt` before it is written.
**§13's "2,372 lines" is `codegen_ast.rs` 1,262 + `ungram.rs` 1,110, raw, exactly.** It omits
`codegen.rs`: without it there is no `SyntaxKind` emitter, no `PUNCT_TABLE`, no keyword table, no
token-class hold and no `rustfmt` pass. The generator is 3,175 raw lines, 2,296 ceiling lines, in three
files, not two.

The emitted `ast.rs` is 140 structs, 11 enums, 3 token-set aliases and 269 accessors. Each struct is a
one-field view over a cursor:

- `pub struct ClassDecl { syntax: SyntaxNode }`, plus an `impl AstCast` (`can_cast`, `cast`,
  `text_range`) and an `impl AstNode` (`cast_node`, `syntax`) — about 20 lines per node kind;
- each accessor a call into `ast.rs`'s hand-written `support` module, which **scans**
  `children_with_tokens()`: `support::child` is `find_map(T::cast)`, `support::child_after` walks the
  children arming on an anchor token (`crates/mahout_syntax/src/ast.rs:83-113`, in the `support`
  module at `:76-194`);
- a required child is `support::child(&self.syntax).expect("required child …")` — 45 `.expect(` calls
  in the emitted file.

Below the accessors sit 1,064 raw / 917 ceiling lines that no engine consumer reads
(`generated/ast.rs:6976-8039`): the `Visitor` trait (422), the `visit` dispatch (264) and
`check_required_children` (231). At the pin the only reader of `Visitor` or `visit` anywhere in Mahout is
`crates/mahout_syntax/tests/typed_ast.rs`; `check_required_children` is read by three test files and
nothing else. **This block is the design's "~1,060 unused generated lines": 1,064, measured.**
ADR-0003 keeps the required-children soak, so 231 of the 1,064 survive and 833 raw / 686 ceiling go.

### 2.3 What the rewrite to typed nodes changes

ADR-0003 replaces the representation, not the schema, and §4.9 states the target: one struct per node
kind with named child fields, resolved once from the parser's event stream, in an arena owned by
`Parsed` and addressed by index, no parent links, no interning, `#![forbid(unsafe_code)]`.

**Survives, unchanged:** `ungram.rs` whole — the DSL parser, `classify`, `validate` and the required-`!`
rule (651 ceiling lines). `codegen.rs` whole — the `SyntaxKind` enum, `PUNCT_TABLE`, `EXTRAS`,
`keyword_variant`, `check_token_classes`, `rustfmt` (543). In `codegen_ast.rs`, the slot model
(`:64-592`): `RAW_LABELS`, `SlotTy`, `Slot`, `FlatItem`, `FlattenCtx`, `SlotMods`, `KindSet`, `Model`,
`resolve_token`, `flatten`, `flatten_alt`, `slots_for` and the anchor/stop computation
that convention A4 needs — this is the part that decides *which* field each child becomes, and it is
the same question whether the answer is a scan or an index. `generate_display_unformatted` (`:1196-1262`)
and its two hand-written tables, `CLASS_DISPLAY` (`:28-50`, 19 rows) and `EXTRA_DISPLAY` (`:51-63`, 9).

**Replaced:** `accessor_body` (`:608-719`), which emits the `support::child`/`child_after`/`children`
scan calls — each becomes a read of a field resolved at build time. `emit_cast_impls` (`:736-760`) and
the per-node emission in `generate_ast_unformatted` (`:761-1018`), which emit the `syntax: SyntaxNode`
field and the two cursor traits — each node becomes a struct of node/token indices into the arena. The
45 emitted `.expect(` calls go with them: a required child is a plain non-`Option` field, which AGENTS.md
("`unwrap` and `expect` are denied in engine crates") requires anyway. `emit_enum` (`:1036-1195`) keeps
its shape and changes its payload from a cursor to an index. The `Visitor` and `visit` emission goes.

**Replaced outside the generator:** `mahout_syntax/src/ast.rs:1-194` is the scan the
rewrite exists to delete (128 ceiling lines; the file's other 88 go with validate, §1.6);
`mahout_parser/src/tree.rs` (166) is where the resolution
moves, since the builder is the only place that sees the event stream and can fill the fields in one
pass.

## 3. The survivors §4.9 names, confirmed one by one

### 3.1 The lexer's mode machine — confirmed

`crates/mahout_lexer/src/lib.rs:103-144`: a `Mode` enum of ten variants, each carrying Zend's state name
in its doc comment — `InlineHtml` (`INITIAL`), `Scripting` (`ST_IN_SCRIPTING`), `DoubleQuoted`,
`Heredoc { label }`, `Nowdoc { label }`, `Backquote`, `LookingForProperty`, `LookingForVarName`,
`VarOffset`, and `HaltCompiler`, which the file records as having no Zend analogue because php-src
halts in the parser (`zend_do_halt_compiler`). Brace depth is repeated `Scripting` entries, exactly
Zend's `yy_push_state`. 1,616 ceiling lines over seven files.

### 3.2 Heredoc dedent into trivia — confirmed

`crates/mahout_lexer/src/heredoc.rs`, 267 ceiling lines. The closing marker's indentation is emitted as
a `HEREDOC_INDENT` token (`:118`, `:137`), so the stripped bytes stay in the stream and the token texts
still concatenate to the input. `HeredocInfo` records an indentation count and a spaces/tabs flag, not
the byte string, because Zend records that shape (`:13-25`), which is why a body line indented with the
other character class is the "cannot mix" error at equal width.

### 3.3 Structured interpolation runs — confirmed

`crates/mahout_lexer/src/strings.rs:1-8`, 266 ceiling lines: interpolation does not re-lex. `$var`,
`$var[dim]` and `$var->prop` come out as structured sub-token runs, `{$` pushes a full `Scripting`
mode, `${` goes through `LookingForVarName`. The `Trigger` enum names Zend's three tokens
(`T_DOLLAR_OPEN_CURLY_BRACES`, `T_CURLY_OPEN`).

### 3.4 `__halt_compiler` handling — confirmed

A `HaltStage` machine arms on the label in `Scripting` and walks `(`, `)`, `;`; `Mode::HaltCompiler`
goes on the stack at `lexer.rs:295`, on the `;` and not on the label, and everything after
`__halt_compiler();` is emitted as one `HALT_DATA` token. 21 non-test lines name the construct, in
four files: `mahout_lexer/src/{lib,lexer}.rs` and `mahout_parser/src/{grammar/mod.rs,validate/structure.rs}`.
7 `#[test]` in `src/tests/halt.rs`.

### 3.5 The event-driven parser and its driver — 37 methods, not ≈ 30

`crates/mahout_parser/src/parser.rs:218-637`, one `impl Parser` block with **37 methods**: `new`, `file`,
`current`, `nth`, `current_text`, `nth_text`, `current_span`, `nth_span`, `prev_span`, `at`, `at_any`,
`peek_at`, `newline_before_current`, `remaining`, `at_eof`, `start`, `bump`, `bump_remap`, `bump_any`,
`do_bump`, `open_delim_span`, `eat`, `error`, `has_error_at_or_after`, `expected_set`, `unexpected_here`,
`error_unexpected`, `unexpected_expecting`, `unexpected_here_expecting`, `line_of`,
`bracket_closer_message`, `unclosed_eof_message`, `is_stuck`, `enter_depth`, `exit_depth`, `spend_fuel`,
`finish`. `Marker` adds `complete`/`abandon`/`precede`. The same block in `~/Projects/mahout` HEAD has
30, which is where "≈ 30" reproduces; at the pin it is 37. The output is `Vec<Event>`
(`src/event.rs`, 15 lines), consumed by `tree.rs`.

### 3.6 The grammar — confirmed

Six files, 116 top-level functions, 3,014 ceiling lines: `grammar/mod.rs` (statements, 41 fns),
`expr.rs` (the Pratt core, 29), `decl.rs` (26), `types.rs` (7), `ckw.rs` (contextual keywords, 7),
`strings.rs` (6). Only `ckw.rs` carries a `#[cfg(test)]` module, and its four test functions are not
in the 116.

### 3.7 The hand-pinned `expecting` clauses — the number 80 does not reproduce

The word `expecting` occurs 132 times across the four crates at the pin. The clauses themselves are
`Parser::unexpected_expecting` (`parser.rs:481`) and `Parser::unexpected_here_expecting` (`:503`), which
take php's position-specific `, expecting …` text as a literal. Counted in the grammar:

| kind | sites |
|---|---|
| `expect_closer(…)` (the closer's php wording, `grammar/mod.rs:338`) | 33 |
| `unexpected_here_expecting("…")` | 21 |
| `unexpected_expecting("…")` | 7 |
| `unexpected_here()` (no explicit clause) | 13 |
| **hand-placed error sites in `grammar/`** | **74** |

A second count that lands near 80: 79 `probed 8.5.x` oracle citations in `mahout_parser/src`, 87 across
the four crates, each marking one message pinned against a live php. Neither is 80. The claim's
direction holds — the wording is hand-pinned at the site, site by site — and the numbers to carry
forward are **61 sites with an explicit `expecting` clause, 74 hand-placed error sites, 79 oracle
probes**. In `~/Projects/mahout` HEAD the two `*_expecting` methods do not exist at all and the
diagnostics are Mahout-worded (`MHT2001@16..17 unexpected \`[\``), which is another reason that clone is
the wrong one to slice from.

### 3.8 `php_token_name` — it is `php_names.rs`

`crates/mahout_lexer/src/php_names.rs`, 185 raw / 135 ceiling lines, absent from `~/Projects/mahout`.
Five functions — `operator_name`, `magic_const_name`, `cast_name`, `int_overflows`, `amp_name` — that
spell each `SyntaxKind` the way php-src's `token_get_all` does. The file states its own boundary:
`keyword_name` is deliberately not there, because the shipped `ext/tokenizer` mapper intercepts
`PRIVATE_KW`, `PROTECTED_KW`, `PUBLIC_KW` and `YIELD_KW` before the lookup and the differential
harness does not, so merging the two tables would produce rows unreachable on one side. Its two
consumers are `ext/tokenizer` and `xtask::diff`, which is where `token_get_all` and the token
differential land here too.

### 3.9 The dump format and the snapshot tests — the format holds, and the 549 is exact

`SyntaxNode::dump` (`crates/mahout_syntax/src/red.rs:287-324`, with `dump_into` at `:293`) writes two-space-indented
`KIND@start..end` for a node and `KIND@start..end "text"` for a token, which is §4.9's format verbatim.
The fixture harness (`crates/mahout_parser/tests/support/mod.rs`) renders `root.dump()`, then the
required-children violations, then the diagnostics one per line, and asserts byte-losslessness on every
fixture including every recovery path.

**549 is exactly the number of `#[test]` functions in `crates/mahout_parser/tests/*.rs` at `1b3104a8`**:

| file | `#[test]` | `expect![[…]]` | raw | ceiling |
|---|---|---|---|---|
| `fixtures_validation.rs` | 296 | 296 | 10,228 | 9,516 |
| `fixtures_expr.rs` | 78 | 78 | 2,035 | 1,875 |
| `fixtures_decl.rs` | 35 | 35 | 1,538 | 1,428 |
| `fixtures_stmt.rs` | 35 | 35 | 1,126 | 1,045 |
| `contextual_keywords.rs` | 31 | 31 | 895 | 781 |
| `fixtures_variable_productions.rs` | 29 | 29 | 818 | 756 |
| `fixtures_strings.rs` | 23 | 23 | 569 | 509 |
| `fixtures_recovery.rs` | 22 | 16 | 706 | 593 |
| `support/mod.rs` | 0 | 0 | 87 | 54 |
| **total** | **549** | **543** | **18,002** | **16,557** |

But 296 of the 549 are `fixtures_validation.rs`, and §6.1 re-homes that file as compile-output fixtures
in `php_compile`. **253 snapshot tests travel into `php_syntax`, not 549**, in eight files worth 7,041
ceiling lines, none of them against the crate's ceiling. The lexer brings 119 more `#[test]` in
`src/tests/`, of which 11 are the checkpoint tests that go with reparse: 108 travel.

### 3.10 The recursion cap — confirmed

`crates/mahout_parser/src/parser.rs:84`, `pub const MAX_DEPTH: u32 = 512`, entered through
`enter_depth`/`exit_depth` and reported once per file. Beside it `MIN_THREAD_STACK = 16 MiB`, the
measured debug-build worst case at that depth with 2× headroom, which every thread that parses must
carry. Beside both, the forward-progress fuel (`FUEL = 256`, `parser.rs:65`) and the per-bump burst cap.

## 4. The total against the 26,000 ceiling

`crates/php_syntax` measures 11 lines today (`cargo xtask budget`: "php_syntax: 11 lines under the
ceiling of 26000"). What the copy spends, in the ceiling's own counting:

| term | ceiling lines |
|---|---|
| lexer, less the `Checkpoint` surface (1,616 − 45) | 1,571 |
| parser driver, events, input, `lib.rs`, and the six grammar files | 3,714 |
| `mahout_source`: `lib.rs` 113 + `diag.rs` 176 + `line_index.rs` 153 | 442 |
| **copied nearly verbatim** | **5,727** |
| rewritten against the arena: `tree.rs` 166 + `syntax/ast.rs:1-194` 128 + `syntax/lib.rs` 11 | 305 |
| the generator as `build.rs`: `ungram.rs` 651 + `codegen_ast.rs` 1,102 + `codegen.rs` 543 | 2,296 |
| the crate's present stub | 11 |
| **total** | **8,339** |
| **headroom under 26,000** | **17,661** |

Not spent: 8,954 ceiling lines dropped (§1.6), plus 686 of generated output. The generator's emission —
402 `SyntaxKind` + 177 display + 4 + 6,255 node views + 231 required-children = 7,069 lines in today's
cursor shape — is written to `OUT_DIR` by `build.rs` and costs nothing, because `xtask budget` counts
tracked files only and refuses a committed `generated/` path outright. Off-ceiling the copy carries
8,856 lines of tests (1,659 lexer, 7,041 parser, 156 syntax) and hands 9,516 more to `php_compile`.

Two terms in that total are floors, not measurements. The 305 rewritten lines are what Mahout spends on
the equivalent code, not what the arena will spend: the arena, the node storage, the lossless token
stream as `(kind, offset, len)` over the retained source, `Parsed` and the diagnostic merge are new code
and were not measured here. The 6,255 lines of node views are today's per-node cost in cursor shape; an
arena node of resolved index fields is a different shape and its size is unknown until the generator is
rewritten. The headroom is large enough that neither changes the answer: the copy fits with well over
half the ceiling unspent.

## 5. The slices

Ten tickets, in dependency order. Each names what it moves, what it is measured by, and the line figure
from the tables above. "Held by" names the instrument whose reading the ticket produces; `budget` and
`clippy` apply to every slice and are named once here rather than ten times.

**What makes one slice one session.** A slice lands as one commit whose readings are all green, so it
is cut to two bounds: it moves at most ~1,200 ceiling lines of code, and it brings at most ~4,000
off-ceiling lines of tests green in that same commit. Where a slice writes new code rather than copying
(4, 5, 10), the ceiling figure is not the work — the work is the one design decision the slice settles,
named in its row, and the bound is that it settles exactly one. The two grammar files that break the
first bound together, `grammar/mod.rs` (1,110) beside `decl.rs` and `types.rs` (772), are two slices for
that reason and no other; every other cut follows a file boundary the inherited tree already has.

| # | slice | moves | ceiling lines | off-ceiling | depends on | held by, beyond `budget` and `clippy` |
|---|---|---|---|---|---|---|
| 1 | **The schema and the kind enum.** `php.ungram` into `crates/php_syntax/`, `ungram.rs` and `codegen.rs` into `build.rs` and a `codegen/` module beside it, emitting `SyntaxKind` into `OUT_DIR`. | `docs/specs/php.ungram` (542 raw), `xtask/src/{ungram,codegen}.rs` | 1,194 | 97 | — | the 33 unit tests in the two files (26 in `ungram.rs`, 7 in `codegen.rs`) and `xtask/tests/real_grammar.rs` re-homed as `tests/schema.rs`; `budget` proves the emission is not committed |
| 2 | **The lexer: modes, numbers, scripting.** The mode stack and the scripting scanner, less every `Checkpoint` line. | `mahout_lexer/src/{lib,lexer,numbers,scripting}.rs`, `mahout_source/src/{lib,line_index}.rs` | 1,169 | 826 | 1 | 65 `#[test]` in `src/tests/{mod,numbers,scripting,tags,diagnostics}.rs` |
| 3 | **The lexer: strings, heredoc, halt, php's token names.** | `mahout_lexer/src/{strings,heredoc,php_names}.rs` | 668 | 833 | 2 | 43 `#[test]` in `src/tests/{strings,heredoc,halt}.rs`; a witness on `token_get_all`'s names |
| 4 | **The typed-node generator.** `codegen_ast.rs` rewritten to emit arena structs with named index fields: the slot model (`:64-592`) and the display emitter (`:1196-1262`) survive; `accessor_body`, `emit_cast_impls` and the node emission are replaced; `Visitor` and `visit` are not emitted. Settles: what one emitted node looks like. | `xtask/src/codegen_ast.rs` | 1,102 | — | 1 | the emitted crate compiling; a test that the emitted file holds no `expect(` |
| 5 | **The arena, the tree builder and the dump.** Node storage addressed by index, the lossless token stream, `Parsed`; `tree.rs` rewritten to resolve each node's fields from the event stream in one pass; `KIND@start..end "text"` reimplemented over the arena (`mahout_syntax/src/red.rs:287-324` as new code), since every later slice's fixtures render through it. Drops `green.rs`, `red.rs`, `builder.rs` and `ast.rs:1-194`. Settles: how a node is addressed and how a tree prints. | `mahout_parser/src/tree.rs`, replacing `mahout_syntax/src/{green,red,builder,ast}.rs` | 166 + new | 156 | 1, 4 | `tests/typed_ast.rs` rewritten; a losslessness test (token texts concatenate to the input) on every fixture |
| 6 | **The parser driver and its events.** The 37 methods, `MAX_DEPTH = 512`, `MIN_THREAD_STACK`, the fuel and burst counters, the open-delimiter stack, the expected-set recorder, the trivia-hiding cursor. | `mahout_parser/src/{parser,event,input,lib}.rs` | 700 | — | 5 | the driver's own 4 `#[test]` (2 in `parser.rs`, 2 in `input.rs`); a recursion-cap test at 600 levels |
| 7 | **The grammar: expressions and strings.** The Pratt core, interpolation, contextual keywords; the fixture harness, which every later grammar slice reads. | `mahout_parser/src/grammar/{expr,strings,ckw}.rs`, `tests/support/mod.rs` | 1,132 | 3,975 | 6 | 161 snapshot tests (`fixtures_expr` 78, `fixtures_variable_productions` 29, `contextual_keywords` 31, `fixtures_strings` 23), each regenerated once against the arena's dump |
| 8 | **The grammar: statements.** Control flow, `switch`, `try`, namespaces and `use`, the alternative syntaxes, `__halt_compiler`, and the recovery paths. | `mahout_parser/src/grammar/mod.rs` (41 fns) | 1,110 | 1,638 | 7 | 57 snapshot tests (`fixtures_stmt` 35, `fixtures_recovery` 22), each regenerated once |
| 9 | **The grammar: declarations and types.** Classes, interfaces, traits, enums, members, hooks, attributes, parameters, and the type grammar. | `mahout_parser/src/grammar/{decl,types}.rs` (33 fns) | 772 | 1,428 | 7 | 35 snapshot tests (`fixtures_decl`), each regenerated once |
| 10 | **php's error text and the `parse` entry.** The 74 hand-placed sites re-worded to php's `syntax error, unexpected …, expecting …` with no code number (§6.1); `codes.rs` and `level.rs` dropped; `diag.rs` rewritten; `parse(&[u8], Options) -> Parsed` with the token stream, the line table, the tree and the diagnostics merged in php's phase order. Settles: the diagnostic's shape and what `parse` hands back. | `mahout_source/src/diag.rs`, `mahout_parser`'s diagnostic sites | 176 + new | — | 8, 9 | a witness per error family against the oracle's `php -l` text; the seven lexical fatals §6.1 names |

Slices 2 and 3 can be worked in either order after 1; 8 and 9 are independent of each other after 7.
Slice 4 is the only one that can run in parallel with the lexer slices, since it touches the generator
alone. Nothing outside slice 1 can start before it: every other slice names `SyntaxKind`. The dump is
slice 5's and not a slice of its own because a fixture cannot assert what it cannot render: slices 7, 8
and 9 each regenerate the fixture files they bring, once each, which is how §4.9's one regeneration
reads when the grammar arrives in three commits (§6.11).

`magic_method.rs` (82) and `builtin_attr.rs` (59) are on §13's take list and appear in no slice; §6.5
says why.

## 6. Where the source did not confirm the design

1. **The pinned commit is not in the clone the work points at.** `1b3104a8`, `d6460ef0` and `8f119938`
   are absent from `~/Projects/mahout`, whose HEAD `934e6507` (2026-07-02, 139 commits) is a different
   lineage: no `php_names.rs`, no `magic_method.rs`, no `builtin_attr.rs`, validate as one file rather
   than twelve, and no `unexpected_expecting` at all. `~/Projects/mahout_base` and `~/Projects/mahout_php`
   carry the pin. A record read at `~/Projects/mahout` HEAD would have priced validate at 1,580 ceiling
   lines instead of 7,726 and found three of §4.9's survivors missing.
2. **"549 snapshot tests survive" is two claims and only one holds.** 549 is exact — the `#[test]` count
   of `mahout_parser/tests/*.rs` at the pin — but 296 of them are `fixtures_validation.rs`, which §6.1
   sends to `php_compile`. 253 survive into `php_syntax`. §4.9 and §6.1 disagree about the same 296 tests
   and §4.9 is the one to correct.
3. **"The 80 hand-pinned `expecting` clauses" reproduces from no grep.** The measurable figures are 61
   sites with an explicit clause, 74 hand-placed error sites in `grammar/`, and 79 `probed 8.5.x`
   citations in `mahout_parser/src` (§3.7).
4. **"≈ 30-method driver" is 37 at the pin** (`parser.rs:218-637`); it is 30 in the other clone (§3.5).
5. **§13 routes `magic_method.rs` and `builtin_attr.rs` into `php_syntax`, and nothing in `php_syntax`
   reads them.** At the pin their consumers are `mahout_parser/src/validate/{attributes,extras,members_decls}.rs`,
   which §6.1 dissolves into the compiler, plus `mahout_linker` and `mahout_runtime`. Neither the lexer
   nor the grammar names either module. They belong with the compiler guards and the linker, in
   `php_compile` and `php_engine`; §13's first row is the line to fix.
6. **§13's generator is two files and needs three.** `codegen_ast.rs` + `ungram.rs` is 2,372 raw lines,
   exactly as stated, but `codegen.rs` (803 raw, 543) emits the `SyntaxKind` enum, the punctuation and
   keyword tables, the token-class hold and the `rustfmt` pass. Without it the other two emit nothing
   that compiles (§2.2).
7. **"Mahout's 93 upward walks were validate's (91)" — the 91 is exact, the 93 is not.** At the pin
   `.parent()` and `.ancestors()` occur 103 times across all of `crates/`: 91 in
   `mahout_parser/src/validate/`, 9 inside `mahout_syntax/src/red.rs` implementing the cursor itself,
   3 in `mahout_parser/src/reparse.rs`, and nowhere else. Every one of the 103 is in a file this copy
   drops, which is the conclusion §4.9 draws; the total is 103, not 93.
8. **"~1,060 unused generated lines" is 1,064, and 231 of them survive.** `generated/ast.rs:6976-8039`
   is the `Visitor` trait, the `visit` dispatch and `check_required_children`; only the last has a
   consumer, and ADR-0003 keeps its soak. 833 raw / 686 ceiling lines are the ones with no reader.
9. **§13's take row omits the two `mahout_source` files the copy cannot compile without.** It names
   `mahout_source/{line_index,magic_method,builtin_attr}.rs` and nothing else, but `mahout_lexer` and
   `mahout_parser` both `use mahout_source::{Diagnostic, Span, FileId, …}`: `lib.rs` (113) and `diag.rs`
   (176) have to come too, and §4 counts them. The row's third and fourth files are the two with no
   reader here (finding 5) and its first two are missing; §13's first row is wrong in both directions.
10. **`mahout_syntax/src/ast.rs` is two files in one, and only the first half is the arena's.**
   `:1-194` (128 ceiling) is `AstCast`, `AstNode`, `AstChildren` and the `support` scans, which the
   rewrite replaces; `:195-321` (88) is `parent_hook_call`, `hook_references_this_prop` and
   `hook_references_parent_hook`, property-hook helpers whose 11 call sites are every one of them in
   `validate/`. They are dropped, not rewritten, which moves 88 lines from §4's rewrite term to §1.6's
   drop table.
11. **§4.9's "one regeneration" reads per fixture file, not per commit.** The dump renders every
   fixture, so it has to land with the arena (slice 5) and not after the grammar; the grammar then
   arrives in three commits (slices 7, 8, 9) and each regenerates the fixture files it brings, once
   each. No snapshot is regenerated twice, which is what ADR-0003's Consequences asks for, but the
   253 do not pass through a single commit. A person reading each of the three diffs is the evidence,
   as it would have been for one.
12. **Not verified here.** §4.9's parse-throughput bar (≥ 25 MB/s) is quoted from Mahout and was not
   measured; §4.1's "M0 token differential 193,377/193,377" and "validator verdicts 99.95% on 172k
   files" were read in the design and not re-run. The size of the arena node storage and of the
   rewritten generator's emission is unknown until slice 4 exists (§4).
