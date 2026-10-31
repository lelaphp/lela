---
name: refactor-recon
description: Systematic reconnaissance of a codebase or subsystem to find refactor opportunities, foundational improvements, better abstractions, and brittle code — producing an evidence-backed, leverage-ranked report rather than immediate edits. Use whenever the user asks to find refactor opportunities, audit tech debt, identify what's brittle or fragile, propose foundational or structural improvements, spot duplication or missing abstractions, assess codebase health, or asks "what would you clean up", "where should we invest", "is there duplication we should collapse" — even when they name no specific file and even if they never say the word "refactor". Also use when the user wants help prioritizing refactoring work they already suspect exists, and for a diff-scoped run over a commit range or merged batch asking whether what it added already exists elsewhere. For reviewing the quality of one already-chosen function or file, a scoped review skill fits better.
---

# Refactor Recon

Refactoring has no intrinsic value; it buys cheaper future changes. So this investigation is not an aesthetics sweep — it is a search for **structural facts that tax upcoming work**, weighted by how often that work will make the team pay. The deliverable is a report (a leverage map), not edits.

The compressed lens, if you hold only one sentence: look for places where **knowledge is duplicated, invariants are unstated, agreement is maintained by hand, or a module is shallow** — weighted by how often the roadmap will make you pay for them.

Two scopes. A **full run** is the three-lane sweep below over a subsystem or repo. A **diff-scoped run** is what the user asks for when they name a commit range or a merged batch: the question is narrower — *does what this range added already exist elsewhere, and did it widen an interface?* — and the run is correspondingly lighter (see Phase 1).

## Operating principles

- **A refactor with no consumer is speculation.** If you cannot name the concrete upcoming work a change makes cheaper, file it and move on. Infinite candidates, finite leverage.
- **Surprise is unpaid comprehension debt.** Every moment a reader is surprised tracing real code is a cost the next ten readers pay too. Log surprises; they are findings.
- **Test pain is design pain.** Elaborate setup and heavy mocking point at hidden, tangled dependencies in the unit under test — the finding is the design, not the tests.
- **The right fix is often not consolidation.** Multi-site edits are fine when they're *checked* (a missed site fails the build). Making forgetting impossible frequently beats collapsing the sites.
- **Deleting is the highest-confidence refactor.** Speculative generality isn't neutral clutter; it obscures the real design and widens every read. Version control remembers.
- **Probe before believing.** Comments and docs describe the *intended* design; only execution reveals the actual one. Refactoring toward what a comment claims is how you break things confidently.

## Phase 0 — Pin the forcing function

Before reading any code, establish the direction of pressure: what are the next ~10 changes likely to be? Sources, in order of reliability:

1. What the user tells you (roadmap, planned features, known pain).
2. Recent git history — what *kind* of work keeps happening (`git log --oneline -50` and skim subjects for themes).
3. Issue tracker / TODO clusters, if reachable.

If none of these yield a clear picture, ask the user one question: *"What are the next several changes you expect to make here?"* — the answer decides the ranking in Phase 3. Without a forcing function, findings can still be collected, but say explicitly in the report that they are unweighted.

## Phase 1 — Evidence sweep

Three independent lanes. When subagents are available, run them in parallel; each lane's output is a list of candidate findings with raw evidence attached. Calibrate depth to repo size: for a large codebase, run Lane A first and let its hotspot ranking choose where Lanes B and C spend their reading time.

**Diff-scoped run:** skip Lane A and Lane C. Take the files the range touched plus every caller of each function it added or widened, and run Lane B over that set with signals 1 and 8 first. Churn does not rank here; the range is the ranking.

### Lane A: Git archaeology

Run the bundled miner (stdlib-only Python, needs `git`; the script lives in this skill's directory, not the target repo):

```bash
python3 <skill-dir>/scripts/hotspots.py --repo <target-repo> --since "18 months ago" --top 25
```

It produces three tables — interpret them, don't just paste them:

- **Hotspots** (churn × size): files that are both large and frequently edited concentrate comprehension debt. These decide where to read next.
- **Fix magnets** (fix/bug/revert commit density): a file that accumulates fix commits isn't unlucky — it usually encodes a **model mismatch**, and the patches are epicycles. Open the actual fix commits for the top magnets: if the fixes share a theme, the model is wrong and the theme names the finding. Scattershot fixes may just mean a busy file.
- **Co-change coupling**: files that repeatedly change in the same commits but live in distant parts of the tree indicate a **misdrawn boundary** — one idea split across modules, or a concept missing that both sides need.

Two interpretation traps seen in practice: log-style files (changelogs, burndown journals) will top the churn table in doc-heavy repos — process artifacts, not findings — while a *docs file co-changing with code* may be a hand-maintained mirror (signal 5) worth flagging. And a heavily co-changing pair guarded by codegen or a completeness test is *checked* lockstep working as designed (signal 4) — note it approvingly, don't report it.

### Lane B: Static signal hunt

Read `references/signals.md` before this lane — it is the full catalog with detection recipes (grep patterns, per-language notes, false-positive warnings) and the typical fix shape for each signal. The signals, in rough order of historical yield:

1. **One concept, many spellings** — the same semantic idea in N representations; conversion functions between *internal* types are the tell.
2. **Unstated invariants** — correctness depending on a fact the compiler can't see ("must call init first", `Option`s that "can't happen", boolean params, sentinels).
3. **Unchecked lockstep** — adding one thing requires editing N sites and nothing fails if you miss one. A written checklist in the docs is the confession.
4. **Hand-maintained symmetry** — pairs that must agree (encode/decode, parse/print, two backends) with no generator or differential test guarding the agreement.
5. **Speculative generality & dead weight** — one-impl abstractions, never-set config, flags fully rolled out but still branching.
6. **Glue density** — modules that mostly transform data between shapes instead of doing work.
7. **Vocabulary drift** — one word with two meanings or two words for one concept; naming drift predicts structural drift.
8. **Shallow module** — an interface nearly as wide as its implementation: a trait or class with dozens of methods, a body too long to have a name, one walk over a structure hand-written at N call sites. Churn-blind by nature — the third copy of a walk sits in a quiet file one call away from the hotspot — so apply the **deletion test** to the top hotspot's callers, not just the hotspot.

### Lane C: Flow trace

Pick one or two *real* end-to-end flows (a request, a command, a job — whatever the system actually does for a living) and read them start to finish. One of them passes through Lane A's top hotspot, and at that point read **every caller** of the hotspot's main entry, including the quiet ones — a copy of the hotspot's logic that lives in a file nobody edits is invisible to churn and only a caller read finds it. While tracing:

- **Keep a surprise journal.** Write down every point where the code did something you didn't predict — indirection you didn't expect, state mutated from afar, a name that lied. Each surprise is a candidate finding.
- **Apply the new-hire test.** Try to explain the flow as you would to a new teammate. Every place the explanation needs an "except" or a "but actually" marks a candidate.
- **Check the vocabulary** against the domain: do the code's nouns match what the team calls things?

## Phase 2 — Verify (probe-refutes-spec)

Before a finding goes in the report, verify its load-bearing behavioral claims empirically: run the relevant tests, execute the path with a small probe script, or read the actual runtime behavior — not just the comment describing it. This matters twice over: an unverified claim can make the report wrong, and the *gap itself* (comment says X, code does Y) is often a separate, stronger finding.

Anything you could not verify, mark **UNVERIFIED** in the report rather than silently asserting it.

## Phase 3 — The leverage map

Synthesize findings into a ranked report. Rank by **roadmap-weighted leverage**: how much of the Phase-0 forcing function each finding taxes, times how expensive the tax is — not by how offensive the code looks.

For each recommended finding, name its **fix shape** from this taxonomy (the diagnosis→treatment mapping matters; the wrong fix shape wastes the whole effort):

| Fix shape | When it's the right move |
|---|---|
| **Collapse representations** | One concept, many spellings — pick the survivor, migrate, delete the rest |
| **Make forgetting impossible** | Unchecked lockstep — codegen from one schema, exhaustive matches (no `_ =>` over open concepts), a completeness test |
| **Make illegal states unrepresentable** | Unstated invariants — typestate, newtypes, sealed constructors, parse-don't-validate |
| **Make agreement automatic** | Hand-maintained symmetry — generate both sides from one spec; failing that, a permanent round-trip/differential test |
| **Delete** | Speculative generality, dead config, rolled-out flags |
| **Deepen** | Shallow module — give the nameless body a name, route the N walks through the one that already exists, narrow the interface to what varies |

**The rewrite exit.** A finding is a ticket-sized move. When several findings share one fix shape across two or more crates or packages, or a finding needs a design ruling (an ADR, a spec amendment) before its first stage can be green, that is not a ticket — it is a **rewrite candidate**, and the report says so in its own section rather than dressing it as N tickets. Naming the cluster is the deliverable; charting the rewrite is the user's decision.

### Report structure

Render the leverage map as a single self-contained HTML file, following [HTML-REPORT.md](HTML-REPORT.md) for the scaffold, section layout, diagram patterns, and tone. Write it to the OS temp directory — resolve from `$TMPDIR`, falling back to `/tmp` (`%TEMP%` on Windows) — as `<tmpdir>/refactor-recon-<slug>-<timestamp>.html`, open it for the user (`open` on macOS, `xdg-open` on Linux, `start` on Windows), and tell them the absolute path. The report is a **local file**, explicitly: it lives on disk and opens in the user's own browser — never published as an Artifact, uploaded, or hosted anywhere. ALWAYS include these sections:

- **Forcing function** — what the ranking is weighted against and where that came from. (Or: "No forcing function established — findings below are unweighted.")
- **Leverage map** — one card per recommended finding, ranked: **structural fact** (one sentence of what is true), **evidence** (files, commit counts, grep hits, probe results — concrete, checkable, `UNVERIFIED` marked), **tax** (which upcoming work this makes expensive, and how), **fix shape** (one of the six above, applied to this finding), **cost / risk** (hours / days / milestone, and what could break).
- **Rewrite candidate** — present only when the rewrite exit fired: the cluster, the shared fix shape, the ruling it waits on. Absent otherwise; an empty section is noise.
- **Filed, not recommended** — real findings with no current forcing function, recorded so they aren't re-discovered from scratch, explicitly not proposed for action now. Not optional filler — it is what separates this method from a tech-debt grievance list. Recommending everything you found means you ranked nothing.
- **What I'd do first** — the single highest-leverage move, and its first behavior-preserving stage.

In chat, alongside the report path: a two-or-three-sentence summary — the forcing function, the top finding, and the first move.

**Diff-scoped run:** no HTML. The report is Markdown, written where the user said or returned in chat, with the same sections minus the forcing function (the range is the forcing function). "Nothing found" is a complete report and the common one; say it in one line.

## Guardrails

- **This skill produces the investigation, not the refactor.** Do not start editing code unless the user asks. When they do ask, carry over the discipline: characterization tests first where coverage is thin, then behavior-preserving stages that are each independently green — never mix behavior change with structure change in one step, because when something breaks you won't know which one did it.
- **Don't let the tools substitute for judgment.** The script finds *where* to look; the signals catalog says *what* to look for; deciding whether a finding is real and worth acting on requires reading the code and thinking about the roadmap. A hotspot with high churn from healthy feature work is not a finding.
- **Report honestly.** If the sweep found little, say so — "this codebase is in good shape for its roadmap" is a valid and valuable conclusion.
