# Signal catalog — detection recipes

Each signal below predicts *expensive future change*, which is the only reason to care. Per signal: what it is and why it costs, how to detect it concretely, what the fix usually looks like, and when it's a false positive. Recipes use `grep -rn` / `rg` style patterns; adapt to the language at hand.

Contents:
1. [One concept, many spellings](#1-one-concept-many-spellings)
2. [Bug & churn clustering](#2-bug--churn-clustering)
3. [Unstated invariants](#3-unstated-invariants)
4. [Unchecked lockstep](#4-unchecked-lockstep)
5. [Hand-maintained symmetry](#5-hand-maintained-symmetry)
6. [Misdrawn boundaries](#6-misdrawn-boundaries)
7. [Glue density](#7-glue-density)
8. [Test pain](#8-test-pain)
9. [Speculative generality & dead weight](#9-speculative-generality--dead-weight)
10. [Vocabulary drift](#10-vocabulary-drift)
11. [Shallow module](#11-shallow-module)

---

## 1. One concept, many spellings

**What / why:** The same semantic idea — a type, an error, a permission, a "stream" — exists in N representations. Every feature touching the concept costs N implementations, and drift bugs breed in the gaps between representations. Historically the highest-yield signal in this catalog.

**Detect:**
- Hunt conversion functions between *internal* types — a conversion between two internal representations of one idea almost always means one should die:
  - names: `_to_`, `to[A-Z]`, `from_`, `into_`, `as[A-Z]`, `convert`, `adapt`, `translate`, `map[A-Z].*To`
  - Rust: `impl From<X> for Y` / `impl TryFrom` where both X and Y are internal; TS/JS: `toXxx(`/`fromXxx(` mapper modules; Java/Kotlin: `*Mapper`, `*Converter`, `*Adapter` classes
- Parallel dispatch over the same tag: grep a variant/enum-case name and count the `match`/`switch`/`if-else` chains that branch on it. Many parallel chains over one tag = the concept's behavior is smeared across files.
- Near-identical type definitions: multiple structs/classes/interfaces with overlapping field lists (`UserRow`, `UserDto`, `UserModel`, `UserView` — how many are load-bearing?).
- The counting question: **"if I add one variant/field to this concept, how many places must change?"** If the answer needs a grep to answer, that's the finding.

**Fix shape:** Collapse representations — pick the survivor (usually the one closest to the domain, not the wire), migrate call sites, delete the rest. Often a campaign, not a commit; stage it.

**False positive:** Conversions at a *system boundary* (wire format, DB row, third-party API) are the design working, not failing. The smell is internal re-representation — the same idea re-spelled between two modules you own.

---

## 2. Bug & churn clustering

**What / why:** Git history is a heat map. A file accumulating fix commits isn't unlucky — it usually encodes a **model mismatch**, and each patch is an epicycle. If you keep patching edge cases of one feature, the model underneath is wrong, not the patches.

**Detect:**
- Run `scripts/hotspots.py` — churn × size ranking plus fix-magnet table.
- For each top fix-magnet, open the actual fix commits (`git log --oneline --grep='fix' -i -- <file>`, then read 5–10 of them). The discriminator: **do the fixes share a theme?** A theme (all about ordering, all about null-handling, all about one state machine) names the model mismatch. Scattershot fixes may just mean a busy, central file.
- Revert commits are extra-strong evidence: `git log --oneline --grep='revert' -i`.

**Fix shape:** Depends on the theme — usually "make illegal states unrepresentable" (the state machine was implicit) or "collapse representations" (two copies of the model drifted).

**False positive:** High churn from active feature development is healthy. Churn alone is noise; churn **and** fix density together is the signal.

---

## 3. Unstated invariants

**What / why:** Working definition of *brittle*: code whose correctness depends on a fact stated nowhere the compiler (or a test) can see. These are the changes that break at a distance — someone violates the unwritten rule three modules away.

**Detect:**
- Confession comments: `grep -rn -iE "must be called|must not|before calling|after calling|caller must|assumes|careful|make sure|don't call|invariant|always non-null|never null|should already"`
- "Can't happen" escape hatches: `unwrap()`/`expect("...impossible...")`/`assert` in ordinary logic; `Option`/nullable fields that are "always set after init" (two-phase initialization is the classic).
- Boolean parameters — call sites reading `frobnicate(true, false)` carry no meaning and invert silently.
- Sentinel values: `-1`, `""`, `0`, `null` meaning "none/unset/special" instead of a real type.
- Stringly-typed data: strings compared against literals to make control-flow decisions (`if kind == "admin"`).
- Order-dependent APIs: `init()`/`setup()`/`configure()` that must precede use, enforced only by convention.

**Fix shape:** Make illegal states unrepresentable — typestate (a `Validated<T>` you can only get by validating), newtypes over primitives, sealed constructors, builders that return only fully-valid values, parse-don't-validate at the boundary so the interior never re-checks.

**False positive / triage note:** An invariant guarded by an assertion that genuinely runs in CI is *half*-stated — still fragile, but rank it below invariants that are truly only in someone's head.

---

## 4. Unchecked lockstep

**What / why:** Adding one thing requires editing N sites — classic shotgun surgery — **and nothing fails if you miss one**. The critical nuance: multi-site edits are fine when they're *checked* (a missed site is a compile error or test failure). The refactor target is not "fewer sites"; it's "forgetting is impossible."

**Detect:**
- The confession is written down: `grep -rn -iE "when adding|remember to (also )?update|don't forget|keep in sync|must match"` in comments, docs, and PR templates. A maintained checklist for adding an X means the lockstep is unchecked.
- Co-change table from `scripts/hotspots.py`: file sets that always change together.
- Wildcard arms over open concepts: `_ =>` / `default:` in dispatch over a tag that grows — every new variant silently falls into the default instead of forcing a decision.
- Registration tables built by hand (lists of handlers/routes/opcodes) with no completeness test against the enum/schema they mirror.

**Fix shape:** Make forgetting impossible — generate the N sites from one schema (codegen), use exhaustive matches so the compiler enumerates the sites, or add a completeness test (iterate the enum, assert every variant is registered). Consolidation is optional; checking is not.

**False positive:** Lockstep that already fails loudly when violated is a *working* contract — leave it alone, or note it approvingly.

---

## 5. Hand-maintained symmetry

**What / why:** Two implementations must agree — serializer/deserializer, parser/pretty-printer, reader/writer, mock/real, two backends or executors — and the agreement is maintained by human diligence. Hand-maintained symmetry is a bug factory with a delay timer: it works until the day one side changes alone.

**Detect:**
- Grep sibling names: `serialize`/`deserialize`, `encode`/`decode`, `read_`/`write_`, `save`/`load`, `pack`/`unpack`, `lower`/`lift`.
- Two directories or modules with mirrored file lists (one per backend).
- Docs tables that mirror code (a markdown table of options/opcodes/fields that must match an enum).
- Ask: **"if these two disagree, what fails, and how soon?"** If the answer is "a user notices eventually," that's the finding.

**Fix shape:** Make agreement automatic — derive/generate both sides from one spec; if genuinely impossible, install a permanent guard: round-trip property tests (decode∘encode = id) or a differential test harness that runs both sides on shared inputs and diffs the outputs.

---

## 6. Misdrawn boundaries

**What / why:** The module structure claims one decomposition; the change pattern reveals another. Files that always change together but live far apart mean one idea is split across a boundary — every change pays a coordination tax and the boundary provides no insulation.

**Detect:**
- Distant co-change pairs from `scripts/hotspots.py` (flagged in its output).
- Layering violations: a "low-level" module importing from a "high-level" one, cyclic imports, backdoor accessors added to punch through a boundary.
- Tramp data: a parameter threaded through many layers untouched, existing only so a deep callee can see it — the concept probably belongs in a context/environment object, or the boundary is drawn wrong.
- Import graph tools where available (e.g. `cargo tree`/`cargo-depgraph`, `madge` for JS, `import-linter` for Python) to spot cycles and upward edges.

**Fix shape:** Move code to where its change-pressure lives, or introduce the missing shared concept both sides are reaching for. Boundary moves are high-risk — stage behind characterization tests.

**False positive:** Some co-change is legitimate fan-out from a *checked* contract (schema + generated code changing together). Cross-reference with signal 4.

---

## 7. Glue density

**What / why:** Measure how much of a module transforms data between shapes versus doing domain work. High glue ratio means the representations are misaligned with usage — the code spends its lines apologizing for its types.

**Detect:**
- Skim-read candidate modules (use Lane A hotspots to pick) and estimate: what fraction of this file is field-copying, mapping, wrapping, unwrapping?
- Files whose names confess: `*mapper*`, `*adapter*`, `*converter*`, `*transform*`, `glue`, `shim`, `compat` — legitimate at boundaries, suspicious in the interior.

**Fix shape:** Usually collapses into signal 1 (the representations should merge) or signal 6 (the boundary is in the wrong place).

---

## 8. Test pain

**What / why:** Test pain is design pain. If exercising a unit needs elaborate setup, the unit has too many hidden dependencies; if behavior-preserving refactors break tests, the tests are coupled to structure instead of behavior. The finding is the design — the tests are just where it hurts first.

**Detect:**
- Fixture/setup size: tests where the arrange section dwarfs act+assert; deep builder chains or 200-line fixture files for one unit.
- Mock density: mocking *internal* collaborators (vs. system boundaries) to make a unit testable at all.
- Flaky clusters: `git log --grep -iE 'flak|deflake|quarantine'`; CI retry annotations.
- Slow suites forcing sparse coverage — ask, or check CI timings.
- Untested hot code: cross-reference Lane A hotspots against test coverage; a fix-magnet with no tests is a compounding risk.

**Fix shape:** Extract the hidden dependencies into explicit parameters/ports; separate the decision (pure, testable) from the effect (thin, integration-tested).

**False positive:** Integration tests are *supposed* to have setup. Judge unit-level pain, not end-to-end scaffolding.

---

## 9. Speculative generality & dead weight

**What / why:** Abstractions with one impl, parameters nobody varies, config never set, feature flags fully rolled out but still branching. Not neutral clutter — every read pays for the flexibility nobody uses, and the live design hides behind the dead one. Deleting is the highest-confidence refactor there is; version control remembers.

**Detect:**
- One-impl abstractions: for each trait/interface, count implementations (`grep -rn "impl .* for"` / "implements X"). One impl and no test double using it = the abstraction is a wish.
- Parameters always passed the same value: spot-check call sites of flexible-looking functions.
- Config archaeology: options defined in code but absent from every deployed config file.
- Rolled-out flags still branching: grep the flag registry against actual toggles.
- Language tooling for dead code: Rust `cargo +nightly udeps` / `#[warn(dead_code)]`, TS `ts-prune`/`knip`, Python `vulture`, Go `deadcode`.

**Fix shape:** Delete. If genuinely unsure, delete behind a revert-friendly commit and see who screams (in a codebase with CI, silence is data).

**False positive:** Public-API surface of a published library — unused *internally* doesn't mean unused. Check downstream consumers first.

---

## 10. Vocabulary drift

**What / why:** One word carrying two meanings ("session" = auth session *and* DB session) or two words for one concept ("user" vs "account", "job" vs "task"). Naming drift predicts structural drift — when the team can't say what a thing is called, the code usually has it in two half-shapes (see signal 1).

**Detect:**
- List the ~10 core domain nouns, grep each, and read a sample of hits: is the meaning stable across modules?
- Look for qualified renames that confess a collision: `RealUser`, `UserV2`, `NewSession`, `LegacyJob`.
- Compare code nouns against how the user/team *talks* about the domain — mismatches surface in the Phase-1 flow trace.

**Fix shape:** Settle the glossary first (a decision, not a refactor), then mechanically rename toward it; structural findings (usually signal-1 collapses) fall out of the exercise.

---

## 11. Shallow module

**What / why:** A module's interface is nearly as wide as its implementation, so callers learn almost as much as the author did and get little leverage back. Three shapes recur: a trait/class with dozens of methods where a handful vary; a function body too long to have been given a name, so the next feature at that position has nothing to call; and one walk over a structure hand-written at N call sites, where the copies drift (the second copy skips a check the first has). The last is the dangerous one — it is a latent bug wearing a refactor's clothes, and it lives in *quiet* files, invisible to churn.

**Detect:**
- Interface width: count required vs. provided methods on the largest traits/interfaces (`grep -c "fn " ` inside a `trait` block; `abstract`/`interface` members). Dozens provided over a few required = the trait is a driver and a capability surface fused.
- Nameless bodies: functions over ~300 lines, or a `match` arm that is itself hundreds of lines. Ask what the next feature at this position would *call*; if the answer is "copy the arm", the body needs a name.
- Hand-written walks: take the top Lane-A hotspot's main entry and list **every caller** (`cq refs`, `rg "fn_name\("`, an LSP). Then, for each caller, ask whether it re-implements part of the entry instead of calling it. Read the copies side by side and diff the checks they perform — a missing check in one copy is a finding *and* a defect.
- The **deletion test** on anything suspect: delete the module in your head. If its complexity vanishes, it was a pass-through (signal 9). If it reappears at N callers, it is earning its keep and the finding is that N callers should share it.

**Fix shape:** Deepen — route the N walks through the one that exists (the first stage is usually "the entry returns what it already computes"), give the nameless body a name, split a fused trait into the surface that varies and the driver that does not. Every stage is behaviour-preserving and independently green.

**False positive:** A wide interface that is *generated* from one declaration is checked lockstep (signal 4), not shallowness. A long body that is one flat table (an ordered list of reads, a dispatch over a closed enum) is data wearing a function's syntax — the fix is to declare it as data, not to split it.
