# Conflicts that recur

Read when a merge into `main` conflicts, or the build or the hook refuses a merge that git took cleanly. In main's checkout, `--ours` is `main` and `--theirs` is the branch.

## The ones git shows

- **`AGENTS.md`** conflicts only in computed regions. Take `main`'s side and re-render: `git checkout --ours -- AGENTS.md && cargo xtask regions AGENTS.md && git add -- AGENTS.md`.
- **A lock** (`heap.lock`, `api.lock`) where both sides added items. The resolution is the union in sorted order; `cargo xtask api-check` confirms it, and a count past `api_items` is a person's (SKILL.md, step 6).
- **A design ledger** (`harness-design.md`, `third-engine-design.md`, `jit-design.md`). A branch writes each revision it adds as `**NEXT. Revision NEXT (<date>).**`, `NEXT-2` for a second in one file, and cites it as `revision NEXT` in the sections it changes, since two branches that both took the next number conflicted at every merge (#2982). The merge numbers them: in the merged file, replace each `NEXT` (and `NEXT-2`) of the branch, the ledger row and each citation, with the next free revision after `main`'s last, in order. Pre-commit refuses a commit on `main` that still carries `Revision NEXT`, naming the line.
- **`Cargo.lock`**: take `main`'s side, `git checkout --ours -- Cargo.lock`, and let `cargo build -p php_sapi_cli -p php_sapi_cgi` add only what's missing. A fresh `cargo generate-lockfile` moves every dependency the branch never asked to move.

## The ones the build or the hook shows

Git merges these cleanly and the result doesn't build, or builds and fails a witness.

- **A struct gained a field** on one side, and literals written on the other lack it (`Parameter::prefer_ref`, which ext/standard's `stub.rs` and ext/ctype's literals lacked). Add the field at every literal, with the value the branch that added it gives the neutral case.
- **An enum variant gained a field** (`Link::Frame`'s stack), and matches written on the other side don't bind it. Bind it at each match, and read what the new field means there before choosing `..`.
- **Two branches encoding one state two ways**, as `fe_pos` was between #681's hash iterators and #658's foreach over a Traversable. Resolve to one typed representation that serves both, never a third encoding beside the two: the standard in [builder-prompt.md](builder-prompt.md) holds a merge as it holds a builder.
