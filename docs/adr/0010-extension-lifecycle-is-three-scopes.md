---
status: accepted
date: 2026-10-04
supersedes: ADR-0005's `engine_state(fn(&StartupIni) -> E)` (engine start that cannot fail and sees only the ini), third-engine-design.md §4.4's state table where it calls engine-state construction `minit` and its drop `mshutdown`
---

# An extension's lifecycle is three scopes that start as values and end as phases

php runs one engine per process, so `MINIT` means both "once per process" and "engine start", and `MSHUTDOWN` both "engine end" and "process end". Here a process may build several engines, and no phase means once per process. The architecture review of 2026-10-03 (#1770) found extensions improvising around that gap: C-library global initialisation runs inside `extension()` (libxml, sodium, xsl), inside engine state (openssl, mbstring) or not at all (curl). Engine start cannot fail: `ExtensionBuilder::engine_state` takes `fn(&StartupIni) -> E` (`crates/php_engine/src/api/abi.rs`), with ten builders under `ext/`, and sees no host name. Nothing corresponds to `MSHUTDOWN` or to `post_deactivate`: the end of a request is `rshutdown` and then `deactivate`, the last place php's `shutdown_executor` reaches before the request's objects and resources are freed, and a persistent cdb file and a pool entry past its bound have no request to be closed with. Request state and engine state drop in forward order where ADR-0005 says reverse, and a second fatal in a request-end function is swallowed where php guards each extension separately. ADR-0005 also names `api::Pool` and says the `api` group gains "no trait", which a lifecycle that closes pool entries has to confirm or change. Settled by Mitchell on #1774; what an error from `rinit` means was settled beside it on #1775.

## Decision

An extension's lifecycle is three scopes, process, engine and request, that start as values and end as phases.

**Starts are values.**

- **Process:** `process_state(fn() -> Result<P, Refusal>)`, `P: Send + Sync + 'static`, reached as `&'static P`. It is built at most once per process, the first time an engine carrying the extension is built, in bundle order, and held by a once in `php_engine`, never in the extension. It sees nothing an engine is configured with, no ini and no host name, so two engines with different configuration cannot race to define it. C-library global initialisation (libxml, sodium, xsl, openssl, curl) lives here.
- **Engine:** `engine_state(fn(&Start) -> Result<E, Refusal>)`, fallible, as php's `MINIT` failure is. `Start` carries the startup ini, the host's name and the process states; its fields are private behind accessors, so a later field is not a breaking change.
- **Request:** unchanged. What an error from `rinit` means is #1775's ruling: an `Err(Thrown)` from `rinit` is php's fatal inside `RINIT`, after which no later extension's `rinit` runs, the program does not run and no request-end phase runs; the request states drop.

**Ends are phases, in reverse bundle order, each extension guarded separately** (php's `zend_try` per module).

- **Request end:** `rshutdown`, then `deactivate` (php's `shutdown_executor`, as built), then **`post_deactivate`**, after the request's objects and resources are freed, with request states still live and no PHP code allowed to run; then the request states drop. Drop is not `post_deactivate`: ADR-0005 says a state's `Drop` touches no engine, and libxml's and a tracer's work there does.
- **Engine end:** one phase run from the **engine's `Drop`**, handed a shutdown request the engine builds for it (the mirror of the startup request); then the engine states drop. `Drop` runs it exactly once whatever a host does, including in each forked fpm child, as php's `php_module_shutdown` runs there.
- **Process end: none. No host ends the process scope, ever.** Process exit releases it: `xmlCleanupParser` and `curl_global_cleanup` are documented unsafe while another thread may use the library, and nothing they do is observable (principle 2).
- Ends that only release stay a `Drop`.

**Threads.** An extension owns no thread. The engine owns every thread it runs (the execution timer, the periodic interrupt of #1806) and joins each one before a fork and at engine end.

**ADR-0005's pool sentence is confirmed and amended.** `api::Pool` stays a concrete engine-owned type, with no trait. An entry leaves the pool only through its extension, which closes it with a request: the ending request when `give_back` returns an entry past the bound (#1798), the shutdown request when the extension drains its pools in its engine-end phase (#1804). `Pool` gains a drain and a `give_back` that returns the overflow.

## Considered options

- **A process scope that sees the ini.** Rejected: two engines with different configuration would race to define a value built once per process.
- **A host-called process end.** Rejected: `xmlCleanupParser` and `curl_global_cleanup` are documented unsafe while another thread may use the library, and nothing they release is observable (principle 2).
- **An explicit consuming `Engine::shutdown`.** Rejected: a host can forget to call it; `Drop` cannot be forgotten.
- **`post_deactivate` as `Drop`.** Rejected: ADR-0005 says a state's `Drop` touches no engine, and libxml's and a tracer's work at `post_deactivate` does.
- **A `Persistent` trait on pool entries.** Rejected: it would need every pool registered with the engine, when the extension already knows its own.

## Consequences

- ADR-0005 carries an amendment line naming this file. Its `engine_state(fn(&StartupIni) -> E)` becomes `engine_state(fn(&Start) -> Result<E, Refusal>)`, so each of the ten `engine_state` builders under `ext/` changes with the code that lands it.
- The code follows in its own tickets: the process scope with libxml's initialisation moved onto it (#1800), fallible engine start and each host's refusal (#1802), the request-end and engine-end phases with `post_deactivate` (#1803), the engine-owned periodic interrupt (#1806), an entry past the pool's bound closed with the ending request (#1798), the persistent cdb file finished at engine end (#1804), and an error from `rinit` as php's fatal in `RINIT` (#1879). Until they land, the code still carries what this file replaces.
- The passages of third-engine-design.md §4.4, §5.14 and §7.1 and of `CONTEXT.md`'s Engine state and Request state entries that still describe engine state as `minit`, its drop as `mshutdown` and a start that sees only the ini wait on a person's wording (#1907).
- A future reader will want a host to end the process scope, as php's `MSHUTDOWN` ends it. Do not; process exit releases it, and this file is the answer.
