---
status: accepted
date: 2026-10-13
supersedes: third-engine-design.md §3.3 (permanent library rule), harness-design.md §7 (actual linkage required after replacement)
narrows: ADR-0014 for migrated date operations; remaining C callers retain its contract
---

# Rust owns engine algorithms; native bindings are transitional

## Context

The engine is Rust, but the previous library rule deliberately linked the libraries PHP links. `crates/timelib_sys/build.rs`, `ext/mbstring/build.rs`, `ext/lexbor/build.rs` and `ext/fileinfo/build.rs` compile vendored C and shims; other extensions bind installed libraries. `sapi/makala/src/tls.rs` explicitly selects a native crypto provider through a Rust interface. A source-suffix scan or Rust dependency name therefore does not establish a Rust runtime closure.

The native interface carries real protocol costs. Date reconciles a foreign time's zone pointer with its Rust owner (`ext/date/src/ffi/mod.rs`). DOM copies a transient Lexbor tree into libxml (`ext/dom/src/html5_parser.rs`). Mbstring copies foreign output into a Rust vector and then a PHP string (`ext/mbstring/src/ffi/mbfl.rs`). Those are deletion opportunities, not measurements of speed gains.

The maintainer changed the destination and ruled its trade-offs on #2817, with dedicated reviews and contracts on #2818, #2824, #2822, #2819, #2820 and #2823. The confirmed final packet is #2817's comment `6149727197`; confirmation and Q1–Q20 are recorded there. #2825 lands the wording and evidence-gated split.

## Decision

Engine design §7.3 is the live shared and candidate contract. Every engine-owned runtime algorithm is Rust throughout its transitive implementation closure at completion, and completed product builds invoke no C/C++ compiler or preprocessor. Explicit reviewed platform-service, assembly and user-selected foreign-code allowances remain. The real pinned PHP oracle remains external; this is not a claim of zero C in the operating system or development environment.

Observable algorithms, bytes, errors, allocation and failure timing, callbacks and supported features remain binding. Unobservable C layouts and foreign protocols can change. Specifically accepted divergences preserve their particular safe behaviour; new unsafe oracle cases need evidence and a person's reason.

Prefer demonstrably compatible maintained Rust implementations, including narrowly scoped pinned compatibility forks when required. Record source/data lineage, licensing, maintenance and security-update routes. Pinned prebuilt generators can produce reproducible Rust build output; they do not justify compiling C dependencies or committing generated implementation.

Compatibility values and implementation identity are different facts. PHP-program-visible compatibility targets can remain where required; actual-linkage rows identify the actual Rust implementation. Upstream lineage and data versions are reported separately, and informational differences still need particular accepted rows.

Coherent interoperable slices migrate with one authoritative active representation. Temporary bindings and bridges retain named removal work and their current contract until their users migrate. Native-removal provenance, ownership/allocation and adversarial evidence precede completion; workload/tolerance and numerical interface/budget decisions are proof-gated. The single PHP executor, immutable published state, extension discipline and manifest authority remain unchanged.

## Considered options

- **Continue linking the same C libraries.** This remains a strong compatibility and update strategy: it obtains library diagnostics, data and many algorithms by construction. Rejected as the final destination because it retains engine-owned native implementation and the cross-language protocols the maintainer ruled to remove. It remains the transition adapter until proof supports each switch.
- **Substitute similar Rust libraries.** Maintained libraries reduce local maintenance and may support a smaller implementation. Similar feature sets or standards compliance do not establish PHP's exact algorithms, data, diagnostics, allocation or streaming behaviour. Historical zlib experiments on #950 and #1557 differed in bytes; date and encoding substitutes previously omitted observable behaviour. Reuse needs demonstrated compatibility, not language-based confidence.
- **Author every library independently.** This gives local control of each algorithm and representation but commits the project to unnecessary maintenance, update translation and security work. Prefer suitable maintained Rust machinery and upstream contributions; use a faithful owned port or scoped fork where evidence requires it.
- **Switch everything at once.** A single cut avoids transitional interoperability, but delays verification until unrelated algorithm and ownership changes meet. Coherent slices give useful proof earlier; a slice that cannot interoperate or meet its full contract cannot become the ordinary execution path.

## Consequences

- Current bindings remain explicitly transitional. This decision changes no runtime behaviour or numerical manifest value and certifies no completed native removal.
- Library implementation is now maintenance work, including fork/data update mapping, continuing derivative licences and risk-appropriate security review. Passing PHP tests alone supplies neither source-language provenance nor side-channel proof.
- Actual request storage, lifetime, partial-failure cleanup and reclamation must be demonstrated before a typed allocator, public handle or larger budget is locked. Representative charges beside system-owned state do not qualify.
- Domain-specific ownership replaces foreign storage choreography without a generic extension facility: private date/mbstring/URI/fileinfo algorithms, libxml storage with DOM tree construction, XSL transforms and a narrowly shared compression dependency.
- Date retires ADR-0014's C-specific protocol only for migrated operations. Other C callers still require scoped lending, and allocation observables remain binding for the Rust port. Existing request holds remain useful wherever PHP callbacks can replace active state.
- Translated executable algorithms cannot hide in the binding pool, vendor or generated output to evade accounting. Existing `permit` rules and ceilings still fire; bounded proof work does not authorize an unapproved signature, edge, home, allowance or budget.
- Informational compatibility differences become honest, specifically recorded differences rather than claims that absent libraries are linked.
- Native interoperability fixtures are isolated and non-shipped. They cannot become product dependencies or alternative engine implementations.
- The native-removal instrument must distinguish remaining work, completed scopes and a completed engine. Static archives, generated objects and transitive providers require provenance; missing evidence cannot pass.

A future reader will propose keeping one C dependency because a Rust port is difficult, or selecting a Rust crate because its name sounds compatible. Keep the destination and require its proof: neither convenience establishes an exception or completion.
