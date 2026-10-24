//! The `unless` qualifier's sensor (harness-design.md §4.1, §4.4): a
//! `deny_pattern` row may name the instrument whose work the pattern
//! describes, and a hit that instrument left behind is permitted where an
//! authored one is not. `witness.exit-authored` is the row that carries
//! one: `cargo xtask witness --accept` writes the oracle's status onto a
//! fence line, and the same line typed by hand is what the row refuses
//! (#105).
//!
//! **What stands as evidence.** The instrument's reading and the tree hash
//! it carries (§4.2, §13 D.18). A hit is the instrument's own when it
//! already stands in the file as that instrument last saw it: the text the
//! call would leave adds no hit over what the file on disk carries, and the
//! crate the file lies in has a reading of the named instrument stamped
//! with the crate's tree hash now. `witness` constructs its reading after
//! `--accept` has written, so the line it captured is inside the tree the
//! reading is over.
//!
//! **The record of what `--accept` took.** Every `--accept`, a run over
//! the crate or one narrowed to a file or an item, records each verdict it
//! wrote, and each one the oracle's run reproduced as it stood, under the
//! crate's accepted scope, keyed by the verdict's block:
//! the `php` fence through the `expect` fence that pins it, and the
//! `stderr` fence after it ([`crate::witness::record_accepted`]). A hit
//! whose block a record names is the instrument's whatever else the file
//! or the crate became since, so a run narrowed with `--item`, which
//! writes no reading of the crate, is evidence as a run over it is, and a
//! prose edit beside the fence or a merge that moves the crate on leaves it
//! standing. The record lands at `worktree retire` as every reading does.
//!
//! **Evidence that travels with the commits.** A tree hash names one
//! state of the crate, and a merge makes a state no branch measured: a
//! line a branch's `--accept` captured reached main beside another
//! branch's change to the same crate, and the reading over the branch's
//! tree was no reading over main's (#1448). So a hit the reading over the
//! tree now does not cover is looked for in the commits that last changed
//! the file: each hit's block, its line and the lines around it up to a
//! blank one, as the file stood at a commit whose crate a reading of the
//! instrument was taken over, the commit's tree hash read from its objects.
//! The readings are this workspace's store, its file and its log, where
//! `worktree retire` appends a branch's, and the store of every other
//! worktree of the clone, where a branch merged before its retire keeps
//! them. A block typed by hand stands in no commit an instrument measured.
//!
//! **Who asks.** The diff judge, which measures a write against the base
//! and not against the file: a line `--accept` wrote stands in the tree and
//! not in the base, so the row fires there and this is what answers it. A
//! call at the hook is measured against the file it would change, so a hit
//! it adds is by that fact one the file has not got and no reading can
//! cover it — the rule's own work, since the authoring it refuses happens
//! there. Counting the hits and not looking for the words is what makes
//! that hold in a file already carrying seventy-eight of the same line: an
//! edit adding a seventy-ninth adds one (#510).
//!
//! **What stays denied.** A line left in the tree by a hand the hook did
//! not see: the crate has no reading of the instrument, or one taken before
//! the line was written, whose stamp is not the tree's, so `permit --diff`
//! and `done` deny it with the rule id.
//!
//! The file comes before the store and git, so a call that would author a
//! line pays one read and the walk ends there; a hit already in the tree is
//! rare enough to pay for its stamp.
//!
//! Evidence this cannot read is no evidence: a file it cannot read, a crate
//! the manifest does not name, a store or a git that answers with an error
//! each leave the hit denied, where the row stood before this sensor. The
//! reading's own status is not read here: a failing `witness` is condition
//! 1's to print, in the instrument's own words, and saying it twice in the
//! row's words would name the wrong problem.
//!
//! The negative control is `xtask/fixtures/rules/witness.exit-authored/`:
//! three `Edit` calls that author a status, denied here as they were
//! before, and `whole-file.json`, the file whole as a `Write` with a
//! captured status in it, which is judged twice — denied over a workspace
//! whose store holds no reading of it, permitted over the tree `witness`
//! measured. No case of this row can be one that never fires, since what
//! decides is the evidence and not the shape of the call (#134).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use regex_lite::Regex;

use crate::git;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::reading::Store;

/// Whether every hit of the row's pattern in `written` was captured by the
/// instrument `unless` names: the sentence at the top of this module.
pub(super) fn captured(
    manifest: &Manifest,
    root: &Path,
    unless: &str,
    regex: &Regex,
    written: &str,
    path: Option<&Path>,
) -> bool {
    let (Some(path), Some(instrument)) = (path, instrument_of(unless)) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(root.join(path)) else {
        return false;
    };
    // Every hit stands in the file already, counted: the text adds none
    // over what is on disk, so what fired is the tree's and not the call's.
    if super::rules::adds(regex, written, std::slice::from_ref(&text)) {
        return false;
    }
    let Some(krate) = manifest.crate_of(path) else {
        return false;
    };
    let fresh = match (
        Store::of_workspace(root).read(instrument, &krate.name),
        Snapshot::disk_over(root, &krate.path()).and_then(|now| now.key(&krate.path())),
    ) {
        (Ok(Some(reading)), Ok(tree)) => !reading.key().is_empty() && reading.key() == tree,
        _ => false,
    };
    fresh
        || accepted(root, instrument, &krate.name, regex, written)
        || carried(
            root,
            instrument,
            &krate.name,
            &krate.path(),
            regex,
            written,
            path,
        )
}

/// Whether every hit in `written` is a verdict `--accept` recorded writing
/// ([`crate::witness::record_accepted`]), in this workspace's store or the
/// store of another worktree of the clone, where `worktree retire` lands a
/// branch's. The record names a verdict by its block, the `php` fence
/// through the `expect` fence that pins it ([`crate::witness::fence::verdict`]),
/// so a run narrowed to a file or an item, which writes no reading of the
/// crate, leaves one as a crate's run does, and prose edited around the
/// block or a merge that moves the crate on leaves it standing. A verdict
/// typed by hand is a block no record names.
fn accepted(root: &Path, instrument: &str, scope: &str, regex: &Regex, written: &str) -> bool {
    let mut wanted = BTreeSet::new();
    for hit in regex.find_iter(written) {
        let line = written[..hit.start()].matches('\n').count();
        let Some(verdict) = crate::witness::fence::verdict(written, line) else {
            return false;
        };
        wanted.insert(verdict);
    }
    if wanted.is_empty() {
        return false;
    }
    let scope = crate::witness::accepted_scope(scope);
    let mut found = BTreeSet::new();
    for store in stores(root) {
        match store.measured_over(instrument, &scope, &wanted) {
            Ok(those) => found.extend(those),
            Err(_) => return false,
        }
    }
    found == wanted
}

/// How many of the commits that last changed a file are looked through for
/// one the instrument measured: `done` over a merge reaches the merged branch's
/// commits within a few, and the bound keeps a file with a long history
/// from costing one `ls-tree` per commit it ever had.
const COMMITS: usize = 64;

/// Whether every hit in `written` stands, in the block around it, in the
/// file at `path` as it was at a commit whose sources of the crate at
/// `under` a reading of `instrument` was taken over, in this workspace's
/// store or the store of another worktree of the clone (#1448). The
/// evidence travels with the commits: a line `--accept` captured on a
/// branch is in the branch's commit, the commit's crate is what the
/// branch's reading measured, and the reading reaches the main checkout's
/// store at `worktree retire`, so a merge that also brings other changes to
/// the crate is still read as the branch's capture, and a block typed by hand
/// in a commit nothing measured is not. A block is the hit's line and every
/// line around it up to the nearest blank one, the rustdoc paragraph and
/// the item under it, so an edit to what the line stands in is a block no
/// commit carried. Anything this cannot read is no evidence.
fn carried(
    root: &Path,
    instrument: &str,
    scope: &str,
    under: &Path,
    regex: &Regex,
    written: &str,
    path: &Path,
) -> bool {
    let mut wanted = blocks(regex, written);
    if wanted.is_empty() {
        return false;
    }
    let Some(file) = path.to_str() else {
        return false;
    };
    let count = COMMITS.to_string();
    let Ok(log) = git::git(root, &["log", "--format=%H", "-n", &count, "--", file]) else {
        return false;
    };
    let mut commits = Vec::new();
    for commit in log.lines().filter(|c| !c.is_empty()) {
        let Ok(tree) = Snapshot::commit(root, commit).and_then(|then| then.key(under)) else {
            return false;
        };
        commits.push((commit, tree));
    }
    let trees: BTreeSet<String> = commits.iter().map(|(_, tree)| tree.clone()).collect();
    let mut measured = BTreeSet::new();
    for store in stores(root) {
        match store.measured_over(instrument, scope, &trees) {
            Ok(found) => measured.extend(found),
            Err(_) => return false,
        }
    }
    for (commit, tree) in &commits {
        if !measured.contains(tree) {
            continue;
        }
        let Ok(Some(then)) = git::show(root, commit, file) else {
            continue;
        };
        for block in blocks(regex, &then) {
            wanted.remove(&block);
        }
        if wanted.is_empty() {
            return true;
        }
    }
    false
}

/// This workspace's store, then the store of every other worktree of the
/// clone: a branch merged before its worktree is retired keeps its
/// readings in its own.
fn stores(root: &Path) -> Vec<Store> {
    let mut roots = vec![root.to_path_buf()];
    for (other, _) in crate::track::worktrees(root).unwrap_or_default() {
        if !roots.contains(&other) {
            roots.push(other);
        }
    }
    roots.iter().map(|r| Store::of_workspace(r)).collect()
}

/// The block around each hit of `regex` in `text`: its line and the lines
/// on either side up to the nearest blank one.
fn blocks(regex: &Regex, text: &str) -> BTreeSet<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut starts = Vec::with_capacity(lines.len());
    let mut at = 0;
    for line in &lines {
        starts.push(at);
        at += line.len() + 1;
    }
    let blank = |i: usize| lines[i].trim().is_empty();
    regex
        .find_iter(text)
        .filter_map(|hit| {
            let line = starts
                .partition_point(|s| *s <= hit.start())
                .checked_sub(1)?;
            let first = (0..line).rev().find(|i| blank(*i)).map_or(0, |i| i + 1);
            let last = (line + 1..lines.len())
                .find(|i| blank(*i))
                .unwrap_or(lines.len());
            Some(lines[first..last].join("\n"))
        })
        .collect()
}

/// The instrument a row's `unless` names: the first word of the phrase that
/// is neither `cargo`, nor `xtask`, nor a flag, so `"xtask witness
/// --accept"` names `witness`, the name its readings carry.
fn instrument_of(unless: &str) -> Option<&str> {
    unless
        .split_whitespace()
        .find(|word| !matches!(*word, "cargo" | "xtask") && !word.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::{blocks, instrument_of};

    #[test]
    fn a_hits_block_runs_to_the_blank_lines_around_it() {
        let regex = regex_lite::Regex::new(r"exit=\d+").unwrap();
        let text = "a\n\n/// x\n/// exit=3\nfn f() {}\n\nb exit=4\n";
        let found: Vec<String> = blocks(&regex, text).into_iter().collect();
        assert_eq!(found, ["/// x\n/// exit=3\nfn f() {}", "b exit=4"]);
        assert!(blocks(&regex, "nothing here").is_empty());
    }

    #[test]
    fn the_instrument_is_the_first_word_that_is_not_cargo_xtask_or_a_flag() {
        assert_eq!(instrument_of("xtask witness --accept"), Some("witness"));
        assert_eq!(
            instrument_of("cargo xtask witness --accept"),
            Some("witness")
        );
        assert_eq!(instrument_of("witness"), Some("witness"));
        assert_eq!(instrument_of("  --accept  "), None);
        assert_eq!(instrument_of(""), None);
    }
}
