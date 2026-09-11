//! A track laid down by one command (harness-design.md §5, §11): `xtask
//! track new <name>`, so every track starts the same way and none is set
//! up by hand. A track lives on `origin` as a branch, since git's ref
//! store refuses `refs/heads/t-x` beside `refs/heads/t-x/witness` (ledger
//! 14), and its root is a clone beside the repository, detached at that
//! branch: the clone's `target/harness` is the track's store, and its
//! `.claude/worktrees` holds the worktrees of its tickets, which
//! [`worktree::add`] lays down and whose [`Track::of`] reads the clone as
//! the store. So the command, in order: pushes `main` to `origin/<name>`;
//! clones `origin` beside the repository, named `<repository>-<name>`,
//! and detaches the clone at the track, so the clone has no branch of the
//! track's name either; builds `xtask` in the clone, so the first hook
//! call there is judged by a built binary and not by a cold `cargo run`
//! inside the hook's timeout (§12 item 8); adds the track's row to the
//! console's track list, `console/tracks.json`, after the rows it had,
//! since the first is the one the console opens on; and prints the
//! `computed trust` line for a person to run, the grant being keyed by
//! the root path and a person's (§11).
//!
//! A branch the repository or `origin` has (`origin` asked, not the
//! remote-tracking refs, which may be stale), a directory that exists, a
//! list that carries the row or is not one object, and a name with a slash in it, which cannot be a
//! prefix of its branches' names, refuse before anything is pushed, as
//! `worktree add` does. A command, not an instrument: nothing is measured
//! and no reading is written. The clone laid down with its build missing
//! is [`worktree::NotReady`], neither done nor undone, as a worktree's is.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::computed;
use crate::git::{git, has_ref};
use crate::track::MAIN;
use crate::worktree::{self, NotReady, complain, say};

/// The console's track list: each track's name and the directory its
/// checkout is in, relative to the repository (console/README.md, Tracks).
const TRACKS: &str = "console/tracks.json";

/// The prefix of every line printed, so a person reads whose line it is.
const NEW: &str = "track new";

/// Lay the track `name` down: `origin/<name>` at `main`, the clone beside
/// `root`, `xtask` built there, the console's row, the trust line printed;
/// or refuse and change nothing. `Ok` is the clone's directory, ready.
pub fn lay(root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') {
        bail!(
            "{name:?} is not a track's name: a track names its branches by being a path prefix of theirs, so it has no slash of its own"
        );
    }
    let dir = beside(root, name)?;
    let list = root.join(TRACKS);
    if has_ref(root, &format!("refs/heads/{name}")) {
        bail!(
            "the branch {name} exists here; a track is origin's branch and never the clone's own, so name another"
        );
    }
    let url = git(root, &["remote", "get-url", "origin"], None)
        .context("the repository has no origin, and a track lives on it")?;
    let url = url.trim();
    if git(
        root,
        &["ls-remote", "--exit-code", "--heads", "origin", name],
        None,
    )
    .is_ok()
    {
        bail!(
            "origin has {name}, so the track is laid down already; clone it by hand, or name another"
        );
    }
    if dir.exists() {
        bail!(
            "{} exists; a track's root is laid down by this command alone, so remove it, or name another",
            dir.display()
        );
    }
    let rows = fs::read_to_string(&list).with_context(|| format!("reading {}", list.display()))?;
    let relative = match dir.file_name() {
        Some(file) => format!("../{}", file.to_string_lossy()),
        None => dir.display().to_string(),
    };
    let rows = with_row(&rows, name, &relative).with_context(|| {
        format!(
            "{}: the row is not added, so nothing is laid down",
            list.display()
        )
    })?;

    git(
        root,
        &["push", "-q", "origin", &format!("{MAIN}:refs/heads/{name}")],
        None,
    )
    .context("git push refused; nothing was laid down")?;
    let tip = git(root, &["rev-parse", "--short", MAIN], None)?;
    say(NEW, &format!("origin/{name} at {MAIN} ({})", tip.trim()));

    git(root, &["clone", "-q", url, &dir.to_string_lossy()], None).with_context(|| {
        format!(
            "git clone refused; origin/{name} is pushed, so clone it by hand: git clone {url} {}",
            dir.display()
        )
    })?;
    git(
        &dir,
        &["checkout", "-q", "--detach", &format!("origin/{name}")],
        None,
    )
    .with_context(|| {
        format!(
            "the clone at {} could not be detached at origin/{name}",
            dir.display()
        )
    })?;
    say(
        NEW,
        &format!(
            "cloned {} detached at origin/{name}, whose target/harness is the track's store and whose .claude/worktrees holds its tickets' worktrees",
            dir.display()
        ),
    );

    let ready = worktree::build(NEW, &dir);
    fs::write(&list, rows).with_context(|| format!("cannot write {}", list.display()))?;
    say(
        NEW,
        &format!(
            "added the row {name}: {relative} to {TRACKS}, uncommitted; a person commits it from the main checkout, which is where the console runs"
        ),
    );
    say(NEW, &computed::trust_line(&dir));
    worktree::carried_over(NEW, &dir);
    if ready {
        say(NEW, &format!("ready: cd {}", dir.display()));
        Ok(dir)
    } else {
        complain(
            NEW,
            "the clone is laid down and the row written; build it by hand: cargo build -p xtask",
        );
        Err(NotReady { dir }.into())
    }
}

/// Where the track's root goes: beside `root`, named after it, as
/// `dax-php-t-opus-5` is beside `dax-php`.
fn beside(root: &Path, name: &str) -> Result<PathBuf> {
    let parent = root
        .parent()
        .context("the repository has no parent directory to clone beside")?;
    let stem = root
        .file_name()
        .and_then(|s| s.to_str())
        .context("the repository's directory has no name")?;
    Ok(parent.join(format!("{stem}-{name}")))
}

/// The list with the row `name: relative` appended after the rows it
/// had, as text, since the first row is the one the console opens on and
/// a parse would reorder them; the parse holds the list to one object
/// without the row already. Computed before anything is pushed, so a
/// list the row cannot go into refuses with the rest.
fn with_row(rows: &str, name: &str, relative: &str) -> Result<String> {
    match serde_json::from_str::<serde_json::Value>(rows) {
        Ok(serde_json::Value::Object(list)) if list.contains_key(name) => {
            bail!("the list carries {name} already; drop the row, or name another")
        }
        Ok(serde_json::Value::Object(_)) => {}
        Ok(_) => bail!("the list is not one JSON object"),
        Err(err) => bail!("the list is not JSON: {err}"),
    }
    let body = rows.trim_end();
    let body = body
        .strip_suffix('}')
        .context("the list does not end in its closing brace")?
        .trim_end();
    let comma = if body.ends_with('{') { "" } else { "," };
    Ok(format!(
        "{body}{comma}\n  {}: {}\n}}\n",
        serde_json::Value::from(name),
        serde_json::Value::from(relative)
    ))
}

#[cfg(test)]
mod tests {
    use super::with_row;

    #[test]
    fn the_row_goes_after_the_rows_the_list_had() {
        let rows = "{\n  \"main\": \".\",\n  \"t-x\": \"../r-t-x\"\n}\n";
        assert_eq!(
            with_row(rows, "t-y", "../r-t-y").unwrap(),
            "{\n  \"main\": \".\",\n  \"t-x\": \"../r-t-x\",\n  \"t-y\": \"../r-t-y\"\n}\n"
        );
        assert_eq!(
            with_row("{}\n", "t-y", "../r-t-y").unwrap(),
            "{\n  \"t-y\": \"../r-t-y\"\n}\n"
        );
    }

    #[test]
    fn a_list_the_row_cannot_go_into_refuses() {
        let rows = "{\n  \"main\": \".\",\n  \"t-x\": \"../r-t-x\"\n}\n";
        assert!(with_row(rows, "t-x", "../r-t-x").is_err());
        assert!(with_row("not json", "t-y", "../r-t-y").is_err());
        assert!(with_row("[]", "t-y", "../r-t-y").is_err());
    }
}
