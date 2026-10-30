---
name: writing-copy
description: Use when writing any user-facing words — UI labels, button text, empty states, error messages, settings descriptions, tooltips, confirmation dialogs, changelog entries, onboarding text, or documentation and guide prose. Triggers whenever text will be read by a user of the product.
---

# Writing Copy

## Overview

A house voice for all user-facing copy. The reader is a capable operator, not a novice — write to them as a peer.

Two gates before any sentence ships:

1. **Does it carry information the reader needs and doesn't already have?** If it's obvious or self-evident, cut it entirely. Writing it better is not the fix; not writing it is.
2. **Can the reader follow it with the knowledge they already have?** If a sentence relies on a concept the reader hasn't been given, build that concept first. Never operate on an antecedent you haven't established.

Everything below serves those two gates.

## What to write (content)

- **Cut the obvious.** Empty states don't say "Nothing here." Headings aren't restated in the body. Don't explain what the reader can already infer.
- **Build antecedents in order.** Premise → consequence → takeaway. State the underlying fact before you describe what happens when it changes. ("Library entries are stored with links to a path. If files move, that link breaks. We try to repair it…")
- **Comprehensibility beats precision.** Drop qualifiers that obscure. A model the reader can follow beats an exact statement they can't. If they can't follow it, accuracy doesn't matter.
- **Reassurance earns its place only when it's the real concern.** "Your video files remain untouched" stays in a delete dialog (the genuine worry). "Your existing library is unaffected" gets cut from a transient error (filler).
- **Give the one remedy that matters,** not every possible one.

## How to write (style)

- **Reader-first, not feature-first.** Lead with the reader's situation or the action — "Files that can't be matched show up here," not "The Review queue holds…".
- **Plain and operational, not literary.** No flourish, personification, rhetorical reframes ("that's not failure, it's…"), or marketing adjectives. State what happens.
- **Heading carries the fact; body carries the consequence and the action.** Don't repeat the heading.
- **Declarative, subject-first** for descriptions: "Records … will be cleaned up after this period," not "How long to keep … before …". Reference the control in context ("this period", "this title").
- **True names** for things (the ingestion pipeline, the Review queue). Mechanism in plain terms.
- **"We" is the app/makers' voice.** Hedge honestly when behavior is best-effort: "we'll try to repair it," not "it will be repaired."
- **Link to real artifacts.** Point an operator at the source on GitHub and at the issue tracker where they can act ("the parsing code is here", "submit an issue to share a filename that wasn't parsed").
- **Close a longer explanation with a plain takeaway:** "In short, renaming or moving files can break the match and we'll try to repair it automatically."

## Before / after (real pairs)

| Register | Default / weak | Voice |
|---|---|---|
| Empty state | "Nothing to review. Every file in your library has been matched." | "Files that show up in your media directories that can't be automatically matched will show up here. You can match them by hand." |
| Error | "Can't reach TMDB. Media Centaur couldn't connect… Check your internet connection and try again. If TMDB is down, it'll resume automatically." | "Without being able to connect to their API we can't ingest new content into your media library or download new artwork. Check your TMDB API key in the settings." |
| Setting | "How long to keep completed download records and resolved error reports before they're cleaned up automatically." | "Records of completed downloads and resolved error reports will be cleaned up after this period." |
| Delete confirm | "This deletes the record and artwork. Your video files stay on disk. If they're in a scanned folder they may be re-added on the next scan." | "Clears database data and artwork related to this title. Your video files remain untouched." |
| Explanatory prose | "Media Centaur re-attaches the file to its existing entry. The match is made on the path relative to its library root plus its size — not its absolute path — so…" | "Library entries are stored with links to a particular path on your system. If the files and folders are moved, then this link is broken. To repair the item we try to match it against your media libraries by file/folder name and size. In short, renaming or moving files can break the match and we'll try to repair it automatically." |

## Register notes

- **Labels (2–4 words):** name the action, nothing else. `Repair links` · `Rescan library` · `Run setup tour`.
- **Empty states:** what fills this place + the action. Never "nothing here yet" — and no trailing status line like "None right now"; the empty screen already shows it's empty.
- **Errors:** heading = what happened; body = consequence + the one fix.
- **Settings descriptions:** declarative, reference the control.
- **Confirmations:** name the real effect; keep only the reassurance that is the genuine concern.
- **Changelog:** lead with the change in bold, then one or two plain sentences of what it does for the reader.
- **Explanatory prose:** build antecedents in order; link to source; close with "In short, …".
- **Marketing/landing copy is the one exception:** it sells. Everything else explains. Don't let marketing register leak into product copy.

## Common mistakes

| Mistake | Fix |
|---|---|
| States the obvious ("Nothing to review") | Cut it; explain what the place is for |
| Repeats the heading in the body | Heading = fact; body = consequence + action |
| Reassurance padding | Keep only if it's the genuine gotcha |
| Lists every possible remedy | Give the one that matters |
| Feature-first ("The X holds…") | Reader-first (their situation / the action) |
| Assumes an antecedent | Establish the premise before relying on it |
| Precision that obscures | Drop qualifiers; comprehensibility wins |
| Flourish / personification / marketing tone | Plain, operational; state what happens |
| Instructional "how to…" for a description | Declarative, subject-first |

## Red flags — stop and rewrite

- You wrote a status line a glance already conveys.
- The first sentence uses a term you haven't introduced.
- You're explaining *why the design is good* instead of *what happens*.
- You reached for "simply", "just", "seamlessly", "powerful", "effortless".
- The body restates the heading.
- You can't say what new thing this sentence tells the reader.
