# Contributing to Lela

Lela is a PHP 8.5 engine in Rust, held to the real `php` binary as its second opinion. Issues and pull requests are both welcome. This file says what a good report holds, and what a pull request has to meet before it lands.

A security issue is never a public issue or pull request: [SECURITY.md](SECURITY.md) says how to report one privately.

## Reporting a difference from PHP

Most bugs in Lela are a difference from PHP: the same script prints something else, or fails where PHP does not. A report that shows the difference is the one we can act on fastest. Please include:

- the smallest PHP script that shows it;
- what `php` prints, and its version (`php -v`);
- what `lela` prints, and its version (`lela -v`);
- the platform, and whether you ran a release tarball, an image (`ghcr.io/lelaphp/lela`) or a build of your own, at which commit.

Lela is tested against one pinned PHP release, the one `oracle.toml` names. A difference that shows only against another PHP version may be PHP's change between the two.

Some differences are deliberate, and each is listed with its reason in [docs/divergence.md](docs/divergence.md). If yours is there, the row says why; an issue that argues the reason is welcome too.

If what you found is a bug in PHP itself, it belongs to the PHP project: https://github.com/php/php-src/issues, or for a security issue, PHP's own private advisory form.

## Before a pull request

Open an issue first for anything larger than a small fix, so the change is agreed before you build it. Lela copies Zend's design and php's algorithms wherever the behaviour is observable (`third-engine-design.md` §3.1), so "how does php-src do it" is usually the first question a change answers.

Read [CONTEXT.md](CONTEXT.md) first. It is the glossary, and its `_Avoid_` lists bind in code, rustdoc and commit messages; `cargo xtask vocab` checks them. [AGENTS.md](AGENTS.md) is the working guide the project's agent sessions follow, and most of it applies to a person's change as well.

## Building and the harness

The [README](README.md#building) says how to build. The harness is the `xtask` crate: `cargo xtask --help` lists its instruments, and `harness-design.md` describes them.

The git hooks under `scripts/hooks/` run the instruments over what you stage and push. Point git at them once in your clone:

```sh
git config core.hooksPath scripts/hooks
```

Many instruments compare Lela with the pinned `php`, the oracle. On an Apple-silicon Mac, `scripts/ci/pinned-php.sh` pours it from Homebrew at the pinned version:

```sh
cargo xtask status --section oracle --format json > /tmp/oracle.json
scripts/ci/pinned-php.sh /tmp/oracle.json
```

Elsewhere, the instruments that need the oracle report it as absent and skip, and a maintainer runs them before your change lands.

## What a pull request meets

`cargo xtask done` is the definition of done. It lists what is unmet since your branch left `main`, with the command that clears each item, and exits 0 when nothing is. The pre-push hook runs it. In short:

- **Fresh readings for every crate you touched.** `cargo xtask test --crate <name>`, `cargo xtask clippy`, `cargo xtask budget` and the instruments `done` names. Every crate has a line ceiling, and a change that crosses it is a separate conversation.
- **A witness on every kernel and native you change.** A witness is a `php` fence in the rustdoc beside the code, with the oracle's output pinned in an `expect` fence after it. Write the script, then run `cargo xtask witness --crate <name> --accept`, which captures the oracle's output. The pinned output is never typed by hand; read the diff `--accept` writes and say in your commit that you did.
- **A lock that changes in the same diff.** Each interface between crates is a list of items in a `.lock` file. `cargo xtask api-check` regenerates them; commit the change beside the code that needs it.
- **No hand edit to a computed region.** A Markdown block marked `<!-- computed ... -->` is rendered from a command; refresh it with `computed run <file>`.

Some files are the maintainer's to change: `seams.toml` (the rules every instrument enforces), `oracle.toml` (the pinned PHP) and `docs/divergence.toml` (the accepted differences). If your change needs a new rule, a raised ceiling or a divergence row, propose it in the pull request description with its reason, and leave the file as it is. The same goes for the design documents, `third-engine-design.md`, `jit-design.md` and `harness-design.md`, and for the ADRs under `docs/adr/`: propose the revision, and a maintainer words and lands it.

CI runs on `main`, not on pull requests. A maintainer runs the harness over your branch before it lands, and what `done` lists then is what review asks for.

## Code

- Copy php's algorithm where its behaviour is observable: hash order, error text, float printing, sort order, memory and GC figures. Where it is not observable, the simplest correct code wins.
- Types over comments: newtypes for indices and handles, enums for state machines.
- No panic reaches a request. `unwrap` and `expect` are denied in the engine crates.
- `unsafe` is allowed only in the modules the `unsafe.homes` rule in `seams.toml` names, one operation per block, each with a `SAFETY` comment. `cargo xtask unsafe-audit` holds it.
- A new extension is laid down with `cargo xtask new-ext <name>`; read `ext/AGENTS.md` first.

## Commits

One concern per commit. The subject names what changed and says the change in one plain sentence, as the log does: `ext/soap witness: the header-fault cache digest starts after the file's time`. If the message needs the word "also", it is two commits. The lock line, the witness and the regenerated `ext/clippy.toml` travel in the commit with the code that needs them.

## Licence

Lela's own code is under the MIT License ([LICENSE](LICENSE)), and by opening a pull request you agree that your contribution is licensed under it too. Code derived from php-src or from another project keeps its notice: say in the pull request where it came from, and the entry in [THIRD-PARTY.md](THIRD-PARTY.md) and `LICENSES/` lands with it.
