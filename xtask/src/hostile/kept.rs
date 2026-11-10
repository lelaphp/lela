//! The runs the lane keeps, and the one it holds a run to. A run is the
//! interpreter's verdict on each program, kept at
//! `target/harness/hostile-runs/<engine tree>.json` in the workspace that
//! took it, where the engine tree is the tracked files under what the
//! binary was built from, as its dep-info names them ([`BuiltFrom`]), and
//! the paths are kept in the run. So a commit that writes no source of the
//! binary keeps the run where it was, and the run at the merge base is the
//! one, in any checkout of the repository, whose paths the base's tree
//! hashes to its name, as `phpt --against` finds a subset's (#725).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::key::Snapshot;
use crate::track;

/// Where a workspace keeps its runs, relative to its root: in the reading
/// store, beside the phpt lane's.
pub(crate) const RUNS: &str = "target/harness/hostile-runs";

/// The interpreter's verdict on one program, as a run keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Verdict {
    /// Its streams and its exit are the oracle's.
    Met,
    /// They differ, and a `trace:` row covering its `hostile:` id accepts it.
    Accepted,
    /// They differ, and no row accepts it.
    Differs,
    /// The engine faulted on it.
    Fault,
}

/// One run as it is kept. A field it does not know is ignored, as a
/// reading's is (harness-design.md §4.2, #4404): a run passed over is no
/// base, and with no base a program that met php there and differs now
/// refuses nothing, so refusing a newer harness's run would loosen the
/// lane it is meant to hold. So a field is added only where the run read
/// without it refuses no less than the run read with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Run {
    /// The commit the workspace stood at.
    pub commit: String,
    /// The [`crate::oracle::Pin::digest`] the oracle's side ran under.
    pub recipe: String,
    /// The blob id of the corpus file the run was taken over.
    pub corpus: String,
    /// The paths, relative to the root, the engine tree is hashed over.
    pub built_from: Vec<PathBuf>,
    /// Each program's verdict by its id; a program the oracle did not
    /// complete has none.
    pub verdicts: BTreeMap<String, Verdict>,
}

/// What the workspaces keep at a commit's engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Base {
    /// The newest run there under the pin.
    Kept(Run),
    /// Runs there under other pins alone.
    Stale,
    /// No run there.
    None,
}

impl Run {
    /// Keep the run in the workspace at `root`, named by `tree`, the
    /// engine tree over [`Run::built_from`]; where it was kept.
    pub(crate) fn keep(&self, root: &Path, tree: &str) -> Result<PathBuf> {
        let file = root.join(RUNS).join(format!("{tree}.json"));
        let text = serde_json::to_string_pretty(self).context("rendering a hostile run")?;
        crate::make_parent(&file)?;
        crate::reading::replace(&file, text.as_bytes())?;
        Ok(file)
    }
}

/// The run kept at `commit`'s engine under the pin whose digest is
/// `recipe`, in any checkout of the repository at `root`.
pub(crate) fn at(root: &Path, commit: &str, recipe: &str) -> Result<Base> {
    let snapshot = Snapshot::commit(root, &track::rev(root, commit)?)?;
    let roots =
        track::checkouts(root).unwrap_or_else(|_| fs::canonicalize(root).into_iter().collect());
    let mut found: Option<(Option<SystemTime>, Run)> = None;
    let mut stale = false;
    for checkout in roots {
        let Ok(files) = crate::entries(&checkout.join(RUNS)) else {
            continue;
        };
        for file in files {
            let Some(run) = read(&file) else {
                continue;
            };
            let named = file.file_stem().and_then(|s| s.to_str());
            if named != Some(snapshot.tracked_over(&run.built_from)?.as_str()) {
                continue;
            }
            if run.recipe != recipe {
                stale = true;
                continue;
            }
            let written = fs::metadata(&file).and_then(|m| m.modified()).ok();
            if found.as_ref().is_none_or(|(at, _)| written > *at) {
                found = Some((written, run));
            }
        }
    }
    Ok(match found {
        Some((_, run)) => Base::Kept(run),
        None if stale => Base::Stale,
        None => Base::None,
    })
}

/// The run kept at `file`, where it is one: a file that is no run, a
/// partial write or another lane's, is passed over as no run.
fn read(file: &Path) -> Option<Run> {
    if file.extension().is_none_or(|e| e != "json") {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(file).ok()?).ok()
}
