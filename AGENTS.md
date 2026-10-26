# dax-php

dax-php is the third engine: a PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion. Its crates are in `crates/`, one per extension in `ext/`, one per host in `sapi/`, and `xtask`, the harness that measures all of it, counted below from the manifest. Think of it as opcache's memory model applied to threads instead of processes: immutable compiled units shared by every request in one process, with no fork and no recompiling.

<!-- computed exec cmd="cargo xtask status --section crates --tracked" inputs=seams.toml,xtask/src/status.rs,xtask/src/status/graph.rs name=crates | do not edit; run computed -->

| directory | crates | behind a feature |
|---|---|---|
| crates/ | 15 | 1 |
| ext/ | 59 | 31 |
| sapi/ | 9 | 0 |
| xtask | 1 | 0 |

<!-- /computed in=3dd572de7d51bc71bb1c233dec408bf25abfa21e0e1791160f873f76129646b1 out=b85e1b865d1c52266cf05808f0e042bf6dea128568977cc61be6b0830734c1ae -->

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

1. **Editing the harness's own files.** `seams.toml`, `oracle.toml` and `docs/divergence.toml` are a person's to decide, and `permit` denies you the edit. While nothing is settled, propose a rule or a divergence row in the commit message, never in the file. Once a person has settled the row on a ticket, land it with `cargo xtask steer <file> --from <proposal>`, which writes only where the proposal adds: one changed ceiling, one widened edge, one dropped rule and nothing is written, because a change to what already binds is a person's hand.
2. **Hand-editing a computed region.** Run `computed run <file>` to refresh it. `computed run --force` discards a hand edit, and a person decides that.
3. **Authoring a witness verdict.** `exit=` in a witness is captured by `xtask witness --accept`, never typed. Read the diff it produces and say in the commit that you did.
4. **Skipping the hook.** `git commit --no-verify` is not available to you.
5. **Running a nightly lane locally.** The full corpus, the compile identity, the differential fuzz and the bench run in CI only. Run the per-crate subset the instrument names.

## Before you call it done

`xtask done` (harness §4.5) prints what is unmet since the merge base with `main`; the Stop hook, pre-push and CI run the same list. Walk it before proposing a change and say which entries applied: fresh passing readings for every touched crate, a witness on every kernel and native, a lock that differs in the same diff, no open divergence on a changed scope, `permit` over the diff denying nothing, and the manifest and the registry untouched or changed by a person.

## Finish the whole task

A session here runs on its own: the turn ends at the Stop hook, not at a question, and nobody is waiting to type "go ahead".

You are operating autonomously. The user is not watching in real time and cannot answer questions mid-task, so asking 'Want me to…?' or 'Shall I…?' will block the work. For reversible actions that follow from the original request, proceed without asking. Stop only for destructive actions or genuine scope changes the user must decide. Offering follow-ups after the task is done is fine; asking permission before doing the work is not.

Exception: when the user is describing a problem, asking a question, or thinking out loud rather than requesting a change, the deliverable is your assessment. Report your findings and stop. Don't apply a fix until they ask for one.

Before ending your turn, check your last paragraph. If it is a plan, an analysis, a question, a list of next steps, or a promise about work you have not done ('I'll…', 'let me know when…'), do that work now with tool calls. That includes retrying after errors and gathering missing information yourself. Do not stop because the context or session is long. End your turn only when the task is complete or you are blocked on input only the user can provide.

Before running a command that changes system state (such as restarts, deletes, or config edits), check that the evidence actually supports that specific action. A signal that pattern-matches to a known failure may have a different cause.

What a session stops for here is listed, not judged in the moment: an edit `permit` denies, and an act that is a person's alone (harness §11) — a manifest row, a divergence reason, an oracle bump, the sentence a design revision states. Each of those is a `for-a-person` ticket, filed, with the work that waits on it wired to wait; asking in the reply instead ends the turn with nothing filed and nothing running.

The user's request — or the plan they approved — sets the scope, and the scope is the deliverable: don't quietly narrow, widen, or swap it. Read ambiguity the way a careful colleague would: make routine judgment calls yourself, and check in only when different readings would lead to materially different work. If you see a real problem with the task as specified, say so in a sentence or two and keep building under stated assumptions; if the user hears the concern and reaffirms, that is their decision, so deliver the full request.

If a question comes up partway, first do everything that doesn't depend on the answer; then state the assumption you made, or — when going ahead on a wrong guess would be unsafe or would make the work useless — put the question at the end of a turn that also delivers that progress. If one part turns out to be blocked, complete every other part in full and say exactly what you left out and why — the whole task is the deliverable, and scaling it down is the user's call, not yours. A step you have decided on is something to run, not to announce: describing the next step and ending the turn leaves it undone until the user replies.

Keep changes to what the request needs. Something else you notice worth doing — cleanup or documentation the task didn't call for, a change to a file the task didn't require — is a suggestion to make at the end, not a change to make; actions clearly beyond what the ask implies, and risky or destructive ones, still need the user's go-ahead.

Where those paragraphs say *suggestion* or *follow-up*, this repository says ticket. A thing found and not done is a ticket, filed before the turn ends: `ready-for-agent` when an agent may take it, `for-a-person` when the act is a person's alone (harness §11); the reply that names it and files nothing is what `ticket.reply-unfiled` holds you to (CONTEXT.md, Ticket). Filing one is the exception and never the shape of the work — the task is finished whole, and what a ticket carries is what the task did not ask for.

## Verifying and committing

- `cargo xtask <instrument>` runs one instrument; `cargo xtask --help` lists the ones built so far. The smallest proof is the instrument for the crate you touched. The harness itself is tested by `cargo test -p xtask`, and every manifest rule has a fixture under `xtask/fixtures/` that must fail.
- A change to behaviour comes with a witness beside the kernel or native, pinned by `--accept`. The witness is the test; write it before the body when you can. The phpt subset for an extension is every `.phpt` under `php-src/ext/<name>`, as run-tests.php walks the directory. A bench number is a claim a person signs off, with the A/A control read by a person: state the measurement, never the conclusion.
- Stage and commit by pathspec, `git add -- <paths>` and `git commit -F <file> -- <paths>`: the index is shared with every session in the checkout, and `git add -A`, `git add .` or a commit that names no path takes what another session staged. The shell is zsh, which does not split an unquoted `$P`: write the paths out, or use an array, `P=(a b)` and `git add -- "${P[@]}"`. A merge, cherry-pick, revert or rebase in progress commits the whole index, whoever staged it; where one stands that you did not start, `permit` refuses the git commands that write its index or step it, so wait for it to end or work in your own worktree. The message file is named for the branch or its ticket, never `msg.txt`: the scratchpad is shared across the checkout's worktrees, so a generic name is whichever session wrote it last. Commits are plain language, one concern, the change in one sentence: `xtask vocab: the glossary as a sensor`. If the message says "also", split it. The lock line, the witness and the regenerated `ext/clippy.toml` travel with the code that needs them. A rule you want, or a divergence you found, is a paragraph in the message proposing it, with the fixture that would fail. What the work opened is the close's `--opens`, recorded as a reading, not a trailer in the message. A ticket's acceptance criteria are boxes its own label can tick. Where the work waits on a decision, the decision is a `for-a-person` ticket of its own and the work is wired to wait on it; a box in an agent's ticket that reads *a person decides* closes nothing however finished the code is.

## Documentation and where code lives

- `third-engine-design.md` is the design of record, `jit-design.md` the compiled tier, `harness-design.md` the harness: living documents with a ledger on top, cited by section. What a revision says is a person's; typing it is not. Where a ticket carries the sentence a person has settled, landing it with its ledger row is yours, and the commit names the ticket that settled it. A revision that states what a ruled ticket built, and reverses, widens or adds to no ruling, is yours to word and land, with its ledger row naming the ruling and the ticket; the person reads that row at the merge, and a sentence they revert becomes a ticket of theirs. A deletion-test argument, an ADR, and a sentence answering a question the design is silent on stay a person's, and any other revision nobody has settled stays a question you file. `research/` holds the earlier revisions and the research behind them, read for history only. `docs/adr/` is the list of reversals; read the ADR before touching what it names.
- Design documents carry no numbers of their own; budgets and interfaces live in `seams.toml`, the oracle in `oracle.toml`. A fact that changes is a region, rendered by `computed run` and held by `computed check`. No tickets, plans or agent scratch files in the repository; keep working material in your scratchpad.
- `crates/php_syntax`, `php_compile` and `php_unit` take source to a unit; `php_heap` is the memory model and the stacks, and `php_process` the process and its file descriptors (`fork`, the signal pool, `LC_CTYPE`, the process identity, the fd calls), each an `unsafe` home the `unsafe.homes` rule names; `php_engine` holds the kernels, dispatch, `execute`, the linker, `serve` and `api`, with `php_jit` the compiled tier behind a feature; `php_ext` is the facade every extension depends on and `php_bundle` the enabled set; `php_proto` is the codecs and `php_sapi` the facade every host depends on.
- `ext/<name>` is one crate per extension, laid down by `cargo xtask new-ext`; read `ext/AGENTS.md` before touching one. `sapi/<name>` is one crate per host, each its binary (`sapi/cli` builds `php`, `sapi/cgi` `php-cgi`; ADR-0008). `xtask` is the harness and `xtask/fixtures/` its negative controls. `data/` holds the censuses and extraction records the design cites. Most of `crates/` is not created yet; the layout regions below say what exists.

## Taste

- Types over comments: newtypes for indices and handles, enums for state machines, `&mut Request` on anything that can run PHP code. No panic reaches a request; `unwrap` and `expect` are denied in engine crates. `unsafe` lives in the homes the `unsafe.homes` rule names, one operation per block, each with a `SAFETY` comment. `pub` crosses a crate boundary only through a locked module. Generated code is generated in `build.rs` and never committed; the one exception, `ext/clippy.toml`, is regenerated and diffed.
- Copy php's algorithm where the behaviour is observable; drop the cleverness where it is not. The floor is php's interpreter by construction; the ceiling comes from threads and whole-program LTO, not a smarter IR. If a rule here fights the task in front of you, say so loudly and get a person's sign-off before breaking it.

## The facts, computed

The regions below are rendered by `computed run AGENTS.md` and held by `computed check` in CI. The volatile readings (budget, phpt, witness, mutants, port-map, divergence, permits, git) live in `.claude/harness.local.md`, which the SessionStart hook renders and prints after this file. Every manifest rule, with its ADR and predicate: `cargo xtask status --section rules`.

<!-- computed exec cmd="ls third-engine-design*.md jit-design*.md harness-design.md | sort -V" inputs=third-engine-design*.md,jit-design*.md,harness-design.md name=design-of-record | do not edit; run computed -->

harness-design.md
jit-design.md
third-engine-design.md

<!-- /computed in=a69eb9110303f5413bebd426e688f66a1a16c421e5bedcce6603b8e9bba2e8e0 out=59c14cd45b0ce3a636958ac2b89a22d1b68161882044aa0617b42f93947e231e -->
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

<!-- /computed in=451017e574bbe4d4095567f537acfa70b308969a70c37f804a528c1b04adaf30 out=059605fab045f8a0e0476a5fbe57a17446b44e77150f840f41eef3b197833fa8 -->
<!-- computed exec cmd="cargo xtask status --section locks --format md --tracked" inputs=seams.toml,crates/**/*.lock,xtask/xtask.lock,xtask/src/status.rs name=seams | do not edit; run computed -->

| seam | lock | items | ceiling |
|---|---|---|---|
| heap | heap.lock | 514 |  |
| process | process.lock | 40 |  |
| unit | unit.lock | 125 |  |
| link | link.lock | 62 |  |
| ops | ops.lock | 209 |  |
| api | api.lock | 1520 | 1525 |
| sapi | sapi.lock | 58 |  |
| tier | tier.lock | 199 |  |
| layer | layer.lock | 38 |  |
| harness | xtask.lock | 428 |  |

<!-- /computed in=b141b920983ddecf1a9d3c11e108f9771d96c73162b5c62a1fb6a7fa294ac64f out=f4128780893b17a262a460863bd65f55c4a0e730cfffe6d4bf996f940eb40d36 -->
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
  worktree           A ticket's worktree: `add` lays one down at <checkout>-worktrees/<branch>, `retire` appends its log into the store and removes it. Writes no reading
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
<!-- /computed in=12c76363ab0e78124f0eec51c5079400f5df6f6e8eb2ff5e5f4477f19b7f3828 out=f75906f17b5734883eb8cab7c977ae1cd12d0f5abd2810e819387650464a5b08 -->
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
├── php_streams
├── php_syntax
├── php_unit
└── timelib_sys
```
<!-- /computed in=70b3b7eca9690068316f014f00bdd615bc6f89449cdfd3567d5ee9f795b18194 out=b73614d2eac7f713540dbdf7f6b1b890c6288f77445621d7a7118414d6885560 -->
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
