# dax-php

The third engine: a PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion.

Read `CONTEXT.md` first. The design of record is `third-engine-design.md`; `jit-design.md` for the compiled tier; `harness-design.md` for the harness. Each is one living document with a ledger on top. Reversed decisions are in `docs/adr/`.

The harness is the `xtask` crate: `cargo run -p xtask -- status --section rules|locks|budget` renders the manifest and the readings under `target/harness/` (harness §4.6).

Data files the harness reads, each stated once:

- `seams.toml`: the seam manifest (harness §4.1).
- `oracle.toml`: the pinned oracle, php 8.5.9, and its invocation recipe.
- `ops.toml`: the instruction set, extracted from `Zend/zend_vm_def.h` at the pinned tag by `scripts/extract_ops.py`.
- `crates/php_ext/api.lock`: the extension-facing interface; v0 is drafted from the Mahout census by `scripts/census.sh`.

The research behind the design, the earlier revisions (`*.rN.md`) and the predecessor data are in `research/`, kept unedited as records and cited by path from there.
