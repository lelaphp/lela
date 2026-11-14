---
status: accepted
date: 2026-10-25
supersedes: docs/adr/0008 decision "Each owns its binary (`php`, `php-cgi`, `php-fpm`, `phpdbg`, …)" and "the binary keeps its name"
---

# The binaries are named after the product from one constant, with `php` an opt-in symlink

ADR-0008 gave each host its own binary and kept php-src's names for them: `php`, `php-cgi`, `php-fpm` and `phpdbg`. Those names put the PHP mark on the product itself. The legal findings of 24 Oct (the launch repository, `legal/findings-2026-10.md`, B2) read that as the use with the weakest cover under *Audi v GQ* (C-334/22). Mitchell settled it on #3404 on 2026-10-25: the binaries carry the product's name, and `php` stays available to the scripts that call it. The name itself is #3404's to decide, and this record does not decide it. Mitchell ruled the form and scope on #3465.

## Decision

- The binaries are `<name>` (`sapi/cli`), `<name>-cgi`, `<name>-fpm`, `<name>-dbg`, and `<name>-linked` (the CLI's `linked` build). `<name>` is one constant, the product name.
- `makala` keeps its own name. It is not a php-src SAPI (ADR-0008, #2332), so its name does not follow the product's.
- The constant lives in `php_sapi`, the facade every host depends on, and `sapi.lock` lists it. Every site that can read a Rust constant reads this one.
- Some sites cannot read a constant. A `[[bin]] name` and an `env!("CARGO_BIN_EXE_…")` are literals. xtask links no workspace crate but its lanes, which link none, so `xtask/src/oracle/hosts.rs` keeps a copy. A checked rule holds those literals and that copy equal to the constant, so a rename that misses one fails instead of shipping.
- `php` is an opt-in symlink to `<name>`, never installed by default. How a release offers it is #3464's to decide.
- A binary's name is not what it prints. The SAPI names (`php_sapi_name()`), `PHP_VERSION`, `zend_version()` and the shape of the first `-v` line follow php, and the identity lines are #3464's. Where php builds a text from `argv[0]`, the engine does too. The CLI's usage line names the program as it was invoked, as `php_cli_usage` does (php-src `sapi/cli/php_cli.c:470-474`), so a call through the symlink reads `php`.
- Nothing is renamed until #3404 names the product.

## Considered options

- **Keep `php` as the binary's name.** Rejected on #3404, because it puts the mark on the product itself.
- **Rename and offer no `php` link.** Rejected on #3404, because `#!/usr/bin/env php`, and every script and tool that runs `php`, would break by default.
- **One multi-call binary that dispatches on `argv[0]`, as busybox does.** Rejected for ADR-0008's reasons. Each host chooses its own bundle features (pcntl and posix only in the CLI), and php-src builds one binary per SAPI. A single binary would carry every host's extensions and every host's `unsafe` homes.
- **A name per binary, with no shared constant.** Rejected. A rename away from the product's name would take five edits, and #3404 ruled that a rename of the product is not a second rename of the binaries.
- **Generate the Cargo names from the constant.** Not available, because Cargo reads `[[bin]] name` before any build script runs. The checked rule is what is left.
- **An amendment section in ADR-0008.** Rejected. In this repository a reversal is an ADR of its own, and `scripts/adr-index.sh` lists ADRs, not sections.

## Consequences

- `seams.toml` changes with this ADR, and a person makes those edits:
  - The checked rule holds the six `[[bin]]` literals, the 19 `CARGO_BIN_EXE_*` uses in 17 files under `sapi/*/tests`, and `xtask/src/oracle/hosts.rs` to the constant.
  - `lanes.nightly`'s `deny_pattern` names `<name>` beside `php`. Today it matches only a program named `php`, so `<name> run-tests.php` would get past it.
- `xtask/src/permit/writes.rs` lists `php` among the interpreters, and `<name>` joins it there.
- The harness finds a sibling host beside the CLI by its name (`EngineHost::for_run`). It answers a missing sibling with a skip, as run-tests.php does, so a wrong name would turn the cgi, fpm and phpdbg subsets into skips, not failures. The checked rule keeps that from passing unnoticed.
- The harness tells the oracle from the engine by its pinned path and hash, never by name. A `php` link on `PATH` therefore cannot stand in for the oracle. It can still shadow Homebrew's `php` for a bare invocation, which is why the link is opt-in.
- The CLI's usage line stops hard-coding `php` (`sapi/cli/src/options.rs:592`) and names the program as it was invoked, as php does.
- What `PHP_BINARY` reports through the symlink on each platform is not measured here. #3464 pins it with a witness.
