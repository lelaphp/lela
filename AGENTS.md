# dax-php

dax-php is the third engine: a PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion. Twelve crates in `crates/`, one crate per extension in `ext/`, and `xtask`, the harness that measures all of it. Think of it as opcache's memory model applied to threads instead of processes: immutable compiled units shared by every request in one process, with no fork and no recompiling.

## What makes the third engine special?

Two predecessors, Mahout and Dax, spent their budgets beside the engine, on IRs, tiers and a semantics language, and measured 17× and 20× behind php's interpreter. This engine spends its budget on the engine. Four things we never trade away:

1. **One executor, and the oracle is the second opinion.** Every semantic lives in exactly one function, called from exactly one dispatch loop. Disagreement is measured against the pinned `php` binary, never against another in-house executor. The compiled tier runs the same kernels on the same frames and returns to the same loop.
2. **Zend's algorithms, Rust's compiler.** Where php's behaviour is observable (hash order, GC status, memory usage, dtor timing, float printing, sort order, error text), implement php's algorithm, not an equivalent one. Where it is not observable, be free.
3. **Immutable after publication, shared across threads.** Compiled units, linked classes, interned strings, constant arrays and published traces are immutable and `Sync`; request state is `Send` and single-threaded. An extension holds no lock, ever.
4. **Measured, then locked.** Every crate has a line ceiling, every cross-crate interface is a listed set of items in a lock file, and every kernel and native carries a witness. The instrument ships before the code it measures. Growth is a diff in the same commit, not a surprise in review.

## A note from the maintainer

I want Zend's design with better tooling, and nothing beside it. No second executor, no MIR, no DSL, no smarter architecture than the one PHP VM known to reach the numbers. When php-src has already specified an algorithm, copy it; when it has not, pick the smallest model that makes the correct behaviour unsurprising. The design of record says what to build, `seams.toml` says what the harness holds you to, and this file says how to work. Where they disagree, the manifest is the one that fires, and the design is the one to fix. This repository is worked on mostly by agents, under hooks: `xtask permit` judges every edit and shell command before it runs, and `xtask done` judges the diff before a turn ends. A denial is a rule with an ADR number, not an obstacle to route around.

## A small glossary

`CONTEXT.md` is the glossary and you read it first. Its `_Avoid_` lists are binding in code, rustdoc and commit messages, and `xtask vocab` checks them. The words this file leans on:

- **you** means the agent reading this file; **we** and **a person** mean Mitchell and the reviewers. Harness §11 lists what only a person does.
- **oracle** means the real `php` binary pinned in `oracle.toml`; **witness** means a PHP snippet in rustdoc beside the code it exercises, with the oracle's output pinned next to it.
- **seam** means a place where behaviour can be swapped without editing there; **lock** means the listed items that cross one; **manifest** means `seams.toml`, the file every rule sensor is derived from.
- **instrument** means one `xtask` subcommand that measures or judges something and writes a **reading**; **divergence** means one accepted difference from the oracle, a row in `docs/divergence.toml` with a reason a person wrote.
- **region** means a block of a Markdown file that `computed` renders from a command. Its body is never typed by hand.

## The five ways to hurt yourself

1. **Editing the harness's own files.** `seams.toml`, `oracle.toml` and `docs/divergence.toml` are a person's, and `permit` denies you the edit. Propose a rule or a divergence row in the commit message, never in the file.
2. **Hand-editing a computed region.** Run `computed run <file>` to refresh it. `computed run --force` discards a hand edit, and a person decides that.
3. **Authoring a witness verdict.** `exit=` in a witness is captured by `xtask witness --accept`, never typed. Read the diff it produces and say in the commit that you did.
4. **Skipping the hook.** `git commit --no-verify` is not available to you.
5. **Running a nightly lane locally.** The full corpus, the compile identity, the differential fuzz and the bench run in CI only. Run the per-crate subset the instrument names.

## Before you call it done

`xtask done` (harness §4.5) prints what is unmet since the merge base with `main`; the Stop hook, pre-push and CI run the same list. Walk it before proposing a change and say which entries applied: fresh passing readings for every touched crate, a witness on every kernel and native, a lock that differs in the same diff, no open divergence on a changed scope, `permit` over the diff denying nothing, and the manifest and the registry untouched or changed by a person.

## Verifying and committing

- `cargo xtask <instrument>` runs one instrument; `cargo xtask --help` lists the ones built so far. The smallest proof is the instrument for the crate you touched. The harness itself is tested by `cargo test -p xtask`, and every manifest rule has a fixture under `xtask/fixtures/` that must fail.
- A change to behaviour comes with a witness beside the kernel or native, pinned by `--accept`. The witness is the test; write it before the body when you can. The phpt subset for an extension is `php-src/ext/<name>/tests`. A bench number is a claim a person signs off, with the A/A control read by a person: state the measurement, never the conclusion.
- Commits are plain language, one concern, the change in one sentence: `xtask vocab: the glossary as a sensor`. If the message says "also", split it. The lock line, the witness and the regenerated `ext/clippy.toml` travel with the code that needs them. A rule you want, or a divergence you found, is a paragraph in the message proposing it, with the fixture that would fail.

## Documentation and where code lives

- `third-engine-design.md` is the design of record, `jit-design.md` the compiled tier, `harness-design.md` the harness: living documents with a ledger on top, revised by a person, cited by section. `research/` holds the earlier revisions and the research behind them, read for history only. `docs/adr/` is the list of reversals; read the ADR before touching what it names.
- Design documents carry no numbers of their own; budgets and interfaces live in `seams.toml`, the oracle in `oracle.toml`. A fact that changes is a region, rendered by `computed run` and held by `computed check`. No tickets, plans or agent scratch files in the repository; keep working material in your scratchpad.
- `crates/php_syntax`, `php_compile` and `php_unit` take source to a unit; `php_heap` is the memory model and the only `unsafe`; `php_engine` holds the kernels, dispatch, `execute`, the linker, `serve` and `api`, with `php_jit` the compiled tier behind a feature; `php_ext` is the facade every extension depends on and `php_bundle` the enabled set; `php_proto`, `php_sapi` and `php` are the codecs, the hosts and the binary.
- `ext/<name>` is one crate per extension, laid down by `cargo xtask new-ext`; read `ext/AGENTS.md` before touching one. `xtask` is the harness and `xtask/fixtures/` its negative controls. `data/` holds the censuses and extraction records the design cites. Most of `crates/` is not created yet; the layout regions below say what exists.

## Taste

- Types over comments: newtypes for indices and handles, enums for state machines, `&mut Request` on anything that can run PHP code. No panic reaches a request; `unwrap` and `expect` are denied in engine crates. `unsafe` lives in four homes, one operation per block, each with a `SAFETY` comment. `pub` crosses a crate boundary only through a locked module. Generated code is generated in `build.rs` and never committed; the one exception, `ext/clippy.toml`, is regenerated and diffed.
- Copy php's algorithm where the behaviour is observable; drop the cleverness where it is not. The floor is php's interpreter by construction; the ceiling comes from threads and whole-program LTO, not a smarter IR. If a rule here fights the task in front of you, say so loudly and get a person's sign-off before breaking it.

## The facts, computed

The regions below are rendered by `computed run AGENTS.md` and held by `computed check` in CI. The volatile readings (budget, phpt, witness, mutants, divergence, permits, git) live in `.claude/harness.local.md`, which the SessionStart hook renders and prints after this file.

<!-- computed exec cmd="ls third-engine-design*.md jit-design*.md harness-design.md | sort -V" inputs=third-engine-design*.md,jit-design*.md,harness-design.md name=design-of-record | do not edit; run computed -->

harness-design.md
jit-design.md
third-engine-design.md

<!-- /computed in=46042eb80cdd97eb805e98c5c80eac532858b6ff659811094427d2383cb86122 out=59c14cd45b0ce3a636958ac2b89a22d1b68161882044aa0617b42f93947e231e -->
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
| heap | heap.lock |  |  | skip |
| unit | unit.lock |  |  | skip |
| ops | ops.lock |  |  | skip |
| api | api.lock |  | 400 | skip |
| sapi | sapi.lock |  |  | skip |
| tier | tier.lock |  |  | skip |

<!-- /computed in=872baa26d867d169dbddbabc9d4946d2bde70a53594593ce83fc6953475b29e0 out=e2521776fafb15658d498ed026b1ee34916cdebfd1efe51daf451437d6292967 -->
<!-- computed exec cmd="cargo xtask status --section rules --format md" inputs=seams.toml name=rules | do not edit; run computed -->

| rule | adr | predicate | reading | text |
|---|---|---|---|---|
| ext.no-locks | ADR-0005 | deny_types | fail | Extension state is request-scoped or immutable; no lock or cell in ext/*. Cross-request caching is api::Cache. |
| unsafe.homes | ADR-0006 | allow | pass | unsafe is permitted in four places; see design §4.1. Every other crate is #![forbid(unsafe_code)]; php_jit and an extension with bindings are #![deny(unsafe_code)] with the allowance on the named module alone. |
| unsafe.one-op |  | deny_lints | no reading | Every unsafe block is one operation with a SAFETY comment. |
| lints.no-panic |  | deny_lints | no reading | Internal invariant failures are InternalError fatals with a message, not panics (design §10). |
| engine.reentry-signature |  | check | no reading | A function in php_engine that calls execute or release takes &mut Request. |
| api.no-hot |  | deny_pattern | no reading | php_heap::hot is for the handlers; api never re-exports it (design §4.4, §10). |
| api.lock-line |  | on | skip | A new pub item in api needs its api.lock line in the same commit; api ≤ 400 items. |
| ext.pub-beyond-extension |  | on | no reading | api-check prints each ext crate's pub items beyond extension(); anything beyond the pdo driver trait and the random handle needs a line in the design (§12). |
| witness.exit-authored |  | deny_pattern | no reading | exit= in a witness is captured by `xtask witness --accept`, never authored. |
| witness.coverage |  | check | no reading | A kernel or a native without a witness fails CI (design §8). |
| vocab.avoid |  | avoid_from | pass | The glossary's _Avoid_ words appear in no public item name or rustdoc prose; a lock line mirrors php's names and is not read. |
| generated.committed |  | generated | pass | ext/clippy.toml is generated from the manifest and committed; a stale copy fails budget. |
| git.no-verify |  | deny_argv | no reading | Commits run the pre-commit hook; --no-verify is not available to agents. |
| lanes.nightly |  | deny_pattern | fail | Nightly lanes run in CI only; run the per-crate subset. |
| computed.force |  | deny_pattern | fail | `computed run --force` discards a hand edit; a person decides that. |
| harness.self |  | deny | fail | Harness configuration, the oracle pin and the divergence registry are edited by a person. |
| divergence.reason |  | check | pass | A divergence row with status accepted has a reason a person wrote; done fails while an open row touches the scope (harness §4.3). |

<!-- /computed in=0d3212222cb1bf000632877dcb2fcc0b0d3a22e1c0d2276d24a5910f957b097e out=da02cf02b585feb1cf5fe3a23ecde3f42a343b29ec38a97d6c747ab11ef4545f -->
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
  clippy             `cargo clippy` with every warning denied, on every target of each crate, as the reading `done` reads; one reading per crate
  unsafe-audit       Allow `unsafe` only in the homes the manifest's unsafe.homes rule names
  generate           Write the files the manifest's `generated` rules render (ext/clippy.toml)
  divergence-reason  Hold the divergence registry to the manifest rule whose `check` names this instrument: an accepted row has a reason a person wrote
  new-ext            Lay one extension crate down at ext/<name> in the template topology: the facade as its one dependency, `extension()`, state on the value, a test through the capture sink, a witness, ext/clippy.toml rewritten and the row in ext/AGENTS.md; refuses an existing one
  vocab              The glossary as a sensor: `_Avoid_` words in public item names and rustdoc prose, never in a lock; one reading per crate

Options:
      --root <DIR>  The workspace root; defaults to the workspace this binary was built in
  -h, --help        Print help
```
<!-- /computed in=7b4a61b69bfade5eb1e79e2ff13a0dfbd4cc0e2b104d5aec5741602ea2d152aa out=ad666c01bd104716127bbec640a8f89208c14c28d831af96fc3c976fc4c4c491 -->
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
