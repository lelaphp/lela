//! What permit keeps beside its readings, in main's store, since a reading
//! per call would grow the store by one per `Bash` call: how many
//! calls each rule judged, the denominator its denials are read against,
//! and per session and agent the checkouts its writing calls were placed
//! in, which the Stop holds (harness-design.md §4.5).
//!
//! Each is one JSON file rewritten whole under a lock, since the two
//! adapters and every session's calls write it at once.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::reading::{self, Store};
use crate::track;

/// The calls judged per rule, under `<main store>/permit/`.
const JUDGED: &str = "judged.json";
/// The calls handed on since a call was last judged, each with when.
const HANDED: &str = "handed.json";
/// How many calls handed on within [`WINDOW`] with none judged are a burst:
/// the walk ending in a cargo that cannot build, which the hook passes on
/// unjudged and nothing else records.
pub(super) const BURST: usize = 3;
/// The window a burst falls in, in seconds.
const WINDOW: u64 = 60;

/// The checkouts written in per agent of one session, under
/// `<main store>/permit/wrote/<session>.json`; the main loop is the agent
/// named by the empty string.
type Wrote = BTreeMap<String, BTreeSet<PathBuf>>;

/// `permit/` in the store of the main checkout of the repository `root` is
/// in, or of `root` where it is in none.
fn dir(root: &Path) -> PathBuf {
    let main = track::main_worktree(root).unwrap_or_else(|_| root.to_path_buf());
    Store::of_workspace(&main).root().join("permit")
}

/// The value at `path` changed by `change` and written back, read and
/// written under the lock beside it; an absent file is the default.
fn update<T: Default + Serialize + DeserializeOwned>(
    path: &Path,
    change: impl FnOnce(&mut T),
) -> Result<()> {
    let parent = path.parent().context("a ledger file has a directory")?;
    std::fs::create_dir_all(parent)?;
    let lock = crate::lock_file(&path.with_extension("lock"))?;
    lock.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    let mut value = read(path);
    change(&mut value);
    reading::replace(path, &serde_json::to_vec(&value)?)
}

/// The value at `path`, the default where it is absent or unreadable.
fn read<T: Default + DeserializeOwned>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// One call judged by each of `rules`, counted, which ends any run of
/// calls handed on.
pub(super) fn count(root: &Path, rules: &[String]) -> Result<()> {
    let dir = dir(root);
    if dir.join(HANDED).exists() {
        std::fs::remove_file(dir.join(HANDED))?;
    }
    if rules.is_empty() {
        return Ok(());
    }
    update(&dir.join(JUDGED), |judged: &mut BTreeMap<String, u64>| {
        for rule in rules {
            *judged.entry(rule.clone()).or_default() += 1;
        }
    })
}

/// The call `id` handed on, kept; how many distinct calls were handed on
/// within the window with none judged since, this one included.
pub(super) fn handed(root: &Path, id: &str) -> Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let mut within = 0;
    update(
        &dir(root).join(HANDED),
        |handed: &mut BTreeMap<String, u64>| {
            handed.insert(id.to_owned(), now);
            handed.retain(|_, at| now.saturating_sub(*at) <= WINDOW);
            within = handed.len();
        },
    )?;
    Ok(within)
}

/// The calls each rule judged, as the store under `store` keeps them.
pub(crate) fn judged(store: &Store) -> BTreeMap<String, u64> {
    read(&store.root().join("permit").join(JUDGED))
}

/// The file of `session`'s writes, its id made a file name.
fn wrote_path(root: &Path, session: &str) -> PathBuf {
    let name: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir(root).join("wrote").join(format!("{name}.json"))
}

/// `checkout` added to those `agent` of `session` wrote in.
pub(super) fn wrote(root: &Path, session: &str, agent: &str, checkout: PathBuf) -> Result<()> {
    if session.is_empty() {
        return Ok(());
    }
    update(&wrote_path(root, session), |wrote: &mut Wrote| {
        wrote.entry(agent.to_owned()).or_default().insert(checkout);
    })
}

/// The checkouts `agent` of `session` wrote in, as recorded in the store
/// of the repository `root` is in; none for a session that wrote nothing.
pub(crate) fn written(root: &Path, session: &str, agent: &str) -> Vec<PathBuf> {
    let mut wrote: Wrote = read(&wrote_path(root, session));
    wrote
        .remove(agent)
        .unwrap_or_default()
        .into_iter()
        .collect()
}
