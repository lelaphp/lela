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
//! already stands in the file as that instrument last saw it: every hit of
//! the pattern is in the file on disk, and the crate the file lies in has a
//! reading of the named instrument stamped with the crate's tree hash now.
//! `witness` constructs its reading after `--accept` has written, so the
//! line it captured is inside the tree the reading is over.
//!
//! **What stays denied.** A call at the hook that would put an `exit=` in a
//! file: what it writes is not in the file yet, so no reading covers it and
//! the denial stands whatever the store holds — which is the rule doing its
//! work, since the authoring it refuses happens there. A line left in the
//! tree by a hand the hook did not see: the crate has no reading of the
//! instrument, or one taken before the line was written, whose stamp is not
//! the tree's, so `permit --diff` and `done` deny it with the rule id.
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

use std::fs;
use std::path::Path;

use regex_lite::Regex;

use crate::git;
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
    if !regex
        .find_iter(written)
        .all(|hit| text.contains(hit.as_str()))
    {
        return false;
    }
    let Some(krate) = manifest.crate_of(path) else {
        return false;
    };
    let Ok(Some(reading)) = Store::of_workspace(root).read(instrument, &krate.name) else {
        return false;
    };
    let Ok(tree) = git::tree_hash(root, &krate.path()) else {
        return false;
    };
    !reading.tree().is_empty() && reading.tree() == tree
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
    use super::instrument_of;

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
