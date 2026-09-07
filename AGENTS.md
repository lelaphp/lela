# dax-php

dax-php is the third engine: a PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion. Twelve crates in `crates/`, one crate per extension in `ext/`, and `xtask`, the harness that measures all of it.

You can think of it as opcache's memory model applied to threads instead of processes: immutable compiled units shared by every request in one process, with no fork and no recompiling.

## What makes the third engine special?

Two predecessors, Mahout and Dax, spent their budgets beside the engine, on IRs, tiers and a semantics language, and measured 17× and 20× behind php's interpreter. This engine spends its budget on the engine. Here is what we never trade away.

### 1. One executor, and the oracle is the second opinion

Every semantic lives in exactly one function, called from exactly one dispatch loop. Disagreement is measured against the pinned `php` binary, never against another in-house executor. The compiled tier runs the same kernels on the same frames and returns to the same loop.

### 2. Zend's algorithms, Rust's compiler

Where php's behaviour is observable (hash order, GC status, memory usage, dtor timing, float printing, sort order, error text), implement php's algorithm, not an equivalent one. Where it is not observable, be free.

### 3. Immutable after publication, shared across threads

Compiled units, linked classes, interned strings, constant arrays and published traces are immutable and `Sync`; request state is `Send` and single-threaded. An extension holds no lock, ever.

### 4. Measured, then locked

Every crate has a line ceiling, every cross-crate interface is a listed set of items in a lock file, and every kernel and native carries a witness. The instrument ships before the code it measures. Growth is a diff in the same commit, not a surprise in review.

## A note from the maintainer

I want Zend's design with better tooling, and nothing beside it. No second executor, no MIR, no DSL, no smarter architecture than the one PHP VM known to reach the numbers. When php-src has already specified an algorithm, copy it; when it has not, pick the smallest model that makes the correct behaviour unsurprising.

The rest of this document helps you move through the codebase. The design of record says what to build, `seams.toml` says what the harness holds you to, and this file says how to work. Where they disagree, the manifest is the one that fires, and the design is the one to fix.

Of note: this repository is worked on mostly by agents, under hooks. `xtask permit` judges every edit and shell command before it runs, and `xtask done` judges the diff before a turn ends. A denial is a rule with an ADR number, not an obstacle to route around.

## A small glossary

`CONTEXT.md` is the glossary and you read it first. Its `_Avoid_` lists are binding in code, rustdoc and commit messages, and `xtask vocab` checks them. The words this file leans on:

- **you** means the agent reading this file and changing the engine.
- **we** and **a person** mean Mitchell and the people reviewing the work. Harness §11 lists what only a person does.
- **oracle** means the real `php` binary pinned in `oracle.toml`, the only second opinion.
- **witness** means a PHP snippet in rustdoc beside the code it exercises, with the oracle's output pinned next to it.
- **seam** means a place where behaviour can be swapped without editing there; **lock** means the listed items that cross one.
- **manifest** means `seams.toml`, the one file every rule sensor is derived from.
- **instrument** means one `xtask` subcommand that measures or judges something and writes a **reading**.
- **divergence** means one accepted difference from the oracle, a row in `docs/divergence.toml` with a reason a person wrote.
- **region** means a block of a Markdown file that `computed` renders from a command. Its body is never typed by hand.

## The five ways to hurt yourself

1. **Editing the harness's own files.** `seams.toml`, `oracle.toml` and `docs/divergence.toml` are written by a person, and `permit` denies you the edit. Propose a rule or a divergence row in the commit message, never in the file.
2. **Hand-editing a computed region.** The body between `<!-- computed` and `<!-- /computed` is rendered. Run `computed run <file>` to refresh it. `computed run --force` discards a hand edit, and a person decides that.
3. **Authoring a witness verdict.** `exit=` in a witness is captured by `xtask witness --accept`, never typed. Read the diff `--accept` produces and say in the commit that you did.
4. **Skipping the hook.** `git commit --no-verify` is not available to you.
5. **Running a nightly lane locally.** The full corpus, the 172k compile identity, the differential fuzz and the bench run in CI only. Run the per-crate subset the instrument names.

## Before you call it done

`xtask done` (harness §4.5) prints what is unmet since the merge base with `main`; pre-push and CI run it too. Walk the list before proposing a change and say which entries applied:

- **Fresh readings.** Every touched crate has a passing reading for clippy, witness, api-check, budget and unsafe-audit, and for phpt on its own subset. Fresh means the reading's tree hash matches the crate now.
- **Every kernel and native has a witness.** A new one without a witness fails CI.
- **A lock that differs is in the diff.** A new `pub` item across a seam gets its lock line in the same commit; `api` stays at 400 items or fewer.
- **No open divergence touches a changed scope.** An open row has no reason yet. A person writes one, or the difference gets fixed.
- **`permit` over the diff denies nothing.**
- **The manifest and the registry are untouched**, or changed by a person.

## Verifying

- `cargo xtask <instrument>` runs one instrument; `cargo xtask --help` lists the ones built so far, and `cargo xtask status --section <name>` renders the readings. The smallest proof is the instrument for the crate you touched.
- A change to behaviour comes with a witness beside the kernel or native, and the oracle's output pinned by `--accept`. The witness is the test. Write it before the body when you can.
- The phpt subset for an extension is `php-src/ext/<name>/tests`, run with `cargo xtask phpt --subset`.
- The harness is tested by `cargo test -p xtask`. Every manifest rule has a fixture under `xtask/fixtures/` that must fail.
- A bench number is a claim a person signs off, with the A/A control read by a person. State the measurement, never the conclusion.

## Commits

- Plain language, the change in one sentence: `xtask vocab: the glossary as a sensor`. The `_Avoid_` words stay out of the message too.
- The lock line, the witness and the regenerated `ext/clippy.toml` travel in the same commit as the code that needs them.
- A rule you want, or a divergence you found, is a paragraph in the commit message proposing it, with the fixture that would fail. A person moves it into the file.
- One concern per commit. If the message says "also", split it.

## Documentation

Agents can read the code, and rustdoc with witnesses is the documentation of a kernel. Prose exists for what the code cannot carry.

- `third-engine-design.md` is the design of record, `jit-design.md` for the compiled tier, `harness-design.md` for the harness. Each is one living document with a ledger on top; a revision edits the sections it changes and adds one ledger entry. Revisions are a person's. You cite them by section.
- `research/` holds the earlier revisions (`*.rN.md`) and the research behind them, kept unedited as records. Read them for history, never for anything current.
- `docs/adr/` is the list of reversals, written only when a decision in the design is reversed. Read the ADR before touching what it names; a manifest rule prints its ADR number at the failure.
- Design documents carry no numbers of their own. Budgets and interfaces live in `seams.toml`, the oracle in `oracle.toml`, and the documents cite those files.
- No tickets, plans, maps or agent scratch files in the repository. The phpt burndown and the divergence registry are the status. Keep working material in your scratchpad.
- A region is how a document states a fact that changes: `computed run` renders it and `computed check` holds it in CI. Prose in this file is decisions; facts are the regions at the bottom.

## How it works

`php_syntax` lexes a file to a lossless token stream and a typed tree; `php_compile` walks the tree in one pass to a _unit_, op arrays identical to php's own. The engine _publishes_ the unit into its cache, immutable and shared across threads, and _serves_ a request: seed the superglobals, run `execute` over the frames, commit headers, shut down, report. Every _kernel_ is one function called from one dispatch loop; the compiled tier is a seam on that loop, asked only at _safepoints_. A _native_ is a PHP function whose body is Rust, declared once in its signature, reaching the engine through one locked module, `php_engine::api`, that `php_ext` re-exports. A _host_ (CLI, CGI, FastCGI, the dev server, embed) builds a request description, supplies an output sink, and crosses `serve` and nothing else.

Full vocabulary: `CONTEXT.md`. The design: `third-engine-design.md` §4 for the seams, §5 for execution.

## Where code lives

- `crates/php_syntax`, `php_compile`, `php_unit` - source to unit. The opcode enum is generated from `ops.toml` in `build.rs`.
- `crates/php_heap` - the memory model and the only `unsafe` in the engine. Miri per commit.
- `crates/php_engine` - kernels, dispatch, `execute`, the linker, `serve`, the unit cache and `api`. `php_jit` is the compiled tier behind a feature.
- `crates/php_ext` - the facade every extension depends on; `api.lock` is its public surface. `php_bundle` is the enabled set in module-startup order.
- `crates/php_proto`, `php_sapi`, `php` - protocol codecs, hosts, the binary.
- `ext/<name>` - one crate per extension, laid down by `cargo xtask new-ext`. Read `ext/AGENTS.md` before touching one.
- `xtask` - the harness. `xtask/fixtures/` are its negative controls.
- `data/` - the predecessor censuses and the extraction records the design cites; `data/INDEX.md` lists them.

Most of `crates/` is not created yet. The layout regions at the bottom say what exists today.

## Taste

- Types over comments. Newtypes for indices and handles, enums for state machines, `&mut Request` on anything that can run PHP code. A function without that parameter cannot re-enter.
- No panic reaches a request. A PHP fatal is a value; an internal invariant failure is an `InternalError` with a message. `unwrap` and `expect` are denied in engine crates.
- `unsafe` lives in four homes, one operation per block, each with a `SAFETY` comment.
- `pub` crosses a crate boundary only through a locked module. Inside a crate, modules are as fine-grained as the tests need.
- Generated code is generated in `build.rs` and never committed. The one exception, `ext/clippy.toml`, is regenerated and diffed.
- Copy php's algorithm where the behaviour is observable; drop the cleverness where it is not. The floor is php's interpreter by construction. The ceiling comes from threads and whole-program LTO, not from a smarter IR.
- If a rule here fights the task in front of you, say so loudly and get a person's sign-off before breaking it.

## The facts, computed

The regions below are rendered by `computed run AGENTS.md` and held by `computed check` in CI. The volatile readings (budget, phpt, witness, mutants, divergence, permits, git) live in `.claude/harness.local.md`, which the SessionStart hook renders and prints after this file.

<!-- computed exec cmd="ls third-engine-design*.md jit-design*.md harness-design.md | sort -V" inputs=third-engine-design*.md,jit-design*.md,harness-design.md name=design-of-record | do not edit; run computed -->

harness-design.md
jit-design.md
third-engine-design.md

<!-- /computed in=4a8b6e411ed76b756e0ea9af642dba4503534de19de30e5215c64340edeaa529 out=59c14cd45b0ce3a636958ac2b89a22d1b68161882044aa0617b42f93947e231e -->
<!-- computed exec cmd=scripts/adr-index.sh inputs=docs/adr/*.md,scripts/adr-index.sh name=adrs | do not edit; run computed -->

- ADR-0001 (accepted, 2026-09-05): The unit format, including the opcode enum, is a leaf crate below both the compiler and the engine
- ADR-0002 (accepted, 2026-09-05): A native entry function takes the engine's concrete request type, not a trait object
- ADR-0003 (accepted, 2026-09-05): The syntax tree is typed nodes in a safe arena over a lossless token stream, not a green/red tree
- ADR-0004 (accepted, 2026-09-05): Extensions are one crate each under `ext/`, with `php_ext` as the facade below them and `php_bundle` as the enabled set above them
- ADR-0005 (accepted, 2026-09-05): Extension state is request-scoped or immutable engine-scoped, and the engine owns the only cross-request cache
- ADR-0006 (accepted, 2026-09-07): The compiled tier is a seam on `execute` with one engine-level word per safepoint, not a second executor with a per-request feedback table

<!-- /computed in=20891f40b50e340bb71e529d7f78a532afdb0b6ccd0713046006fcc6f8099e29 out=1f3c18c06af7268751e776de5e0802d533ba51482ac1afe4d73c8a3cc7b350cf -->
<!-- computed exec cmd="cargo xtask status --section locks --format md" inputs=seams.toml,crates/**/*.lock name=seams | do not edit; run computed -->

| seam | lock | items | ceiling | reading |
|---|---|---|---|---|
| heap | heap.lock |  |  | no reading |
| unit | unit.lock |  |  | no reading |
| ops | ops.lock |  |  | no reading |
| api | api.lock |  | 400 | no reading |
| sapi | sapi.lock |  |  | no reading |
| tier | tier.lock |  |  | no reading |

<!-- /computed in=050bf6d3f36ffaabb2788a7033f3a30b41100c79e633fe7f1f167879df0ce01f out=174b8c9b719cd9dcad1007dba5fe338e1f2300a7798e0bc43e1bb6654f8dc2d9 -->
<!-- computed exec cmd="cargo xtask status --section rules --format md" inputs=seams.toml name=rules | do not edit; run computed -->

| rule | adr | predicate | reading | text |
|---|---|---|---|---|
| ext.no-locks | ADR-0005 | deny_types | no reading | Extension state is request-scoped or immutable; no lock or cell in ext/*. Cross-request caching is api::Cache. |
| unsafe.homes | ADR-0006 | allow | no reading | unsafe is permitted in four places; see design §4.1. Every other crate is #![forbid(unsafe_code)]; php_jit and an extension with bindings are #![deny(unsafe_code)] with the allowance on the named module alone. |
| unsafe.one-op |  | deny_lints | no reading | Every unsafe block is one operation with a SAFETY comment. |
| lints.no-panic |  | deny_lints | no reading | Internal invariant failures are InternalError fatals with a message, not panics (design §10). |
| engine.reentry-signature |  | check | no reading | A function in php_engine that calls execute or release takes &mut Request. |
| api.no-hot |  | deny_pattern | no reading | php_heap::hot is for the handlers; api never re-exports it (design §4.4, §10). |
| api.lock-line |  | on | no reading | A new pub item in api needs its api.lock line in the same commit; api ≤ 400 items. |
| ext.pub-beyond-extension |  | on | no reading | api-check prints each ext crate's pub items beyond extension(); anything beyond the pdo driver trait and the random handle needs a line in the design (§12). |
| witness.exit-authored |  | deny_pattern | no reading | exit= in a witness is captured by `xtask witness --accept`, never authored. |
| witness.coverage |  | check | no reading | A kernel or a native without a witness fails CI (design §8). |
| vocab.avoid |  | avoid_from | no reading | The glossary's _Avoid_ words appear in no public item name, rustdoc prose or api.lock line. |
| generated.committed |  | generated | no reading | ext/clippy.toml is generated from the manifest and committed; a stale copy fails budget. |
| git.no-verify |  | deny_argv | no reading | Commits run the pre-commit hook; --no-verify is not available to agents. |
| lanes.nightly |  | deny_pattern | no reading | Nightly lanes run in CI only; run the per-crate subset. |
| computed.force |  | deny_pattern | no reading | `computed run --force` discards a hand edit; a person decides that. |
| harness.self |  | deny | no reading | Harness configuration, the oracle pin and the divergence registry are edited by a person. |
| divergence.reason |  | check | no reading | A divergence row with status accepted has a reason a person wrote; done fails while an open row touches the scope (harness §4.3). |

<!-- /computed in=0b24ac67ac3208d3e7249545fc0106ca3f0a19f2af23b496014fa207c16be965 out=006a86a2462aae2690ff041ff10c4d58a277a32daab194d97008ca790d0868ab -->
<!-- computed exec cmd="cargo xtask --help" inputs=xtask/src/cli.rs name=instruments as=fence | do not edit; run computed -->
```
The harness for the third engine

Usage: xtask [OPTIONS] <COMMAND>

Commands:
  api-check          Hold the seams: regenerate and diff each lock, check crate edges, `use` paths and the bundle order; one reading per seam and per crate
  status             Render one section from the manifest and the store, or with `--session` the map plus the local facts; computes nothing else
  permit             Judge one intended tool call from the `PreToolUse` hook's stdin; the decision is the JSON on stdout and the exit is 0 always
  done               The definition of done: print the conditions unmet for the changes since the base and exit 1 while any is; the Stop hook, pre-push and CI read the same list
  budget             Hold every crate to its line ceiling and the workspace to [budget]; refuse committed generated code and a stale generated file
  unsafe-audit       Allow `unsafe` only in the homes the manifest's unsafe.homes rule names
  generate           Write the files the manifest's `generated` rules render (ext/clippy.toml)
  divergence-reason  Hold the divergence registry to the manifest rule whose `check` names this instrument: an accepted row has a reason a person wrote
  new-ext            Lay one extension crate down at ext/<name> in the template topology: the facade as its one dependency, `extension()`, state on the value, a test through the capture sink, a witness, ext/clippy.toml rewritten and the row in ext/AGENTS.md; refuses an existing one
  vocab              The glossary as a sensor: `_Avoid_` words in public item names, rustdoc prose and the lock; one reading per crate

Options:
      --root <DIR>  The workspace root; defaults to the workspace this binary was built in
  -h, --help        Print help
```
<!-- /computed in=cb363c0bbcf30f72b6e3a2b717aa2226a408a272bba80008f1a09854df17199e out=5ee497166780977a8c785b5b33fd8e725f2105f093aeddbae66e8b5394d7c39a -->
<!-- computed tree src=crates depth=1 name=layout | do not edit; run computed -->
```
.
└── php_ext
```
<!-- /computed in=9f0136b5f4fd541e073442f8a06659d0111c9413126e447c87d6793e6e866a6c out=d686c3a3adc50126cc7f7622e563fd7e0fb76a7713bde7b4246769baab3fc229 -->
<!-- computed tree src=ext depth=1 name=layout-ext | do not edit; run computed -->
```
.
├── AGENTS.md
└── clippy.toml
```
<!-- /computed in=b79427b807153f594393f176c0cf681c8e5c6d8daf4b63a6a12b4e7de1fe2b6a out=17d2f15969dd83d101598d8d652d14071b4363f72c9f9092dfb9323b6938ee54 -->
