# The third engine

A PHP 8.5 engine in Rust that copies Zend's execution model once, with one executor, held to the real `php` binary as its second opinion. This glossary names the seams; `third-engine-design.md` is the source of truth for behaviour, `jit-design.md` for the compiled tier and `harness-design.md` for the harness. Each is one living document with a ledger on top; the `rN` files are records.

## Language

### Programs

**Unit**:
A compiled, immutable PHP program: its declarations, op arrays, literal tables and line tables, shareable across threads once published. The format is one vocabulary, produced by the compiler for user code and by the native macros for natives.
_Avoid_: module, script, compilation result

**Published**:
The engine-owned form of one cached unit: the immutable unit beside the engine's `Func` for each of its op arrays and their safepoint words. The map-pointer offset assigned at publication is each op array's own, carried on the op array as php's `zend_op_array` carries it. What a request, a trace and a claim hold to keep an op array alive; the unit cache stores these.
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

**Property hook**:
A property's `get` or `set` body, php's word since 8.4 (`zend_property_hook_kind`, `prop_info->hooks`, `ReflectionProperty::getHook`): declared with the property, inherited and overridden per kind, and run in place of the plain read or write. A property with a hook and no backing store is virtual.

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
_Avoid_: arg (php's `arg_type_error`, `arg_value_error`, `arg_count_error`, `arg_null_deprecated`), param (php's `coerce_param`) as nouns for the value; `Ret` for a return

**Return**:
The value a native hands back to the engine.
_Avoid_: ret, result (which means `Result` in Rust; the FastCGI 1.0 specification names `GET_VALUES_RESULT` and `get_values_result`)

**Tentative return type**:
A native method's declared return type that php enforces on the native itself but not yet on user overrides: a subclass method that omits it or narrows it incompatibly gets a deprecation, not a fatal, and `#[\ReturnTypeWillChange]` on the override silences that. One bit on the return type in the declaration model, set only by natives, read by the linker and Reflection and nothing else.
_Avoid_: soft type, ReturnTypeWillChange as the name of the type (it is the attribute that suppresses the deprecation), "no return-type info" (Mahout's mis-encoding)

**Native face**:
The whole interface a native may touch, published by the engine as a small number of facets. Nothing outside the facets is reachable from an extension.
_Avoid_: request context (the implementation behind the face), runtime API

**Owner**:
One counted claim on a refcounted value. Whoever holds an owner hands it to a sink or gives it up; a native gives up exactly the owners it took and did not hand on. Not php's reference (`&`), which is a value with owners of its own.
_Avoid_: strong reference

**View**:
A refcounted value named without an owner. It stays valid only while an owner elsewhere keeps the value alive, so it is never handed to a sink as it stands and never kept across user code.

**Sink**:
A call on the native face that takes an owner from its caller: from the call on, the value (and a string key, counted as a value is) is the sink's. Not the **Output sink**, where a request's bytes go.
_Avoid_: consumer (a seam's importing crates, the **Manifest**'s consumers, is its own sense)

**Facet**:
One named group of the native face with a short interface and a deep implementation: diagnostics, symbols, callbacks, output, ini, streams and resources, request environment. Zend builtins that need frames use a privileged facet inside the engine.
_Avoid_: port (a URL's is php's and lexbor's word, ext/lexbor's `ffi::url::Url::port`, `ffi::url::Url::set_port`, `ffi::url::ErrorKind::PortOutOfRange` and `ffi::url::ErrorKind::PortInvalid`), capability, service

**Extension**:
One module of natives, classes, constants, ini entries and lifecycle hooks, registered as a value and linked at module startup. Order of registration is derived from the declarations, never from call position.
_Avoid_: ext crate, plugin

**Bundle**:
The set of extensions compiled into one binary, in module-startup order, chosen by cargo features on one crate. What php's `internal_functions.c` lists and `get_loaded_extensions()` reports. The engine links whatever bundle it is handed; the empty bundle serves a Zend-only script.
_Avoid_: registry, plugin list, enabled extensions (the list, not the concept)

**Request state**:
One extension's per-request value, declared on its extension, built at request start and dropped after the request-end phases (`rshutdown`, `deactivate`, `post_deactivate`), in startup order and its reverse. Reached through the request, never ambient; private to the extension that declared it.
_Avoid_: tenant, ext state, globals (php's word for the per-thread struct this replaces half of), thread-local

**Engine state**:
One extension's per-engine value, built at engine start, in startup order, from what `Start` carries (the startup ini, the host's name and the process states), immutable afterwards, shared by every request, and dropped after the engine-end phase. Beside it, an extension's process state is built at most once per process, before its engine start, and never ended: process exit releases it (ADR-0010). What changes across requests lives in the engine's cache of immutable values inside it, never behind a lock the extension owns.
_Avoid_: module globals, persistent state, static (php's `read_static_prop`, `write_static_prop`; `abi::Static`, php's `static` return type)

### Requests

**Request description**:
Everything a host knows about one request before it starts: arguments, environment, working directory, server variables, request headers, body stream, SAPI name, and whether it is a web request. One plain value, handed over once.
_Avoid_: SAPI (the host; php's `sapi_name`), context, config (php's `PHP_CONFIG_FILE_PATH`, `PHP_CONFIG_FILE_SCAN_DIR`)

**Output sink**:
Where a request's bytes go: one rendered header block, then body bytes. The engine owns headers, status, output buffering and the commit moment; a sink only writes.
_Avoid_: backend, writer, transport

**Host**:
A program that builds request descriptions and supplies output sinks: the CLI, CGI, the FastCGI worker, the dev server, the in-process test harness. Hosts parse protocols and contain no engine policy.
_Avoid_: SAPI as a synonym for the crate (SAPI is what php calls the host role; php's `sapi_name`), driver

**Serve**:
The one request transaction: compile, seed superglobals, run, commit headers, shut down, report observables. Every host crosses it; the embed API is this and nothing else.
_Avoid_: run, execute (`tier::execute`, the tier's entry, design §4.3), handle

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
_Avoid_: JIT (the crate and its adapter, `Jit`, not the seam), executor (there is one), mode

**Safepoint**:
One of three places the loop asks the tier what to do: a loop back-edge, the activation of a frame after its parameter prologue has run without raising, and a return. Nothing else in the loop consults the tier. The compiler numbers the safepoints of each op array; the entry is number zero, and a back-edge or return carries its number in a field php leaves unused. Not a hook: that word is php's, for a property's `get` and `set` (**Property hook**).
_Avoid_: probe, counter handler (php's mechanism)

**Word**:
The opaque two-atomic slot the engine allocates per safepoint of a function, and the compiled tier allocates per exit, whose bits only the tier interprets: heat in one half while cold; recording, compiled with a trace index, or blacklisted in the other. The trace index is one number in one space whichever word holds it. A sealed word, all ones, is one the tier never acts on; a request-scoped op array (`eval`, an uncached include) has only sealed words.
_Avoid_: feedback (the per-request `MapPtr<[Feedback]>` of `research/third-engine-design.r2.md` §5.14, which the next engine revision deletes), counter, flag

**Interrupt**:
The one flag on a request that the execution timer, signal delivery, an extension's periodic interrupt and a deferred FFI callback set, and nothing else touches. The loop tests it at a back-edge, at a frame's activation, after every `Resume` and after an internal call returns; compiled code tests it once per loop iteration and hands the check to the loop, by an exit or, where php's trace takes its interrupt stub, by returning to it and entering the trace again; the interrupt helper that runs at the check raises the timeout fatal, or dispatches the pending signals and serves every extension's interrupt hook.
_Avoid_: vm_interrupt (php's field), timeout flag, pending signal, tick (php's `register_tick_function`, `unregister_tick_function`)

**Trace**:
One recorded straight-line path through the op arrays, compiled to machine code, with exits back to the interpreter, and freed with its epoch's trace table, never alone; the epoch keeps every unit it inlined until then. A root starts at a hot safepoint; a side trace starts at a hot exit of another trace.
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

### Distribution

**Platform**:
An operating system, architecture and C library a release is built and smoke-tested for. Cargo's triple is a target, and a host is the program that serves requests.

**Release**:
A version of Lela named by a tag: its tarballs and its images, built from that commit, and what it attaches beside the tarballs: `install.sh`, `platforms.json`, which lists each platform, its tarball and whether `install.sh` installs there, and `SHA256SUMS` over every other attached file.
_Avoid_: release manifest (the manifest is `seams.toml`)

**Image**:
The container image a release publishes, one per platform under one name. A container is an image running. An image built from `main` is tagged `nightly-<variant>` and moves on every merge to `main`; the word names the channel, not a schedule.

### Harness

**Instrument**:
One `xtask` subcommand that measures or judges something and ends by writing a reading, in the order `cli::Command` has them: `api-check`, `permit`, `done`, `track-work`, `ticket`, `usage`, `review`, `bench`, `budget`, `clippy`, `test`, `miri`, `phpt`, `port-map`, `compile-identity`, `crosscheck`, `trace-dump`, `hostile`, `fuzz`, `fold`, `dispatch-census`, `fpm-compare`, `soak`, `unsafe-audit`, `steer`, `divergence-reason`, `witness`, `vocab`; `mutants` is named by `harness-design.md` §4.2 and not built yet. What an instrument does to a crate or a rule is to *hold* it. The design of record for the harness is `harness-design.md`.
_Avoid_: check (the manifest's `check` predicate and `api_check`, its module), gate (that is a numbered pass/fail bar such as G1), tool (a rule's `tool`, and the hook's `tool_name` and `tool_input`, Claude Code's words), script (php's `script_name`)

**Command**:
One `xtask` subcommand that acts or answers and writes no reading: `track` and its `next` and `chains`, `worktree`, `regions`, `new-ext`, `generate`, `status`, `oracle propose`, `hosts`. `hosts` builds the hosts of the oracle's list in the profile it is named and prints each one's path, and takes a seat as the building instruments do; `hosts --list` prints each host and where a release ships it, its tarballs and which images, one JSON object a line for `dist/` to read, and builds nothing and takes no seat. What decides which of the two a subcommand is, is the reading and never the name it hangs under, so `track work` is the one subcommand of a command that is an instrument: it judges what it lands and writes a `track-work` reading. A command is not an instrument, so `done` and `status` never look for its reading; what it answers is asked again each time.
_Avoid_: helper, utility, subtool

**Tally**:
The one module behind the corpus lanes: the runner ladder (a control is a skip, an absent oracle is a skip, the engine is pass or fail), the rule that nothing run is not green, the print loop and the reading. A lane contributes its judgement and how to judge one item.
_Avoid_: scoreboard, summary, aggregator

**Seam manifest**:
The one tracked data file, `seams.toml`, that declares each seam once: producer, consumers, lock file, permitted edges, `unsafe` homes, forbidden types, line ceilings, the ADR that decided it, and the text a failure prints. Every rule sensor is derived from it; a rule stated anywhere else is a copy.
_Avoid_: config (php's `PHP_CONFIG_FILE_PATH`, `PHP_CONFIG_FILE_SCAN_DIR`), rules file, policy, `api-check`'s tables

**Reading**:
The one-shaped value an instrument writes after a run: instrument, scope, rule, status, numbers, message, ADR, the scope's key (§4.2), commit, time, the run of the process that wrote it (one id per process, printed once, or the one `XTASK_RUN` names), and the profile of the build it judged where the instrument knows it. Everything an agent or a person reads about the state of the engine is a rendering of readings by `xtask status`.
_Avoid_: result, artefact, report, telemetry (the manifest's `[telemetry]` table: the field `telemetry` and `Telemetry`, its type), output (php's `OutputOp`, `output_buffer_op`, `output_started_at`; seam 4's `OutputSink`)

**Caller**:
Who asked `done` for its verdict, which each `done` reading names at the head of its message: the Stop hook, the `SubagentStop` hook, pre-push, CI, or a hand at the command line (`done::Caller`). The Stop's verdict is over the checkouts the stopping session and agent wrote in, as permit recorded them; pre-push's and CI's is over the checkout they run in.
_Avoid_: invoker, requester

**Released**:
Said of a block the second Stop let through (`stop_hook_active`): the turn ends with the conditions unmet, and a `done` reading over `released` records the count the block had.
_Avoid_: forgiven

**Owed**:
Said of a reading a crate must have fresh and passing before `done`: what an `[[owed]]` row of the manifest names for the crates it covers. A row with `held_at = "merge"` is **delegated** in a checkout whose branch records a ticket: `done` there lists it under the merge check's name and does not hold it, and main's checkout holds it.
_Avoid_: obligatory

**Remedy**:
What a denied call does instead, a rule's `remedy` in the manifest, which a denial renders after the rule's text and before the checkout it judged.
_Avoid_: suggestion

**Defect**:
What a person's file carries that parses but does not validate across files, such as a variant's build made against a closure the pin has moved past (`manifest::Defect`). Only the instruments that read the failing part refuse; permit and `done` judge by what loaded and note it, and the session's facts list it. A file that does not parse is no defect: loading it fails.
_Avoid_: broken pin, invalid pin

**Merge check**:
The verdict `land` asks for after `done`: whether a head's kept runs pass every lane it owes, which decides whether main moves. Not built yet: coordinators run the lanes and judge them by eye.
_Avoid_: batch check, merge gate

**Hostile lane**:
The instrument `hostile`, not built yet: a fixed corpus of PHP programs whose error handlers disturb an opline mid-step, each held to the oracle. A program fails it on a fault, or by differing now where it met php at the merge base.
_Avoid_: differential fuzz (the `fuzz` instrument's)

**Nightly lane**:
The CI workflow `nightly.yml`, run once a night: the lanes too long for a commit, such as `phpt --full`, the tier's lanes and soak, outside `done`. The `nightly-<variant>` image tag is not it: that moves on every merge.
_Avoid_: nightly build, nightly run (say which)

**Unmet item**:
One entry of `done`'s list, with a kind: failing, stale, missing or refused. It is not a divergence's kind, which names a lane.
_Avoid_: unmet line

**Repeat**:
A Stop's list that names nothing the last judged Stop of this session and agent did not: the same list or a shrunk one. A repeat passes and is recorded.
_Avoid_: nothing-new pass

**Found work**:
Something a session met that its ticket did not ask for. It is verified, then built on the same ticket before the ticket lands, or filed.
_Avoid_: leftover, follow-up

**Verifying**:
The step in which a spawned agent checks each item of found work against the tree and the tracker, and says whether a fresh builder builds it or it is filed. The verifying agent edits nothing.
_Avoid_: triage (G7's sorting of what outsiders file)

**Brief**:
What a spawned builder starts with, rendered from its ticket by `xtask ticket brief`.
_Avoid_: builder prompt

**Role**:
One of builder, reviewer and advisor: a file under `.claude/agents/` that sets a spawned agent's default model and effort, and its tools. It is a person's file.
_Avoid_: agent type (Claude Code's `agentType`)

**Seat**:
One of the machine's few places a building instrument runs in, `XTASK_SEATS` of them (three unless set), each an exclusive lock on a file under `XTASK_SEAT_DIR` that the operating system gives back however its holder ends. `clippy`, `test`, `miri`, `phpt`, `compile-identity`, `crosscheck`, `trace-dump`, `hostile`, `fuzz`, `fold`, `dispatch-census`, `fpm-compare`, `soak`, `witness`, `bench` and the `hosts` command take one before they build and wait while every one is held; what a seated instrument spawns sits in its seat (`seat`). A bench on a dedicated machine takes every seat (`bench --machine dedicated`).
_Avoid_: lease

**Snapshot**:
Each file of the workspace and its blob id, read once from the working tree, the index or a commit, and asked for the key of every scope a run stamps or holds: a reading's key is a lookup on one (`key::Snapshot`). An instrument takes its snapshot before it measures, so a reading carries the key of what it measured, and a file written while it ran leaves the reading stale rather than fresh; one that writes sources and then stamps them takes another after it writes. It lives as long as the run and is never kept between runs: git's index is what spares re-reading a file git knows to be unchanged.
_Avoid_: blob cache, key cache (it is no store kept between runs)

**Track**:
The tickets that carry one `track:<name>` label, worked toward the destination of the one `frontier:chart` issue that carries the same label: `track:parity-compiler` and the chart `Chart: parity-compiler, to phpt parity`. A ticket with no `track:` label is `main`'s, so every ticket is exactly one track's. A track has no branch, clone or store: each ticket is worked on its own branch cut from `main`, and `xtask track next --track <name>` names the next.
_Avoid_: workstream, experiment branch, a track's branch (ADR-0009)

**Ticket**:
One GitHub issue in this repository: what happened, what to build, its acceptance criteria and what blocks it, filed once, recorded ready on its branch by `xtask ticket ready` with what its work opened and found, and closed when its work is on `main`: the merge's hook runs `xtask ticket merged`, which closes each ticket the merge's subject names through `xtask ticket close`. A `Ticket:` trailer names the ticket a commit settles and closes nothing. A thing found and not done is found work: verified before the ready record, then built on the same ticket as a box of its own or filed as its outcome says, and a person's act or a blocker is filed at once; a session with no ticket files what it finds. The only question a filed ticket raises is whose, and that is a label, not a noun: `ready-for-agent` for an act an agent may take, `for-a-person` for one that is a person's alone (harness §11): a manifest row, a divergence reason, an oracle bump, the decision a design revision states. A branch records the ticket its work is for. The prose no sensor reads is the ticket itself, where a person reads the word before anything closes, and the harness's own rustdoc until the manifest holds `xtask` to this section; that is acceptable because the entry's words are two, and a person reads the ticket. The commit message is read by commit-msg, against the glossary each crate the commit stages is held to, which notes what it finds and refuses nothing until a person rules otherwise. At the end of a whole task a session may leave to a ticket: a defect it found outside the ticket's scope, a decision that is a person's alone, a blocker outside the work, and cleanup or refactoring it noticed and was not asked for. It may not leave to a ticket any box of the ticket it works, the witness or test its own change needs, a regression its own change caused, or the lock line, design sentence or region its change makes stale.
_Avoid_: leftover (hides whether the thing was filed), follow-up (hides whose it is)

**Map**:
`AGENTS.md` at the workspace root: what an agent reads first, a person's prose and the rest rendered as computed regions, loaded into every session by the runtime itself, through `CLAUDE.md`'s `@AGENTS.md` (harness-design.md §5). §5 has called it the map since revision 1 and the harness spells the word in its own code, so the map is this file and nothing else. It says where a session starts and never where the track is going; that is the track's chart.
_Avoid_: context file, system prompt, a track's map (the chart is that, and the two were one word until revision 22)

**Frontier**:
What a track has left to work, as the tracker has it: the open tickets labelled `ready-for-agent` that no open ticket blocks, that are the track's (carry its `track:` label, or, for `main`, no `track:` label at all), and that no branch of the repository records. `xtask track next --track <name>` names the widest by the census, the number breaking a tie, `xtask track work` runs them until none is named, and the frontier is empty when `next` says `none`. A ticket closes when its work is on `main`, so a blocker whose branch waits on its merge is open and blocks. It is a state of the tracker read fresh each time, never a list a session keeps.
_Avoid_: backlog (everything not yet done; a frontier is what is workable now), sprint, milestone

**Chart**:
One issue on the tracker, labelled `frontier:chart`, that says where one track is going: a Destination taken from the row of design §11 the engine stands at, the route walked as a line per closed ticket, the fog of work not yet sharp enough to ticket, and what is ruled past the destination. Its open sub-issues are the tickets, so the chart indexes and never restates, and a session loads it cold in one read. `/frontier chart <track> <days>` lays it down and files the first batch; `/frontier work <track>` files the next when the frontier empties. The noun is the verb that writes it.
_Avoid_: roadmap, board, a track's map (revision 22 settled the word on `AGENTS.md`)

**Spec**:
A spec is the long form of a chart: a reviewed issue that no builder takes.

**Divergence**:
One accepted difference from the oracle, a row in `docs/divergence.toml` keyed by a witness hash, a phpt path, a compile-identity path, an extension's name, a trace site, a conformance suite's case or a test the production server answers otherwise by design, with a kind (the lane that reads it), a cause (whether it is parity debt: `debt` by default, while `php-ub`, `by-design` and `oracle-build` are counted apart from the day-30 target), a reason, a status and a date. The oracle module consults the registry on every comparison; a person writes the reason.
_Avoid_: recorded difference, known disagreement, XFAIL, exception (php's `set_exception_handler`, `restore_exception_handler`; `Thrown::exception`; `raising::throw_exception_ex`, php's `zend_throw_exception_ex`), divergence-table row as a thing apart from the registry
