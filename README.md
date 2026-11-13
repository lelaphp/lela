# dax-php

The third engine: a PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion.

`AGENTS.md` is the map an agent reads first, fed to it by the `SessionStart` hook as `xtask status --session` with the local facts of `.claude/harness.local.md` (harness §5); its regions are computed, `computed run` renders them and `computed check` holds them in CI. Read `CONTEXT.md` first. The design of record is `third-engine-design.md`; `jit-design.md` for the compiled tier; `harness-design.md` for the harness. Each is one living document with a ledger on top. Reversed decisions are in `docs/adr/`.

The harness is the `xtask` crate, `cargo xtask <instrument>`: `cargo xtask status --section rules|locks|budget|divergence` renders the manifest, the divergence registry and the readings under `target/harness/` (harness §4.6). `cargo xtask budget` holds each crate to its ceiling and the workspace to `[budget]`; `cargo xtask unsafe-audit` allows `unsafe` in the manifest's homes alone; `cargo xtask generate` writes `ext/clippy.toml` from the manifest, and a committed copy that differs fails `budget`; `cargo xtask api-check` holds the locks, the edges and the bundle order; `cargo xtask divergence-reason` holds the registry to its rule. The oracle module runs the pinned `php` and owns every comparison (harness §4.3). Each instrument's negative controls are under `xtask/fixtures/`.

Data files the harness reads, each stated once:

- `seams.toml`: the seam manifest (harness §4.1).
- `oracle.toml`: the pinned oracle, the only statement of its version, and its invocation recipe.
- `docs/divergence.toml`: the divergence registry, one row per accepted difference from the oracle; `docs/divergence.md` is its rendering, a computed region.
- `ops.toml`: the instruction set, extracted from `Zend/zend_vm_def.h` at the pinned tag by `scripts/extract_ops.py`.
- `crates/php_ext/api.lock`: the extension-facing interface; v0 is drafted from the Mahout census by `scripts/census.sh`.

The research behind the design, the earlier revisions (`*.rN.md`) and the predecessor data are in `research/`, kept unedited as records and cited by path from there.

Parts of this project are derived from php-src, Copyright (c) The PHP Group and Contributors and Zend Technologies Ltd., and used under the Modified BSD License. See NOTICE.
