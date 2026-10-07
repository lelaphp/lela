# dax-php

dax-php is the third engine: a PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion. Twelve crates in `crates/`, one crate per extension in `ext/`, one per host in `sapi/`, and `xtask`, the harness that measures all of it. Think of it as opcache's memory model applied to threads instead of processes: immutable compiled units shared by every request in one process, with no fork and no recompiling.

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
- Stage and commit by pathspec, `git add -- <paths>` and `git commit -F <file> -- <paths>`: the index is shared with every session in the checkout, and `git add -A`, `git add .` or a commit that names no path takes what another session staged. The message file is named for the branch or its ticket and never `msg.txt`, for the same reason the staging is by pathspec: the scratchpad is one directory per project, shared across the checkout's worktrees, so a generic name is whichever session wrote it last. Commits are plain language, one concern, the change in one sentence: `xtask vocab: the glossary as a sensor`. If the message says "also", split it. The lock line, the witness and the regenerated `ext/clippy.toml` travel with the code that needs them. A rule you want, or a divergence you found, is a paragraph in the message proposing it, with the fixture that would fail. What the work opened is the close's `--opens`, recorded as a reading, not a trailer in the message. A ticket's acceptance criteria are boxes its own label can tick. Where the work waits on a decision, the decision is a `for-a-person` ticket of its own and the work is wired to wait on it; a box inside an agent's ticket that reads *a person decides* is one box that is both, and it closes nothing however finished the code is.

## Documentation and where code lives

- `third-engine-design.md` is the design of record, `jit-design.md` the compiled tier, `harness-design.md` the harness: living documents with a ledger on top, cited by section. What a revision says is a person's; typing it is not. Where a ticket carries the sentence a person has settled, landing it with its ledger row is yours, and the commit names the ticket that settled it. A revision nobody has settled stays a question you file. `research/` holds the earlier revisions and the research behind them, read for history only. `docs/adr/` is the list of reversals; read the ADR before touching what it names.
- Design documents carry no numbers of their own; budgets and interfaces live in `seams.toml`, the oracle in `oracle.toml`. A fact that changes is a region, rendered by `computed run` and held by `computed check`. No tickets, plans or agent scratch files in the repository; keep working material in your scratchpad.
- `crates/php_syntax`, `php_compile` and `php_unit` take source to a unit; `php_heap` is the memory model and `php_process` the process-wide mechanisms (`fork`, the signal pool, `LC_CTYPE`), the engine's only `unsafe`; `php_engine` holds the kernels, dispatch, `execute`, the linker, `serve` and `api`, with `php_jit` the compiled tier behind a feature; `php_ext` is the facade every extension depends on and `php_bundle` the enabled set; `php_proto` is the codecs and `php_sapi` the facade every host depends on.
- `ext/<name>` is one crate per extension, laid down by `cargo xtask new-ext`; read `ext/AGENTS.md` before touching one. `sapi/<name>` is one crate per host, each its binary (`sapi/cli` builds `php`, `sapi/cgi` `php-cgi`; ADR-0008). `xtask` is the harness and `xtask/fixtures/` its negative controls. `data/` holds the censuses and extraction records the design cites. Most of `crates/` is not created yet; the layout regions below say what exists.

## Taste

- Types over comments: newtypes for indices and handles, enums for state machines, `&mut Request` on anything that can run PHP code. No panic reaches a request; `unwrap` and `expect` are denied in engine crates. `unsafe` lives in seven homes, one operation per block, each with a `SAFETY` comment. `pub` crosses a crate boundary only through a locked module. Generated code is generated in `build.rs` and never committed; the one exception, `ext/clippy.toml`, is regenerated and diffed.
- Copy php's algorithm where the behaviour is observable; drop the cleverness where it is not. The floor is php's interpreter by construction; the ceiling comes from threads and whole-program LTO, not a smarter IR. If a rule here fights the task in front of you, say so loudly and get a person's sign-off before breaking it.

## The facts, computed

The regions below are rendered by `computed run AGENTS.md` and held by `computed check` in CI. The volatile readings (budget, phpt, witness, mutants, port-map, divergence, permits, git) live in `.claude/harness.local.md`, which the SessionStart hook renders and prints after this file.

<!-- computed exec cmd="ls third-engine-design*.md jit-design*.md harness-design.md | sort -V" inputs=third-engine-design*.md,jit-design*.md,harness-design.md name=design-of-record | do not edit; run computed -->

harness-design.md
jit-design.md
third-engine-design.md

<!-- /computed in=1f2360d4c0fd0843fb094271d497ca6f093bdb8e16198160705440b60e116641 out=59c14cd45b0ce3a636958ac2b89a22d1b68161882044aa0617b42f93947e231e -->
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

<!-- /computed in=5529edd9c45b6b0b1813b2ed64be47761bec3e13fe0055e19aa53743db32499a out=a717c7dcef6bb51d05f58dcc2aaebb45f624853d7634b25efa867cd7428ec528 -->
<!-- computed exec cmd="cargo xtask status --section locks --format md" inputs=seams.toml,crates/**/*.lock name=seams | do not edit; run computed -->

| seam | lock | items | ceiling | reading |
|---|---|---|---|---|
| heap | heap.lock | 390 |  | pass |
| process | process.lock | 17 |  | pass |
| unit | unit.lock | 122 |  | pass |
| ops | ops.lock | 209 |  | pass |
| api | api.lock | 1318 | 1500 | pass |
| sapi | sapi.lock | 35 |  | pass |
| tier | tier.lock | 67 |  | pass |
| harness | xtask.lock | 372 |  | pass |

<!-- /computed in=145e0c22435afad2fc4b44b3d84fd426e836077fd8648e84931deb3f6e393c64 out=788c2fa22252e19efff3938eefd6d6d2b003228fa70406452177ae68f88552e2 -->
<!-- computed exec cmd="cargo xtask status --section rules --format md" inputs=seams.toml name=rules | do not edit; run computed -->

| rule | adr | predicate | reading |
|---|---|---|---|
| ext.no-locks | ADR-0005 | deny_types | no reading |
| unsafe.homes | ADR-0006 | allow | pass |
| unsafe.one-op |  | deny_lints | no reading |
| fork.home |  | deny_pattern | no reading |
| lints.no-panic |  | deny_lints | no reading |
| engine.reentry-signature |  | check | no reading |
| api.no-hot |  | deny_pattern | no reading |
| ext.no-private |  | deny_pattern | no reading |
| api.lock-line |  | on | pass |
| xtask.lock-line |  | on | pass |
| ext.pub-beyond-extension |  | on | pass |
| witness.exit-authored |  | deny_pattern | no reading |
| witness.coverage |  | check | pass |
| vocab.avoid |  | avoid_from | pass |
| vocab.harness |  | avoid_from | pass |
| generated.committed |  | generated | pass |
| git.no-verify |  | deny_argv | fail |
| ticket.close |  | deny_argv | fail |
| ticket.close-api |  | deny_pattern | fail |
| ticket.reply-unfiled |  | deny_pattern | no reading |
| lanes.nightly |  | deny_pattern | fail |
| lanes.nightly-flags |  | deny_argv | no reading |
| lanes.builder-subset |  | require_argv | fail |
| computed.force |  | deny_pattern | no reading |
| harness.self |  | deny | fail |
| divergence.reason |  | check | no reading |
| owner.no-assign-discard |  | deny_pattern | no reading |

<!-- /computed in=264816a7ea9a5cb0964a15c52ba6ac697c4093df2a81319ede7106ae4f27d6c0 out=81c5fd38e09d43769bfaa6193fe7a3818a0a3c60900153d819318d78cf805a3e -->
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
  budget             Hold every crate to its line ceiling and the workspace to [budget]; refuse committed generated code and a stale generated file
  clippy             `cargo clippy` with every warning denied, on every target of each crate, as the reading `done` reads; one reading per crate
  test               `cargo test` on each crate, as the reading `done` reads; one reading per crate, with what passed, failed, was ignored and was filtered out in its numbers
  phpt               Run the pinned `.phpt` corpus through the engine and hold each test to its `--EXPECT*--` section; one reading per subset, scoped to the crate whose subset it is
  port-map           Join the Mahout census to `api.lock`: each item `same`, `renamed` or `gone`, the rows not yet taken as the number; one reading, at the lock's crate
  compile-identity   Compile a corpus under the oracle's dump recipe and under the engine, and hold every file to byte-identity; one reading per corpus, scoped to the corpus
  crosscheck         Hold each default extension's `--re` and `--ri` rendering to the oracle's bytes, kept in the store; one reading per extension per flag
  unsafe-audit       Allow `unsafe` only in the homes the manifest's unsafe.homes rule names
  generate           Write the files the manifest's `generated` rules render (ext/clippy.toml)
  steer              Apply a proposal to one of the harness's own files, and only where it adds: one changed value writes nothing
  divergence-reason  Hold the divergence registry to the manifest rule whose `check` names this instrument: an accepted row has a reason a person wrote
  new-ext            Lay one extension crate down at ext/<name> from the template, with its `php_bundle` line, ext/clippy.toml and ext/AGENTS.md row; refuses an existing one
  new-sapi           Lay one host crate down at sapi/<name> as the CLI and CGI are laid, with its binary and test; refuses an existing one or one without its `[[crate]]` row (ADR-0008)
  witness            Run every `php` fence of a crate's rustdoc under the oracle's recipe and hold it to its `expect` fence; one reading per crate
  vocab              The glossary as a sensor: `_Avoid_` words in public item names and rustdoc prose, never in a lock; one reading per crate

Options:
      --root <DIR>  The workspace root; defaults to the workspace this binary was built in
  -h, --help        Print help
```
<!-- /computed in=713cd9235eeb66ff8a3561a3522bec71eb5bda1e01d7bcb1c5d5af351db14bde out=e664430ce96b37a74f9976fb01ecb603c92aea0cffab0212baffcd7705d0b565 -->
<!-- computed tree src=crates depth=1 name=layout | do not edit; run computed -->
```
.
├── php_bundle
├── php_compile
├── php_engine
├── php_ext
├── php_heap
├── php_macros
├── php_process
├── php_proto
├── php_sapi
├── php_streams
├── php_syntax
└── php_unit
```
<!-- /computed in=f0c7b431c0b09dd1f77217aa12a1df01ec11c8acda891c0c220c5b01f645a614 out=41028121a5904b9375f2470827c829ad35eb7f8905d93b9e3304c072dee99ea0 -->
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
├── fpm
└── phpdbg
```
<!-- /computed in=d9792c70e043b4549f1ed04bf5ddbc594d3104b27bce7a39b12cc5c2dc6ef79d out=470dec71ee5cf90528ce6b4eef3301fa86defac22de9048c8274b9ca2838028e -->
