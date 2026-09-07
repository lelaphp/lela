# The map

1. Read `CONTEXT.md` first; its `_Avoid_` lists are binding in code, rustdoc and commit messages.
2. The design of record is `third-engine-design.md`, `jit-design.md` for the compiled tier, `harness-design.md` for the harness; the `rN` files under `research/` are records and are not read for anything current.
3. `docs/adr/` is the list of reversals; read the ADR before touching what it names.
4. Done (`xtask done`, harness §4.5): every touched crate has a fresh passing reading for clippy, witness, api-check, budget and unsafe-audit, and for phpt on its subset; every kernel or native added has a witness; a lock that differs is in the diff; no open divergence touches a changed scope; `permit` over the diff denies nothing; `seams.toml` and `docs/divergence.toml` are unchanged, or changed by a person.
5. Not alone (harness §11): the oracle pin, every write to `seams.toml` and `docs/divergence.toml`, ADRs and design revisions, accepting a witness diff, a perf claim, licences, `computed trust` and a forced `computed run`. Propose a rule or a divergence row in the commit message, never in the file.

<!-- computed exec cmd="ls third-engine-design*.md jit-design*.md harness-design.md | sort -V" inputs=third-engine-design*.md,jit-design*.md,harness-design.md name=design-of-record | do not edit; run computed -->

harness-design.md
jit-design.md
third-engine-design.md

<!-- /computed in=338af59f5674af125bfc31b8f77f11f179360718e215055984ef085ba0de9e13 out=59c14cd45b0ce3a636958ac2b89a22d1b68161882044aa0617b42f93947e231e -->
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
  budget             Hold every crate to its line ceiling and the workspace to [budget]; refuse committed generated code and a stale generated file
  unsafe-audit       Allow `unsafe` only in the homes the manifest's unsafe.homes rule names
  generate           Write the files the manifest's `generated` rules render (ext/clippy.toml)
  divergence-reason  Hold the divergence registry to the manifest rule whose `check` names this instrument: an accepted row has a reason a person wrote
  vocab              The glossary as a sensor: `_Avoid_` words in public item names, rustdoc prose and the lock; one reading per crate

Options:
      --root <DIR>  The workspace root; defaults to the workspace this binary was built in
  -h, --help        Print help
```
<!-- /computed in=2677f371dd837f829a0449dab89c091199f5788fe49366f84c6175a744a1cb24 out=3ee58c7d343ddf77c4b57c8237d9b40bfac398271f0a9e44eeb8eedc738eb34d -->
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
