# Lela

The third engine: a PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion.

## Building

The toolchain is the Rust release `rust-toolchain.toml` pins, with rustfmt and clippy. rustup reads that file in the checkout and installs the release on first use unless its auto-install mode is off; `rustup show active-toolchain` names the one in use. The workspace and its profiles are the root `Cargo.toml`: `release` builds the binaries, and `release-da`, release with debug assertions and overflow checks, is the build the harness's correctness lanes run (harness §7). `.cargo/config.toml` holds what every build in the workspace is given: an LLVM option the engine's `build.rs` refuses a release build without, and the variables that bind the system libraries.

```sh
cargo build --release
```

builds every crate of the workspace. Each host under `sapi/` is one crate (ADR-0008), and the `[[bin]]` tables of its `Cargo.toml` name its binaries (ADR-0016). The build writes them to `target/release/`, all but a binary whose table has a `required-features` line, which is built when `--features` names the feature. `cargo build --release -p php_sapi_cli` builds the command-line host alone, and its binary, `target/release/lela`, run with `-v`, prints php's version line.

The C the build needs is a `build.rs`'s to compile or to find. A C library that is vendored is php-src's copy, byte for byte at the tag `oracle.toml` pins, beside the `build.rs` that compiles it. Any other is the system's, found by the `build.rs` of the crate that binds it or of the `-sys` crate it uses, most through `pkg-config`. On macOS some are bound to the ones the oracle loads, the kegs `oracle.toml`'s `[closure]` names: `.cargo/config.toml` points at their directories and says which it links statically, and `ext/intl/build.rs` refuses an ICU whose major version is not the pin's, finding it through `PKG_CONFIG_PATH`.

A component under the LGPL-2.1 is source in this tree and builds with the rest, and its crate's `license` field names the licence:

```sh
grep -H '^license = .*LGPL' crates/*/Cargo.toml ext/*/Cargo.toml sapi/*/Cargo.toml
```

After its source changes, `cargo build --release` rebuilds every binary that links it.

The oracle's pin is `oracle.toml`, and `cargo xtask status --section oracle` prints the pin as this machine has it. The harness is described in `harness-design.md`, and `cargo xtask --help` lists its instruments. Building the binaries runs neither the oracle nor the harness.

## The images

CI publishes three variants to `ghcr.io/lelaphp/lela`, each shaped as php's official image of that name is, and each for `linux/amd64` and `linux/arm64` under one reference (`dist/README.md`). Every reference names its variant. `<version>` is a release's version, and its tags never move; the bare variant is the newest release that is not a pre-release; `nightly-` is the last merge to `main`, and moves on every merge, not once a night, since the word names the channel and not a schedule; and a `sha-` tag, with the full commit, is one merge to `main`, kept 30 days.

`cli`, the CLI and CGI hosts, running `lela -a`:

```
ghcr.io/lelaphp/lela:cli
```

```
ghcr.io/lelaphp/lela:<version>-cli
```

```
ghcr.io/lelaphp/lela:nightly-cli
```

```
ghcr.io/lelaphp/lela:sha-<commit>-cli
```

`fpm`, those and the FPM, listening on port 9000:

```
ghcr.io/lelaphp/lela:fpm
```

```
ghcr.io/lelaphp/lela:<version>-fpm
```

```
ghcr.io/lelaphp/lela:nightly-fpm
```

```
ghcr.io/lelaphp/lela:sha-<commit>-fpm
```

`apache`, those and Apache in front of the FPM, serving port 80:

```
ghcr.io/lelaphp/lela:apache
```

```
ghcr.io/lelaphp/lela:<version>-apache
```

```
ghcr.io/lelaphp/lela:nightly-apache
```

```
ghcr.io/lelaphp/lela:sha-<commit>-apache
```

The hosts carry Lela's names (ADR-0016), `lela`, `lela-cgi` and `lela-fpm`, and no `php` is on an image's `PATH`. The names other tools call, `php`, `php-cgi` and `php-fpm`, are symlinks to the hosts in `/opt/lela/compat`, which no image puts on `PATH`. One line in a Dockerfile built on an image opts in:

```dockerfile
ENV PATH=/opt/lela/compat:$PATH
```

An image's `org.opencontainers.image.licenses` label is the union of the `license` lines of this workspace's crates the hosts link, which `dist/publish.py` reads from their manifests (`dist/README.md`). The licences of the third-party crates and the system's libraries in an image are in the SBOM it carries.

## The repository

`AGENTS.md` is the map an agent reads first, fed to it by the `SessionStart` hook as `xtask status --session` with the local facts of `.claude/harness.local.md` (harness §5); its regions are computed, `computed run` renders them and `computed check` holds them in CI. Read `CONTEXT.md` first. The design of record is `third-engine-design.md`; `jit-design.md` for the compiled tier; `harness-design.md` for the harness. Each is one living document with a ledger on top. Reversed decisions are in `docs/adr/`.

The harness is the `xtask` crate, `cargo xtask <instrument>`: `cargo xtask status --section rules|locks|budget|divergence` renders the manifest, the divergence registry and the readings under `target/harness/` (harness §4.6). `cargo xtask budget` holds each crate to its ceiling and the workspace to `[budget]`; `cargo xtask unsafe-audit` allows `unsafe` in the manifest's homes alone; `cargo xtask generate` writes `ext/clippy.toml` from the manifest, and a committed copy that differs fails `budget`; `cargo xtask api-check` holds the locks, the edges and the bundle order; `cargo xtask divergence-reason` holds the registry to its rule. The oracle module runs the pinned `php` and owns every comparison (harness §4.3). Each instrument's negative controls are under `xtask/fixtures/`.

Data files the harness reads, each stated once:

- `seams.toml`: the seam manifest (harness §4.1).
- `oracle.toml`: the pinned oracle, the only statement of its version, and its invocation recipe.
- `docs/divergence.toml`: the divergence registry, one row per accepted difference from the oracle; `docs/divergence.md` is its rendering, a computed region.
- `ops.toml`: the instruction set, extracted from `Zend/zend_vm_def.h` at the pinned tag by `scripts/extract_ops.py`.
- `crates/php_ext/api.lock`: the extension-facing interface; v0 is drafted from the Mahout census by `scripts/census.sh`.

The research behind the design, the earlier revisions (`*.rN.md`) and the predecessor data are in `research/`, kept unedited as records and cited by path from there.

This project's own code is licensed under the MIT License; see LICENSE. Parts of this project are derived from php-src, Copyright (c) The PHP Group and Contributors and Zend Technologies Ltd., and used under the Modified BSD License. See NOTICE.
