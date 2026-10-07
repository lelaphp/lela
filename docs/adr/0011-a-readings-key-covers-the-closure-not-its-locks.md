---
status: accepted
date: 2026-10-06
---

# A reading's key covers its crate's whole dependency closure, not the locks of the crates it depends on

#1318 made a reading's key cover the crate's files and the files of every workspace crate in its dependency closure, as cargo's fingerprint does (`key.rs`). So one private edit to `php_engine` makes every crate built from it stale: after a one-line edit, `done` listed 296 to 360 unmet conditions on 2026-10-06, nearly all of them clippy, test and witness over the ~75 crates above the engine. The architecture review of 2026-10-06, which looked into why xtask is slow, proposed keying a reading of an interface-only instrument (clippy, vocab, api-check) over the crate itself plus the locks of the crates it depends on. A dependency's private edit would then leave those readings standing. Settled by Mitchell in that review: the candidate is dropped.

## Decision

- A reading's key stays the hash over the crate's files and its whole dependency closure, as #1318 settled, for every instrument.
- A lock does not stand in for a dependency's sources in any key.

## Considered options

- **Key interface-only instruments over the dependencies' locks.** Rejected: a lock lists item names and nothing else (`crates/php_ext/api.lock` holds lines such as `__private::Entry`). A dependency can change a function's signature, or add `#[must_use]` or `#[deprecated]`, without its lock changing, and that changes what clippy finds in the crates that call it. A key over locks would call such a reading fresh when it is not, which is the failure #1318 exists to prevent: a reading standing over code it never measured.
- **Make the locks carry signatures and attributes, then key over them.** Rejected for now: it widens all eight locks and api-check to solve a cost that the review's other candidates attack directly. One file read per run (#2163), batched cargo runs (#2165) and an engine-free harness binary (#2166) bring the cost of re-measuring down without making any reading less sound.
- **Hash each dependency's public interface some other way (rustdoc JSON, `cargo public-api`).** Rejected: computing the interface needs a build of the dependency, so the key would cost about what the re-measurement it saves costs.

## Consequences

- A private edit to a crate still makes every reading built on it stale. The work goes into making that re-measurement cheap (#2163, #2165, #2166), not into narrowing what counts as a change.
- A later review that finds `done` still slow after those tickets land can reopen the signature-bearing locks option. It would start from this ADR, and would have to show that the locks then capture everything clippy reads across a seam.
- The sentences #1719 proposes for harness §4.2 and CONTEXT.md, which describe the key as covering the closure, stay the ones to settle.
