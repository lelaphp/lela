---
name: to-spec
description: "Turn the current conversation into a spec and write it as a draft for review: no interview, just synthesis of what you've already discussed."
---

This skill takes the current conversation context and codebase understanding and produces a spec. Do NOT interview the user; just synthesize what you already know.

A **spec** is the long form of a chart: a reviewed issue that no builder takes. On the tracker it is a `frontier:chart` issue with its `track:<track>` label, and `/frontier work <track>` cuts its tickets. It goes public with the repository, and anyone who can read an issue can read its edit history, so a spec is clean at its first publication.

## Process

1. Explore the repo to understand the current state of the codebase, if you haven't already. Use the project's domain glossary vocabulary throughout the spec, and respect any ADRs in the area you're touching.

2. Sketch out the seams at which you're going to test the feature. Existing seams should be preferred to new ones. Use the highest seam possible. If new seams are needed, propose them at the highest point you can. The fewer seams across the codebase, the better - the ideal number is one. State them under Testing Decisions; Mitchell's review is where they are checked.

3. Write the spec using the template below to `~/dax-php-specs/<track>.md`, one spec per session, and commit it in that directory's own git (`git init` it where it is not yet a repository), so each review round is a diff. The draft lives there, outside every checkout and any session's scratch directory, since the session that publishes is a later one; it is a local file and nothing is hosted: no OnePager and no Artifact.

4. Run the exclusion check below on the draft, then stop: print the draft's path, its commit and the check's result. The draft is this skill's deliverable.

5. Publish only on Mitchell's word, in that later session, after the check passes on the exact file: `gh issue create --label frontier:chart --label track:<track> --body-file <draft>`, with no parent and never `ready-for-agent`.

## What a spec leaves out

A spec carries no crash programs, reproducers, php-src defect text, GHSA or CVE references or list of known memory faults; no legal findings, employer, lawyer, name risk, marks or press; no launch words, dates, gate numbers, version strings or go/no-go; no names or hosts (people, hosted pages, home and scratch paths, session ids, a personal account as a registry owner); no agent process (model names, subagents, permit counts); no bench number a person has not signed; no ticket numbers, old repository name or commit hashes; no billing, quotas, runner addresses or secret names; and no per-ticket defect table, since the per-ticket detail (boxes, reproducers, old numbers) lives in the private ledger. A known gap is stated as the outcome: "the heap checks its free list as php does", never "the free list is unprotected".

The check runs on every draft round and on the file published. It reads the private names, hosts and paths from `names.txt` beside the drafts, one pattern per line, so that no checked-in file carries them:

```sh
grep -n -i -E -f ~/dax-php-specs/names.txt -e '#[0-9]+|GHSA|CVE-|SIGSEGV|Termsig|exit 13[49]|lawyer|counsel|employer|launch|\bG[0-9]\b|session [0-9a-f]{8}|\b[0-9a-f]{9,40}\b|[0-9]×' <draft>
```

The draft is clean when grep exits 1; a hit, or exit 2 for a missing `names.txt`, stops publication until a person has read it.

<spec-template>

## Destination

What reaching the end of this spec looks like, from the user's perspective, in one or two lines. Every session orients to it before choosing a ticket.

## Problem Statement

The problem that the user is facing, from the user's perspective.

## Solution

The solution to the problem, from the user's perspective.

## User Stories

A numbered list, one user story per outcome a user sees, each in the format:

1. As an <actor>, I want a <feature>, so that <benefit>

<user-story-example>
1. As a mobile bank customer, I want to see balance on my accounts, so that I can make better informed decisions about my spending
</user-story-example>

## Implementation Decisions

A list of implementation decisions that were made: the modules that will be built or modified and their interfaces, architectural decisions, schema changes, API contracts, specific interactions.

Do NOT include specific file paths or code snippets. They may end up being outdated very quickly.

Exception: if a prototype produced a snippet that encodes a decision more precisely than prose can (state machine, reducer, schema, type shape), inline it within the relevant decision and note briefly that it came from a prototype. Trim to the decision-rich parts, not a working demo, just the important bits.

## Testing Decisions

The seams from step 2, which modules are tested at each, what makes a good test there (external behavior, not implementation details), and the prior art in the codebase.

## Rulings carried

Each ruling a person settled that the spec rests on, word for word, never re-derived or paraphrased:

**R-<spec><n>.** <who>, <date>. Applies to story <n>; design §<section>.
> <the sentence, word for word>

The id is the only reference: no ticket number, comment link or session id; the mapping from id to source stays in the private ledger. A ruling authorized for the private repository only is quoted on Mitchell's word for that row. A ruling whose sentence holds excluded text reads `<Mitchell's sentence>`, for Mitchell to replace at review.

## Decisions so far

Empty at publication: `/frontier work` appends one line per closed ticket.

## Not yet specified

The fog: in-scope work not yet sharp enough to ticket, coarser than a ticket, not pre-sliced. `/frontier work` graduates it into tickets three to five at a time.

## Out of scope

Work ruled past the destination.

## Further Notes

Any further notes about the feature.

</spec-template>
