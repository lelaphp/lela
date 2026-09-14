---
name: stage-commit
description: Land one unit of third-engine work as a green checkpoint. Use when the user says "commit this", "land this", "ship this", or an increment is finished and a commit is the next step.
---

A commit is a checkpoint a reader can re-run: one concern, its instruments green, a message whose every claim names the reading behind it. `AGENTS.md` says what done means and how a message reads; this is the order.

1. **Shape one concern.** If the description says "also", split there. The lock line, the witness and the regenerated `ext/clippy.toml` travel with the code that needs them; lint debt is its own commit naming the parent.

2. **Ask the oracle first.** Write the witness beside the kernel or native, pin it with `cargo xtask witness --accept`, and read what it pinned. Run the crate's phpt subset. A difference the engine is right about is a divergence row, proposed in the message, never written into the file.

3. **Say every gap.** A failed invariant is an `InternalError` with a message, never a wrong value. A difference you did not fix is an open row, proposed. What only a person may do goes under `Left for a person:`.

4. **Run the instruments** for the crates you touched: fmt, clippy with `-D warnings`, `api-check`, `budget`, `unsafe-audit`, `vocab`, `witness`, `computed run` on every file with a region, and `cargo test -p xtask` when the harness changed. Read each command's own exit, never a pipeline's. A red reading you inherited is stated, with the proof that it is not yours. Never `git stash`; every worktree shares its stack.

5. **Write the message** beside its neighbours in the log. Subject: the crate or file it touches, a colon, and the change in one plain sentence, lowercase after the colon, no trailing period: `xtask vocab: the glossary as a sensor`. One concern; if the sentence needs "also", split the commit. Body: what changed and why, with the design section or ADR; the mechanism; the reading that proves it; then the proposals and `Left for a person:`. Numbers are receipts. Describe the defect before the fix. Last, the trailer, as `git interpret-trailers` reads it, one blank line after the body: `Ticket: #<n>`, the ticket the commit settles, as the message's last line. A number cited in the body is prose and closes nothing. What the work opened is the close's `--opens`, recorded as a reading, not a trailer in the message.

Walk the done list in `AGENTS.md` last. A no anywhere is a smaller commit or a fix, not a commit.
