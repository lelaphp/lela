# The third engine

A PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion. This glossary names the seams; `third-engine-design.md` is the source of truth for behaviour, `jit-design.md` for the compiled tier and `harness-design.md` for the harness. Each is one living document with a ledger on top; the `rN` files are records.

## Language

### Programs

**Unit**:
A compiled, immutable PHP program: its declarations, op arrays, literal tables and line tables, shareable across threads once published. The format is one vocabulary, produced by the compiler for user code and by the native macros for natives.
_Avoid_: module, script, compilation result

**Published**:
The engine-owned form of one cached unit: the immutable unit beside the engine's `Func` for each of its op arrays, their safepoint words, and the map-pointer offsets assigned at publication. What a request, a trace and a claim hold to keep an op array alive; the unit cache stores these.
_Avoid_: cache entry, loaded unit, `Func` table

**Declaration model**:
The unlinked description of a class, function, constant or attribute as its producer wrote it: names, declared types, parameters, doc comment, declaration site, and constant expressions still unevaluated. Part of a unit.
_Avoid_: descriptor, metadata, arginfo (php's name for one slice of it)

**Linked table**:
The frozen, engine-owned form of declarations after linking: parents resolved, inherited members placed, identities assigned. What Reflection, dumpers and the stub cross-check read.
_Avoid_: class table alone (functions and constants are linked too), registry

**Constant expression**:
An unevaluated expression tree carried by a declaration: a default value, an attribute argument, a class constant or an enum case. Evaluated by the engine on first use, kept in its written form for Reflection.
_Avoid_: default value (that is the result, not the expression), literal

**Token stream**:
The lossless sequence of tokens for one source file, including whitespace, comments and heredoc indentation, each token knowing its kind, offset and length. What the tokenizer extension, syntax highlighting and the halt-compiler offset read.
_Avoid_: lexeme list, trivia (that names only the insignificant tokens)

**Tree**:
The typed syntax tree of one source file: one node kind per grammar rule, children as named fields resolved once at build time, significant tokens only, no parent links. What the compiler walks.
_Avoid_: AST as a distinct thing from the tree (there is one tree), green tree, red tree, CST

### Natives

**Native**:
A PHP function or method whose body is Rust. Declared once, in its Rust signature, from which its declaration model is derived.
_Avoid_: builtin, internal function, intrinsic

**Parameter / Argument**:
A parameter is a declared slot on a function; an argument is the value passed to it at a call. Php's error text keeps this distinction ("Argument #1 ($x)") and so does the engine.
_Avoid_: arg, param as nouns for the value; `Ret` for a return

**Return**:
The value a native hands back to the engine.
_Avoid_: ret, result (which means `Result` in Rust)

**Tentative return type**:
A native method's declared return type that php enforces on the native itself but not yet on user overrides: a subclass method that omits it or narrows it incompatibly gets a deprecation, not a fatal, and `#[\ReturnTypeWillChange]` on the override silences that. One bit on the return type in the declaration model, set only by natives, read by the linker and Reflection and nothing else.
_Avoid_: soft type, ReturnTypeWillChange as the name of the type (it is the attribute that suppresses the deprecation), "no return-type info" (Mahout's mis-encoding)

**Native face**:
The whole interface a native may touch, published by the engine as a small number of facets. Nothing outside the facets is reachable from an extension.
_Avoid_: request context (the implementation behind the face), runtime API

**Facet**:
One named group of the native face with a short interface and a deep implementation: diagnostics, symbols, callbacks, output, ini, streams and resources, request environment. Zend builtins that need frames use a privileged facet inside the engine.
_Avoid_: port, capability, service

**Extension**:
One module of natives, classes, constants, ini entries and lifecycle hooks, registered as a value and linked at module startup. Order of registration is derived from the declarations, never from call position.
_Avoid_: ext crate, plugin

**Bundle**:
The set of extensions compiled into one binary, in module-startup order, chosen by cargo features on one crate. What php's `internal_functions.c` lists and `get_loaded_extensions()` reports. The engine links whatever bundle it is handed; the empty bundle serves a Zend-only script.
_Avoid_: registry, plugin list, enabled extensions (the list, not the concept)

**Request state**:
One extension's per-request value, declared on its extension, built at request start and dropped after the shutdown hooks, in bundle order and its reverse. Reached through the request, never ambient; private to the extension that declared it.
_Avoid_: tenant, ext state, globals (php's word for the per-thread struct this replaces half of), thread-local

**Engine state**:
One extension's per-engine value, built at module startup from the startup ini and immutable afterwards, shared by every request. What changes across requests lives in the engine's cache of immutable values inside it, never behind a lock the extension owns.
_Avoid_: module globals, persistent state, static

### Requests

**Request description**:
Everything a host knows about one request before it starts: arguments, environment, working directory, server variables, request headers, body stream, SAPI name, and whether it is a web request. One plain value, handed over once.
_Avoid_: SAPI (the host), context, config

**Output sink**:
Where a request's bytes go: one rendered header block, then body bytes. The engine owns headers, status, output buffering and the commit moment; a sink only writes.
_Avoid_: backend, writer, transport

**Host**:
A program that builds request descriptions and supplies output sinks: the CLI, CGI, the FastCGI worker, the dev server, the in-process test harness. Hosts parse protocols and contain no engine policy.
_Avoid_: SAPI as a synonym for the crate (SAPI is what php calls the host role), driver

**Serve**:
The one request transaction: compile, seed superglobals, run, commit headers, shut down, report observables. Every host crosses it; the embed API is this and nothing else.
_Avoid_: run, execute, handle

### Correctness

**Oracle**:
The real `php` binary, pinned by version, digest and invocation recipe in one place. The only second opinion the engine is compared against.
_Avoid_: reference implementation, upstream php (ambiguous with php-src the source tree)

**Witness**:
A PHP snippet in rustdoc beside the code it exercises, with the oracle's exact output pinned next to it. Every kernel and every native carries at least one.
_Avoid_: doctest, example, golden test

### Tiers

**Tier**:
What runs a program's op arrays: the interpreter alone, or the interpreter with compiled code beside it. The engine's loop asks the tier only at safepoints; the choice is bound once per engine. The design of record for the compiled tier is `jit-design.md`.
_Avoid_: JIT (the crate, not the seam), executor (there is one), mode

**Safepoint**:
One of three places the loop asks the tier what to do: a loop back-edge, the activation of a frame after its parameter prologue has run without raising, and a return. Nothing else in the loop consults the tier. The compiler numbers the safepoints of each op array; the entry is number zero, and a back-edge or return carries its number in a field php leaves unused.
_Avoid_: hook, probe, counter handler (php's mechanism)

**Word**:
The opaque two-atomic slot the engine allocates per safepoint of a function, and the compiled tier allocates per exit, whose bits only the tier interprets: heat in one half while cold; recording, compiled with a trace index, or blacklisted in the other. The trace index is one number in one space whichever word holds it. A sealed word, all ones, is one the tier never acts on; a request-scoped op array (`eval`, an uncached include) has only sealed words.
_Avoid_: feedback (the per-request `MapPtr<[Feedback]>` of `research/third-engine-design.r2.md` §5.14, which the next engine revision deletes), counter, flag

**Interrupt**:
The one flag on a request that the execution timer and signal delivery set and nothing else touches. The loop tests it at a back-edge, at a frame's activation and after every `Resume`; compiled code tests it once per loop iteration and exits when it is set; the interrupt helper that runs at the check raises the timeout fatal or dispatches the pending signals.
_Avoid_: vm_interrupt (php's field), timeout flag, pending signal, tick

**Trace**:
One recorded straight-line path through the op arrays, compiled to machine code, holding its unit alive, with exits back to the interpreter. A root starts at a hot safepoint; a side trace starts at a hot exit of another trace.
_Avoid_: compiled function, region (month three's word), tracelet (HHVM's)

**Exit**:
A guard in a trace that failed. Compiled code stores what it held in registers, returns the exit's number, and the trampoline lands it at an opline of whatever frame is current, whose layout is identical to the interpreter's. An exit carries its own word, so a hot exit grows a side trace by the same machine that grew the root.
_Avoid_: deopt (the whole protocol), bailout (that is a fatal), side exit as a distinct thing

**Landing**:
What the trampoline must do at an exit that no compiled code could: set the opline, free named temporaries, or restore an interrupted call sequence. Decided when the trace is built.
_Avoid_: exit flags (php's bitset), fixup, compensation (that is what the exit block itself does)

**Claim**:
Ownership of one word in the recording state, taken by a compare-and-swap and settled exactly once as compiled, cold with one more failed attempt, or blacklisted. A claim that is dropped settles itself as a failed attempt, so a panic or bailout cannot leave a word recording forever. It owns what keeps its word alive and may leave the thread that took it.
_Avoid_: lock, lease, ticket

**Edge**:
What one step of the interpreter reports it did to the frame chain: the same frame, a callee pushed, the frame popped to its caller, the frame moved to the heap, an exception left pending, or the entry frame of the enclosing execute popped. The recorder reads the edge; it never infers it from frame pointers.
_Avoid_: transition, continuation, `Next` (the handler's return inside the loop)

### Harness

**Instrument**:
One `xtask` subcommand that measures or judges something and ends by writing a reading: `witness`, `phpt`, `compile-identity`, `crosscheck`, `port-map`, `api-check`, `budget`, `unsafe-audit`, `bench`, `fuzz`, `mutants`, `permit`, `done`, `vocab`. The design of record for the harness is `harness-design.md`.
_Avoid_: check, gate (that is a numbered pass/fail bar such as G1), tool, script

**Seam manifest**:
The one tracked data file, `seams.toml`, that declares each seam once: producer, consumers, lock file, permitted edges, `unsafe` homes, forbidden types, line ceilings, the ADR that decided it, and the text a failure prints. Every rule sensor is derived from it; a rule stated anywhere else is a copy.
_Avoid_: config, rules file, policy, `api-check`'s tables

**Reading**:
The one-shaped value an instrument writes after a run: instrument, scope, rule, status, numbers, message, ADR, the scope's tree hash, commit, time. Everything an agent or a person reads about the state of the engine is a rendering of readings by `xtask status`.
_Avoid_: result, artefact, report, telemetry, output

**Divergence**:
One accepted difference from the oracle, a row in `docs/divergence.toml` keyed by a witness hash, a phpt path, a compile-identity path or a trace site, with a kind, a reason, a status and a date. The oracle module consults the registry on every comparison; a person writes the reason.
_Avoid_: recorded difference, known disagreement, XFAIL, exception, divergence-table row as a thing apart from the registry
