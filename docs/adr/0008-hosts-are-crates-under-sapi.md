---
status: accepted
date: 2026-09-29
supersedes: third-engine-design.md §4.1 (`php_sapi` as the one crate of hosts; `php` as the one binary)
---

# Hosts are one crate each under `sapi/`, with `php_sapi` as the facade below them, as ADR-0004 laid extensions out

Full parity means every SAPI is supported (Mitchell, #1164, #1165), and php-src's SAPIs measure, in non-blank, non-comment C lines at php-8.5.9: cli 6,641 with the `-S` server, cgi 2,118, fpm 11,284, phpdbg 10,795, litespeed 6,088, apache2handler 1,313, fuzzer 921, embed 210. Revision 1 put every host in `php_sapi` under one ceiling of 5,000 lines and made `php` the one binary. That shape cannot hold fpm and phpdbg under an honest ceiling, gives fpm's process manager (fork, signals, a shared scoreboard) and phpdbg's terminal one `unsafe` home between them, builds one binary where php-src builds one per SAPI with its own extensions, and serialises every host's work on one crate. Settled by Mitchell on #1248.

## Decision

- One crate per SAPI under `sapi/`: `sapi/cli` (with the `-S` server, as php-src keeps it in `sapi/cli`), `sapi/cgi`, `sapi/fpm`, `sapi/phpdbg`, `sapi/embed`, `sapi/apache2handler`, `sapi/litespeed`, `sapi/fuzzer`. Each owns its binary (`php`, `php-cgi`, `php-fpm`, `phpdbg`, …) and chooses its bundle features, as revision 35 keeps pcntl and posix to the CLI. The `php` crate folds into `sapi/cli`; the binary keeps its name. *Amended 2026-10-09 by #2332 (Mitchell): a host php does not ship is laid out the same way, one crate under `sapi/` over `php_sapi`; the production server `sapi/makala`, the binary `makala`, is the first, with a ceiling of its own and no `porting` or `phpt` row, since it ports nothing of php-src.* *Amended 2026-10-25 by ADR-0016 (#3465, Mitchell): the binaries are named after the product from one constant, `<name>`, `<name>-cgi`, `<name>-fpm`, `<name>-dbg` and `<name>-linked`, with `php` an opt-in symlink; `makala` keeps its name.*
- `php_sapi` is the hosts' facade, as `php_ext` is the extensions': it re-exports `php_engine`'s host face (`sapi.lock`), and holds besides only what php-src shares between SAPIs: the CGI environment made a request description (cgi, fpm), header emission, `php_getopt` (cli, cgi, phpdbg). The contract stays a request description and a sink, not a trait (revision 3).
- Edges: `sapi/*` → `php_sapi`, `php_proto`, `php_bundle`; `php_sapi` → `php_engine` through `sapi.lock`, and `php_proto`. No host names `php_engine`, `api` or an extension. *Amended 2026-10-01 by #1348 (Mitchell): a host that declares natives of its own, php's `additional_module` and `additional_functions`, depends on `php_ext` as an extension does, and hands the extension it built to `EngineBuilder`; it names no extension and never `php_engine` or `api`.* *Amended 2026-10-07 by #1911 (Mitchell): a host reads frames, op arrays and the linked tables through the native face from its own tier, as an extension does; `tier.lock` keeps the mechanism, the writes and nothing that reads.*
- `unsafe` gains the homes `sapi/*/src/ffi/`, as `ext/*/src/ffi/`; every other host is `#![forbid(unsafe_code)]`.
- Budgets by #727's rule (1.25 × php-src's C lines, rounded up to 500), each row carrying its `sapi/<name>/tests` subset and `porting = true`.

## Considered options

- **Every host in `php_sapi` (revision 1).** Rejected for the reasons above.
- **Hosts depending on `php_engine` directly.** Rejected as ADR-0004 rejected it for extensions: the host face is a seam, and one facade keeps its lock in one place.
- **A trait every host implements.** Rejected: revision 3 replaced the `Sapi` trait with a request description and a sink, and a host has nothing to implement.
- **A shared helper crate under `sapi/`.** Rejected: `php_sapi` holds what php-src itself shares between SAPIs, and anything wider is engine policy.

## Consequences

- Workspace members gain `sapi/*`; xtask learns the directory (`CRATE_DIRS`, `Crate::path`), and a scaffold lays a host down with its row, edge and binary, as `new-ext` lays an extension.
- `php_sapi`'s ceiling falls to what a facade with shared code needs; the `php` row goes; the `phpt` subsets move from `php_sapi`'s row to each host's.
- Each host is a slot of its own in the `/specs` loop.
