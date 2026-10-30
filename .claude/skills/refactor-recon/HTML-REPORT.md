# HTML Report Format

The leverage map is a single-file, no-build HTML page in the OS temp directory, written in the house document language: Tailwind v4 from the browser CDN, Fraunces display over IBM Plex Sans and Mono, light and dark from one token block, hand-authored inline SVG with a prose fallback beside every diagram, and IntersectionObserver-gated animation. The chrome is already written: **start from [`references/report-base.html`](references/report-base.html)**, verbatim, and fill the `{{…}}` slots and the `{{sections}}` marker. Do not swap the fonts, the token block, the container width, or the script; do not load Mermaid or any other library. The base is lifted from `~/Projects/mahout_php_vs_dax/third-engine-jit.html`, the exemplar for component and animation architecture, with `third-engine-design.html` as the exemplar for section rhythm; open either when a pattern below is not enough.

This report is a *ranked argument*: the forcing function is the lens, the leverage-map sections are the case, and the evidence must stay checkable, so a reader can re-run any grep or reopen any commit the report cites.

## Page structure

Sticky nav → hero → §0 short version → §1 forcing function → one numbered section per recommended finding → rewrite candidate (only when it fired) → filed → first move → footer. Every section after the hero is the 12-column split with the numeral rail:

```html
<section id="f3" class="py-14 border-t border-line">
  <div class="grid grid-cols-1 lg:grid-cols-12 gap-8">
    <div class="lg:col-span-3"><div class="secnum">3</div><p class="sectitle">finding · collapse representations</p></div>
    <div class="lg:col-span-9 prose measure">
      <h2 class="text-3xl sm:text-4xl font-medium">Order lives in four spellings, and every feature touching it pays four times.</h2>
      …
    </div>
  </div>
</section>
```

The `h2` is the structural fact as one sentence ending in a full stop. The `sectitle` kicker is mono lowercase: `finding · <fix shape>`, `the lens`, `filed, not recommended`, `first move`. Section ids: `#short`, `#forcing`, then `#f2 … #fN` where the number is the rail numeral (findings count on from §1, so the top finding is §2 and `#f2`), `#rewrite`, `#filed`, `#first`. The nav in the base links the fixed ones; add one `<a>` per finding and drop the rewrite link when the section is absent. The three unnumbered sections carry a short mono word in the rail instead of a numeral (`rw`, `filed`, `first`), at the exemplar's smaller size: `<div class="secnum" style="font-size:clamp(2rem,4vw,3.4rem)">filed</div>`.

### Hero

Mono dateline (subsystem, date, commit count and window, "local file only") → `h1` = the verdict on the subsystem's shape in one sentence → standfirst = top finding and first move → the hero figure. The hero figure is the churn table from `hotspots.py` drawn as `.barrow` rows, or the top finding's before/after when the churn table is uninformative. Hero animation runs on load, ungated: `.hero .bar` grows without `play-on-view`, and SVG nodes and edges in the hero use `.h-node` / `.h-edge`, staggered by inline `animation-delay` in 60–120 ms steps.

### §0 The short version

Three to five plain sentences in a tinted card (`fig p-5 bg-accent-soft/40`): the verdict, the top finding, the first move. A reader who stops here leaves with the ranking's conclusion. No codename the card does not itself explain.

### §1 The forcing function

Its own section: what upcoming work the ranking is weighted against and where that came from, or plainly "No forcing function established — findings below are unweighted." When the roadmap is a list (tickets, issues, planned changes), render it as a `.tbl` in a `fig scroll` with the row count or weight in a `num text-right` cell. When the findings carry more than about three codenames, close the section with a terms card: a two-column `grid` of mono term → one-line definition. A term defined there is used bare below.

### A finding section

In rank order, numbered from 2. Rank is the argument and must visibly follow roadmap-weighted leverage.

- **Chip row** directly under the `h2`: `chip fix` with the fix shape (`Collapse representations`, `Make forgetting impossible`, `Make illegal states unrepresentable`, `Make agreement automatic`, `Delete`, `Deepen`), `chip cost` (`hours` / `days` / `milestone`, a qualifier allowed when the estimate needs one: `days per family`), and `chip unverified` on any section whose behavioural claims Phase 2 could not verify.
- **Evidence** as a figure (`fig p-4 sm:p-6 play-on-view`) with the title-left, caption-right header row. Files, commit counts, grep hits, probe results in mono; a churn bar, a co-change graph, or a probe transcript as `<pre>` often *is* the evidence, and drawn evidence lands harder than quoted.
- **Tax** as one short paragraph or a two-column `.cost` grid (`h` header row, then `was` / `now` cells): which upcoming work this makes expensive, and how. This ties the section to §1; a finding that cannot state its tax belongs in Filed.
- **Fix shape, applied**: one sentence on what the shape looks like here, and a before/after figure when the restructuring is graph-shaped.
- **Consumer**: the roadmap items this cheapens, as inline `.rev` pills (`<span class="rev new">779</span>`; a consumer that is not a ticket gets its name, `<span class="rev">wave-2 table</span>`), so the reader sees the link to §1 without re-reading it. `.rev` is the pill for anything that names a roadmap item or a revision; the header's subsystem pill is the same class by design.
- **Risk** as a `.steps` grid or a short list, six words a step.
- **`check:`** as the section's last line, in `figcap`: the one command or artefact that would prove the finding wrong.

No paragraphs of explanation. If a diagram needs a paragraph to be understood, redraw the diagram.

### Rewrite candidate

Present only when the rewrite exit fired; the section, the nav link, and the chip (`chip rewrite`) all appear together or not at all. It names the cluster (which findings, one line each with anchors), the shared fix shape, and the ruling it waits on. It proposes no stages: charting the rewrite is the reader's decision, and the section says so in one sentence.

### Filed, not recommended

Deliberately quiet: a `.tbl plain`-style list, one row per finding, two cells: the gist and why it has no forcing function now. No figures, no chips. The contrast with the map *is* the ranking.

### First move

One closing section: the single highest-leverage move, why it outranks the rest, its first behaviour-preserving stage as a `.steps` row with the green-gate step marked `commit`, and an anchor to its finding. Then the footer.

## Diagrams

Hand-authored inline SVG only: `viewBox="0 0 760 H" width="100%" role="img" aria-label="<full prose description>"`, nodes as `<g>` with one `rect rx="6"` and one to three `text text-anchor="middle"`, edges as orthogonal `path` with `marker-end`, one `<defs>` per SVG with the `arr` markers (ids suffixed per figure to stay unique). Colour is semantic and comes from the `.sv-*` classes in the base: teal = existing and kept, accent = new or the survivor, rust = taxed, deleted, or the unguarded site, soft grey = inert, dashed `stitch` = a seam. Labels are mono at 9.5 / 10.5 / 11.5 px (`sv-small`, `sv-t`, `sv-b`); a citation line sits under its node in `sv-small sv-muted`.

**Every SVG is `class="hidden sm:block"` and is followed by an `<ol class="sm:hidden mt-4 prose text-[0.9rem]">` that says the same thing in words.** The diagram is never merely shrunk.

Animation inside a figure uses the base's generic vocabulary, nothing per-figure: `p-node` / `p-edge` with `late` / `later` / `last` for stagger, `bar` rows that `grow` on view, `step` cards that settle in sequence. All of it is gated behind `play-on-view`; nothing off-screen moves.

Patterns, by finding:

- **Churn bars** (evidence workhorse): `.barrow` rows, label / `.bartrack` / mono value, widths proportional to churn × size, fix density, or co-change count. Colour the taxed file `c2`.
- **Co-change graph** (misdrawn boundary, glue): nodes placed by directory, `sv-dash` edges between files that change together, taxed edges in `sv-line-rust`.
- **Collapse** (one concept, many spellings; hand-maintained symmetry): N `sv-box-soft` boxes, a drawn edge, one `sv-box-acc` survivor with the retired spellings inside it as `sv-ghost`.
- **Lockstep strip** (unchecked lockstep): a row of small boxes, one per edit site, the unguarded one `sv-box-rust`; after, the same row behind one `sv-box-acc` generator or completeness test.
- **Walk copies** (shallow module): the N callers each holding a copy of the walk, the checks each copy performs as small rects, the missing check as an empty `sv-box-rust` slot; after, N thin callers and one deep box.
- **Before / after**: two `sv-box` regions side by side inside one SVG, ~320px tall, reserved for the "fix shape, applied" line.

## Responsiveness

The base already carries `overflow-x: clip` on the body, `min-width: 0` on every grid child, the `.scroll` floor widths for tables, `overflow-wrap: anywhere` on inline code and cost cells, and `scroll-margin-top` under the sticky header. Keep to it: tables always inside `fig scroll` (add `wide` past five columns), `pre` always `overflow-x: auto`, every SVG with its `sm:hidden` prose twin, and only Tailwind's `sm:` / `md:` / `lg:` breakpoints. Check the page at 400 px and at 1440 px before reporting the path: nothing overlaps, nothing scrolls sideways but a table.

## Tone

The report is read by a human who was not in the investigation and may return to it weeks later. The register is the exemplar pages': British spelling, lowercase domain nouns where the project writes them so, small structural numbers spelled out, the "X is Y, not Z" correction shape, headings that are sentences ending in a full stop, and a `check:` line at the end of a finding naming the command or artefact that would prove it wrong.

- **Every identifier is defined before it is used**, in the terms card or with a plain antecedent at first use. Paths in mono need no gloss; invented names do.
- **One idea per sentence, subject early**, under about twenty words. Two plain sentences beat one compressed chain.
- **Budgets inside a section:** prose blocks run two or three sentences; a block carrying more than two facts becomes a list.
- **The report's nouns are this skill's:** structural fact, evidence, tax, fix shape, consumer, forcing function, leverage, checked / unchecked lockstep, shallow, UNVERIFIED. Every claim is evidenced or chipped `UNVERIFIED`; nothing in between.

Phrasings that fit: "Order has four spellings; every feature touching it pays four times." "Lockstep is unchecked: a missed site compiles clean and fails in production." "Tax: all three roadmap items route through this glue." "check: `rg 'fn walk' engine/` returns one hit."

No hedging, no throat-clearing, no second person, no exclamation marks. A finding that reads as taste rather than a structural fact has not earned a section.
