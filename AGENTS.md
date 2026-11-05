# dax-php

dax-php is the third engine: a PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion. Its crates are in `crates/`, one per extension in `ext/`, one per host in `sapi/`, and `xtask`, the harness that measures all of it, counted below from the manifest. Think of it as opcache's memory model applied to threads instead of processes: immutable compiled units shared by every request in one process, with no fork and no recompiling.

<!-- computed exec cmd="cargo xtask status --section crates --tracked" inputs=seams.toml,xtask/src/status.rs,xtask/src/status/graph.rs name=crates | do not edit; run computed -->

| directory | crates | behind a feature |
|---|---|---|
| crates/ | 16 | 1 |
| ext/ | 59 | 31 |
| sapi/ | 9 | 0 |
| xtask | 1 | 0 |

<!-- /computed in=92adf5f69cc602d3dc1a0df0f6e1b0a915f4d2c8b05b8c7ed297f57cc18b956b out=6ba0892c9255d463a40e6c1df9a6c86358dea5a0b2a04df470497c174dc89391 -->

## What makes the third engine special?

Two predecessors, Mahout and Dax, spent their budgets beside the engine, on IRs, tiers and a semantics language, and measured 17× and 20× behind php's interpreter. This engine spends its budget on the engine. Four things we never trade away:

- **§3.2 One executor, and the oracle is the second opinion.** Every semantic lives in exactly one function, called from exactly one dispatch loop. Disagreement is measured against the pinned `php` binary, never against another in-house executor. The compiled tier runs the same kernels on the same frames and returns to the same loop.
- **§3.1 Zend's algorithms, Rust's compiler.** Where php's behaviour is observable (hash order, GC status, memory usage, dtor timing, float printing, sort order, error text), implement php's algorithm, not an equivalent one. Where it is not observable, be free.
- **§3.4 Immutable after publication, shared across threads.** Compiled units, linked classes, interned strings, constant arrays and published traces are immutable and `Sync`; request state is `Send` and single-threaded. An extension holds no lock, ever.
- **§3.7–8 Measured, then locked.** Every crate has a line ceiling, every cross-crate interface is a listed set of items in a lock file, and every kernel and native carries a witness. The instrument ships before the code it measures. Growth is a diff in the same commit, not a surprise in review.

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

1. **Editing the harness's own files.** `seams.toml`, `oracle.toml` and `docs/divergence.toml` are a person's to decide, and `permit` denies you the edit. While nothing is settled, propose a rule or a divergence row in the commit message, never in the file. Once a person has settled the row on a ticket, land it with `cargo xtask steer <file> --from <proposal>`, which writes only where the proposal adds: one changed ceiling, one widened edge, one dropped rule and nothing is written, because a change to what already binds takes a person's hand.
2. **Hand-editing a computed region.** Run `computed run <file>` to refresh it. `computed run --force` discards a hand edit, and only a person runs it.
3. **Authoring a witness verdict.** `exit=` in a witness is captured by `xtask witness --accept`, never typed. Read the diff it produces and say in the commit that you did.
4. **Skipping the hook.** `git commit --no-verify` is not available to you.
5. **Running a nightly lane locally.** The full corpus, the compile identity, the differential fuzz and the bench run in CI only. Run the per-crate subset the instrument names.

## Before you call it done

`xtask done` (harness §4.5) prints what is unmet since the merge base with `main`; the Stop hook, pre-push and CI run the same list. Walk it before proposing a change and say which entries applied: fresh passing readings for every touched crate, a witness on every kernel and native, a lock that differs in the same diff, no open divergence on a changed scope, `permit` over the diff denying nothing, and the manifest and the registry untouched or changed by a person.

## Finish the whole task

A session here runs on its own, and a turn ends at a hold, never at a question. The holds, by layer:

- **The Claude hooks bind.** `permit` judges each Edit, Write and Bash call before it runs; `done` holds the turn at the Stop and warns at `SubagentStop`.
- **The git hooks measure.** pre-commit runs the instruments over what is staged, and pre-push runs `done`.
- **The ticket instrument closes.** `xtask ticket ready` refuses while `done` is unmet in the builder's worktree, and only `xtask ticket close` closes a ticket.

The request sets the scope, and all of it is the deliverable. Take each reversible step it implies without asking. A problem described or a question asked gets an assessment, and the tree changes only when a change is asked for. Before a command changes the system's state, check that the evidence supports that act. End on finished work: a last paragraph that plans, promises or asks is work to do now, with tool calls. Where one part is blocked, finish the rest and say what waits on what.

A session stops where `permit` denies an edit, and at an act harness §11 gives a person, such as a manifest row, a divergence reason or an oracle bump. File each as a `for-a-person` ticket, name its number, and wire the work that waits on it with `gh issue edit <ticket> --add-blocking <n>`. Whatever else you find beyond the task, file a ticket and name its number before the turn ends, `ready-for-agent` when an agent may take it. A ticket carries what the task did not ask for, never a part of it.

## Verifying and committing

- `cargo xtask <instrument>` runs one instrument; `cargo xtask --help` lists the ones built so far. The smallest proof is the instrument for the crate you touched. The harness itself is tested by `cargo test -p xtask`, and every manifest rule has a fixture under `xtask/fixtures/` that must fail.
- A change to behaviour comes with a witness beside the kernel or native, pinned by `--accept`. The witness is the test; write it before the body when you can. The phpt subset for an extension is every `.phpt` under `php-src/ext/<name>`, as run-tests.php walks the directory. A bench number is a claim a person signs off, with the A/A control read by a person: state the measurement, never the conclusion.
- Stage and commit by pathspec, `git add -- <paths>` and `git commit -F <file> -- <paths>`: `git add -A`, `git add .` or a commit that names no path takes whatever else stands in the index, a person's work among it. One session writes in a checkout, its owner: `xtask worktree add` records the session that lays a worktree down, `xtask worktree own` claims main's checkout or the merge worktree and `--release` lets one go, and `permit` and pre-commit refuse another session's writes and commits there. Your subagents are your session; a person is refused nowhere. The shell is zsh, which does not split an unquoted `$P`: write the paths out, or use an array, `P=(a b)` and `git add -- "${P[@]}"`. A merge, cherry-pick, revert or rebase in progress commits the whole index, and it is its checkout's owner's to commit or abort; in a checkout you do not own, `permit` refuses the git commands that write its index or step it, so work in your own worktree. The message file is named for the branch or its ticket, never `msg.txt`: the scratchpad is shared across the checkout's worktrees, so a generic name is whichever session wrote it last. Commits are plain language, one concern, the change in one sentence: `xtask vocab: the glossary as a sensor`. If the message says "also", split it. The lock line, the witness and the regenerated `ext/clippy.toml` travel with the code that needs them. A rule you want, or a divergence you found, is a paragraph in the message proposing it, with the fixture that would fail. What the work opened is the close's `--opens`, recorded as a reading, not a trailer in the message. A ticket's acceptance criteria are boxes its own label can tick. Where the work waits on a decision, the decision is a `for-a-person` ticket of its own and the work is wired to wait on it; a box in an agent's ticket that waits on a person's decision closes nothing however finished the code is.

## Documentation and where code lives

- `third-engine-design.md` is the design of record, `jit-design.md` the compiled tier, `harness-design.md` the harness: living documents with a ledger on top, cited by section. What a revision says belongs to a person; typing it does not. Where a ticket carries the sentence a person has settled, landing it with its ledger row is yours, and the commit names the ticket that settled it. A revision that states what a ruled ticket built, and reverses, widens or adds to no ruling, is yours to word and land, with its ledger row naming the ruling and the ticket; the person reads that row at the merge, and a sentence they revert becomes a ticket of theirs. A deletion-test argument, an ADR, and a sentence answering a question the design is silent on stay a person's, and any other revision nobody has settled stays a question you file. `research/` holds the earlier revisions and the research behind them, read for history only. `docs/adr/` is the list of reversals; read the ADR before touching what it names.
- Design documents carry no numbers of their own; budgets and interfaces live in `seams.toml`, the oracle in `oracle.toml`. A fact that changes is a region, rendered by `computed run` and held by `computed check`. No tickets, plans or agent scratch files in the repository; keep working material in your scratchpad.
- `crates/php_syntax`, `php_compile` and `php_unit` take source to a unit; `php_heap` is the memory model and the stacks, and `php_process` the process and its file descriptors (`fork`, the signal pool, `LC_CTYPE`, the process identity, the fd calls), each an `unsafe` home the `unsafe.homes` rule names; `php_engine` holds the kernels, dispatch, `execute`, the linker, `serve` and `api`, with `php_jit` the compiled tier behind a feature; `php_ext` is the facade every extension depends on and `php_bundle` the enabled set; `php_proto` is the codecs and `php_sapi` the facade every host depends on.
- `ext/<name>` is one crate per extension, laid down by `cargo xtask new-ext`; read `ext/AGENTS.md` before touching one. `sapi/<name>` is one crate per host, each its binary (`sapi/cli` builds `php`, `sapi/cgi` `php-cgi`; ADR-0008). `xtask` is the harness and `xtask/fixtures/` its negative controls. `data/` holds the censuses and extraction records the design cites. Most of `crates/` is not created yet; the layout regions below say what exists.
- Finding code: a Rust name you know, or who calls it, is `cq`'s: `cq find <name>`, then `cq body <handle>` to read it and `cq refs <handle>` for its callers, and `cq area <path>` for what is around a path. Text, strings, docs, TOML and php-src's C are grep's, since cq matches names and not bodies. A brief to a subagent carries this line.

## Taste

- Types over comments: newtypes for indices and handles, enums for state machines, `&mut Request` on anything that can run PHP code. No panic reaches a request; `unwrap` and `expect` are denied in engine crates. `unsafe` lives in the homes the `unsafe.homes` rule names, one operation per block, each with a `SAFETY` comment. `pub` crosses a crate boundary only through a locked module. Generated code is generated in `build.rs` and never committed; the one exception, `ext/clippy.toml`, is regenerated and diffed.
- Copy php's algorithm where the behaviour is observable; drop the cleverness where it is not. The floor is php's interpreter by construction; the ceiling comes from threads and whole-program LTO, not a smarter IR. If a rule here fights the task in front of you, say so loudly and get a person's sign-off before breaking it.

## The facts, computed

The regions below are rendered by `computed run AGENTS.md` and held by `computed check` in CI. The volatile readings (budget, phpt, witness, mutants, port-map, divergence, permits, git) live in `.claude/harness.local.md`, which the SessionStart hook renders and prints after this file. Every manifest rule, with its ADR and predicate: `cargo xtask status --section rules`.

<!-- computed exec cmd="ls third-engine-design*.md jit-design*.md harness-design.md | sort -V" inputs=third-engine-design*.md,jit-design*.md,harness-design.md name=design-of-record | do not edit; run computed -->

harness-design.md
jit-design.md
third-engine-design.md

<!-- /computed in=eaee2eef9e517226545e981082f0c49b8639b812142c721ab5ff03c676d544cb out=59c14cd45b0ce3a636958ac2b89a22d1b68161882044aa0617b42f93947e231e -->
<!-- computed exec cmd=scripts/adr-index.sh inputs=docs/adr/*.md,scripts/adr-index.sh name=adrs | do not edit; run computed -->

- ADR-0001 (accepted, 2026-09-05): The unit format, including the opcode enum, is a leaf crate below both the compiler and the engine
- ADR-0002 (accepted, 2026-09-05): A native entry function takes the engine's concrete request type, not a trait object
- ADR-0003 (accepted, 2026-09-05): The syntax tree is typed nodes in a safe arena over a lossless token stream, not a green/red tree
- ADR-0004 (accepted, 2026-09-05): Extensions are one crate each under `ext/`, with `php_ext` as the facade below them and `php_bundle` as the enabled set above them
- ADR-0005 (accepted, 2026-09-05): Extension state is request-scoped or immutable engine-scoped, and the engine owns the only cross-request cache
- ADR-0006 (accepted, 2026-09-07): The compiled tier is a seam on `execute` with one engine-level word per safepoint, not a second executor with a per-request feedback table
- ADR-0007 (accepted, 2026-09-26): `php_macros` depends on no workspace crate, and `php_engine` depends on it, so Core declares its natives with the same attributes
- ADR-0008 (accepted, 2026-09-29): Hosts are one crate each under `sapi/`, with `php_sapi` as the facade below them, as ADR-0004 laid extensions out
- ADR-0009 (accepted, 2026-10-03): Branch tracks are removed; a track is a label on the tracker, worked from main
- ADR-0010 (accepted, 2026-10-04): An extension's lifecycle is three scopes that start as values and end as phases
- ADR-0011 (accepted, 2026-10-06): A reading's key covers its crate's whole dependency closure, not the locks of the crates it depends on
- ADR-0012 (accepted, 2026-10-07): The linker is a crate of its own, `php_link`, between the unit format and the crates that call it
- ADR-0013 (accepted, 2026-10-08): A foreign library's object that PHP code can free during a call is the request's, and the call holds it
- ADR-0014 (accepted, 2026-10-09): A request's heap is lent to C for the length of one call
- ADR-0015 (accepted, 2026-10-21): The compiled tier keeps php's function-level analysis, computed on a trace's first pass through an op array, in a crate of its own below the tier
- ADR-0016 (accepted, 2026-10-25): The binaries are named after the product from one constant, with `php` an opt-in symlink
- ADR-0017 (accepted, 2026-10-31): The compiled tier is refused as the default on G3: tracing stays opt-in, and Cranelift stands

<!-- /computed in=34988da3010119b7a6587d5a7defdc3917400f9f81928f7756a2e866bec24881 out=505c842c422e5cf0dbefccae4eb1489aed874733945edcc8cac26bedb9aee9d3 -->
<!-- computed exec cmd="cargo xtask status --section locks --format md --tracked" inputs=seams.toml,crates/**/*.lock,xtask/xtask.lock,xtask/src/status.rs name=seams | do not edit; run computed -->

| seam | lock | items | ceiling |
|---|---|---|---|
| heap | heap.lock | 568 |  |
| process | process.lock | 41 |  |
| unit | unit.lock | 185 |  |
| link | link.lock | 65 |  |
| ops | ops.lock | 209 |  |
| api | api.lock | 1550 | 1550 |
| sapi | sapi.lock | 60 |  |
| tier | tier.lock | 237 |  |
| ssa | ssa.lock | 114 |  |
| layer | layer.lock | 40 |  |
| harness | xtask.lock | 448 |  |

<!-- /computed in=dde92fc328f3e82d94518f83327f25ccf0eb78e09e3bf4f8f688821c8792ece4 out=62f8de45b5c51064dc3e3f44134af2c53d42b85726ec62baf5019ccefebe4002 -->
<!-- computed exec cmd="cargo xtask --help" inputs=xtask/src/cli.rs name=instruments as=fence | do not edit; run computed -->
```
The harness for the third engine

Usage: xtask [OPTIONS] <COMMAND>

Commands:
  api-check          Hold the seams: regenerate and diff each lock, hold crate edges, `use` paths and the bundle order; one reading per seam and per crate
  status             Render one section from the manifest, registry, store, repository and pinned tree; computes nothing else
  permit             Judge one intended call from the `PreToolUse` hook's stdin; the decision is the JSON on stdout and the exit is 0 always
  done               The definition of done: the conditions unmet since the base, and exit 1 while any is; the Stop hook, pre-push and CI read the list
  track              Say the branch, the commit it was taken off main at, the store and the branch's ticket; as text or as one object. Writes no reading
  worktree           A ticket's worktree: `add` lays one down at <checkout>-worktrees/<branch>, `retire` removes it, `own` and `whose` say its owner, `queue` and `land` run the merge queue
  ticket             A ticket, one GitHub issue: `read <n>` holds it to its acceptance criteria, `close <n>` closes it naming what it opened, `parent <n>` says whether a chart sheet is at the cap
  usage              What a session spent per turn: `record` reads the plugin's `SessionUsage` on stdin and writes one `usage` reading under the branch's ticket, or the session id
  regions            Render the named files' regions with `computed run` and say what it did, one line per file, for pre-commit to re-stage. Writes no reading
  bench              Measure one bar under the protocol: paired reps with the A/A control and the load average, one reading per bar, exit 0 always
  budget             Hold every crate to its line ceiling and the workspace to [budget]; refuse committed generated code, a stale generated file and a `build.rs` that bakes its crate's directory in
  clippy             `cargo clippy` with every warning denied, on every target of each crate and in release where it names `debug_assertions`, as the readings `done` reads; one per crate and profile
  test               `cargo test` on each crate, as the reading `done` reads; one reading per crate, with what passed, failed, was ignored and was filtered out in its numbers
  miri               `cargo miri test` on `php_heap`, or on the crates named, as the lane design §9 runs per commit; one reading per crate, with what ran, failed and was ignored in its numbers
  phpt               Run the pinned `.phpt` corpus through the engine and hold each test to its `--EXPECT*--` section; one reading per subset, scoped to the crate whose subset it is
  port-map           Join the Mahout census to `api.lock`: each item `same`, `renamed` or `gone`, the rows not yet taken as the number; one reading, at the lock's crate
  compile-identity   Compile a corpus under the oracle's dump recipe and under the engine, and hold every file to byte-identity; one reading per corpus, scoped to the corpus
  crosscheck         Hold each default extension's `--re` and `--ri` rendering to the oracle's bytes, kept in the store; one reading per extension per flag
  trace-dump         Hold the engine's `TRACE` lines under `jit.mode=force` to the oracle's under `[regime.jit-inline]` on the seven kernels, kept in the store; one reading per kernel
  fold               Hold the release `serve_frame` to LLVM's fold of a request field's address into each access; one reading, at `sapi/cli`
  fpm-compare        Hold makala to the pinned php-fpm: each request of the HTTP-served subset served by both, the status, header lines and body compared; one reading, at makala's crate
  soak               Many clients against one makala for a fixed time, each response held to the request served alone; one reading, at makala's crate
  unsafe-audit       Allow `unsafe` only in the homes the manifest's unsafe.homes rule names
  generate           Write the files the manifest's `generated` rules render (ext/clippy.toml)
  steer              Apply a proposal to one of the harness's own files, and only where it adds: one changed value writes nothing
  divergence-reason  Hold the divergence registry to the manifest rule whose `check` names this instrument: an accepted row has a reason a person wrote
  new-ext            Lay one extension crate down at ext/<name> from the template, with its `php_bundle` line, ext/clippy.toml and ext/AGENTS.md row; refuses an existing one
  new-sapi           Lay one host crate down at sapi/<name> as the CLI and CGI are laid, with its binary and test; refuses an existing one or one without its `[[crate]]` row (ADR-0008)
  witness            Run every `php` fence of a crate's rustdoc under the oracle's recipe and hold it to its `expect` fence; one reading per crate
  vocab              The glossary as a sensor: `_Avoid_` words in public item names and rustdoc prose, never in a lock; one reading per crate

Options:
      --root <DIR>  The workspace root; defaults to the workspace this binary runs in
  -h, --help        Print help
```
<!-- /computed in=8c6c6b426b7a04e344549b86e2d7ba12801dbe6c369ca2c0b743c98f1a90471c out=7d36360474ef686aa1dbccf693f6c58be3e09ee20203fbcf67b49f6bd0e36f2b -->
<!-- computed tree src=crates depth=1 name=layout | do not edit; run computed -->
```
.
├── clippy.toml
├── php_bundle
├── php_compile
├── php_engine
├── php_ext
├── php_heap
├── php_jit
├── php_link
├── php_macros
├── php_process
├── php_proto
├── php_sapi
├── php_ssa
├── php_streams
├── php_syntax
├── php_unit
└── timelib_sys
```
<!-- /computed in=9658ec9f1437b147db08254f39be027100720d383cb2755361228e4fae66d65a out=d92203a48d53e6c6b64fca99ec784fc36a1c0b46849b43e148ef89dc27b4ec4d -->
<!-- computed tree src=ext depth=1 name=layout-ext | do not edit; run computed -->
```
.
├── AGENTS.md
├── CLAUDE.md
├── bcmath
├── bz2
├── calendar
├── clippy.toml
├── ctype
├── curl
├── date
├── dba
├── dom
├── exif
├── ffi
├── fileinfo
├── filter
├── ftp
├── gd
├── gettext
├── gmp
├── hash
├── iconv
├── intl
├── json
├── lexbor
├── libxml
├── mbstring
├── mysqli
├── mysqlnd
├── opcache
├── openssl
├── pcntl
├── pcre
├── pdo
├── pdo_mysql
├── pdo_sqlite
├── phar
├── posix
├── random
├── readline
├── reflection
├── session
├── shmop
├── simplexml
├── soap
├── sockets
├── sodium
├── spl
├── sqlite3
├── standard
├── sysvmsg
├── sysvsem
├── sysvshm
├── tidy
├── tokenizer
├── uri
├── xml
├── xmlreader
├── xmlwriter
├── xsl
├── zend_test
├── zip
└── zlib
```
<!-- /computed in=432887617f7e74cad48d4fdfcd8b3967bc1dc8eb7f2c4040dc47e823e1c9f8cb out=bb0f5b5f2e5f8d121baf9c01693a1ec651329cdaf6e79a2ba394482425f4531b -->
<!-- computed tree src=sapi depth=1 name=layout-sapi | do not edit; run computed -->
```
.
├── cgi
├── cli
├── clippy.toml
├── fpm
├── makala
└── phpdbg
```
<!-- /computed in=cf19cb953ba961646d8d1eac4d4757e95e4fa1762a6cd992f3c664f2492477bc out=4554d325a05a615057f5f927a90ad2bc6dbd301ef02efcaff91e57313fa259ae -->
