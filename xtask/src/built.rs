//! What this build judges by, and whether it still holds (harness-design.md
//! §4.4).
//!
//! The fallback of the two hook adapters keys on an exit code: `permit`'s
//! exit is 0 whatever it decides, so a non-zero one is a build that never
//! judged, and the call goes to the next candidate. An exit code sees only
//! the loud staleness — a build that cannot parse the manifest in front of
//! it, the way one from before `[telemetry]` landed could not. A build that
//! is stale in a way which still parses, a rule removed or a pattern
//! loosened, exits 0 and judges by rules that are no longer the rules; a
//! build stale in its own code judges by a matcher that has since been
//! fixed, with no rule moved at all (harness revision 13).
//!
//! So the build records what it judges by: `build.rs` takes the digest of
//! `seams.toml`, the rules, and of `xtask/src`, the code that reads them,
//! and hands both over as `XTASK_MANIFEST_DIGEST` and
//! `XTASK_SOURCES_DIGEST`. `judges` compares them with the tree, and the
//! quiet staleness becomes the loud kind the fallback already walks. Cargo
//! closing the candidate list is what makes this safe: its build re-runs
//! `build.rs` when either input changes, so the candidate that rebuilds
//! always carries the digest of what it is about to read.
//!
//! The manifest is compared against the checkout the call belongs to,
//! since that is where the rules a build could have been compiled against
//! are: a fixture workspace inside it carries a manifest of its own that
//! judges the call and that no build is compiled against, so the
//! build that judges by it is held to the checkout's. The sources are
//! compared against the workspace this binary runs in ([`built_root`]),
//! the code it must have been built from: a binary cargo reused from
//! another checkout sharing the target directory is held to this one's
//! sources, and is stale where they differ.
//!
//! [`built_root`]: crate::built_root
//!
//! One manifest is not held that way: a worktree's. A call in another
//! checkout of the repository than the one this build was asked about is
//! judged by that checkout's rules, and a worktree laid before a
//! `seams.toml` change carries rules no build of the checkout is compiled
//! against. No rebuild of the checkout repairs that, so the answer is not
//! a refusal but [`Judge::Checkout`]: the worktree's own build judges the
//! call, and `permit` hands it over.
//!
//! A refusal says which of the two it is, since only one of them is a
//! judgement the harness is missing. A build apart from the tree is passed
//! over by one that rebuilds from it, so the call is judged and the
//! refusal is a step of the walk; a manifest no build can read ends the
//! walk with the call unjudged, which is the outage of harness revision
//! 11. `permit` records the first as a skip that names where the call went
//! and the second as a failure.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};

use crate::inputs::{manifest_digest, sources_digest};

/// The digest of `seams.toml` as it stood when this binary was compiled.
/// Empty for a build made without the manifest beside it, which is a build
/// that cannot say which rules it judges by.
pub const MANIFEST: &str = env!("XTASK_MANIFEST_DIGEST");
/// The digest of `xtask/src` as it stood when this binary was compiled.
pub const SOURCES: &str = env!("XTASK_SOURCES_DIGEST");

/// Why a build will not judge one call, and whether the candidate after it
/// will. The hook walks its candidates and stops at the first that exits 0
/// (§4.4), so the two refusals are not one thing, and a walk recorded as
/// a failure is what came of spelling them as one.
#[derive(Debug)]
pub enum Refusal {
    /// This build is not the tree's: another manifest, other sources, or
    /// neither beside it when it was compiled. The candidate that closes
    /// the hook's list rebuilds from the tree, so the call goes on to a
    /// build that judges it, and this refusal decided nothing either way.
    Stale(anyhow::Error),
    /// What any build judges by cannot be read at all: the manifest is
    /// absent, or it is bytes no build of this harness parses. A rebuild
    /// reads the same bytes and refuses the same way, so the call reaches
    /// the end of the walk unjudged.
    Unreadable(anyhow::Error),
    /// The build of the checkout the call is in, which [`Judge::Checkout`]
    /// named, did not judge it: none of its candidates exited 0. Every
    /// candidate of this walk hands the call to that same build, so the
    /// call reaches the end of the walk unjudged, never allowed.
    HandOff(anyhow::Error),
}

/// Which build judges one call, where [`judges`] does not refuse.
#[derive(Debug, PartialEq, Eq)]
pub enum Judge {
    /// This one: the manifest in front of it and its own sources are what
    /// it was compiled against.
    This,
    /// The build of this checkout, another checkout of the repository than
    /// the one this build was asked about, whose manifest is not the one
    /// this build was compiled against. Each tree is judged by the
    /// rules it carries, so that build judges the call, against its own
    /// tree, where the checkout it is asked about is its root and it never
    /// hands the call on again.
    Checkout(PathBuf),
}

impl Refusal {
    /// Whether the candidate after this one judges the call. A stale build
    /// hands it on; a manifest no build reads ends the walk.
    pub fn hands_the_call_on(&self) -> bool {
        matches!(self, Refusal::Stale(_))
    }

    /// Why the build refused, in the words the exit carries.
    pub fn why(&self) -> &anyhow::Error {
        match self {
            Refusal::Stale(why) | Refusal::Unreadable(why) | Refusal::HandOff(why) => why,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.why())
    }
}

impl From<Refusal> for anyhow::Error {
    fn from(refusal: Refusal) -> anyhow::Error {
        match refusal {
            Refusal::Stale(why) | Refusal::Unreadable(why) | Refusal::HandOff(why) => why,
        }
    }
}

/// This build against what it judges by: the manifest of `checkout`, the
/// one the call belongs to, and its own sources in `built_root`, the
/// workspace it was compiled in.
/// [`Judge::This`] when both are what it was compiled against, and a
/// [`Refusal`] naming the one that is not when either has moved; except
/// that a manifest apart in a `checkout` that is not `root`, the checkout
/// this build was asked about, is [`Judge::Checkout`], since that
/// checkout's own build is compiled against it. The caller refuses to
/// judge on that rather than deciding by rules or with code it was not
/// built from, whichever way the decision would fall: a stale build that
/// happens to agree is still a build whose agreement means nothing.
///
/// Any byte apart is apart, a comment included. A build cannot say which
/// differences change a judgement without reading rules it may be too old
/// to understand, which is the whole of what went wrong in the outage of
/// harness revision 11; the cost of the blunt answer is a rebuild, which
/// the candidate that closes the hook's list does anyway. That is why a
/// digest apart is [`Refusal::Stale`] and hands the call on, while a
/// digest that cannot be taken at all is [`Refusal::Unreadable`]: no
/// rebuild repairs a manifest that is not there.
pub fn judges(checkout: &Path, root: &Path, built_root: &Path) -> Result<Judge, Refusal> {
    let manifest = manifest_digest(checkout)
        .with_context(|| {
            format!(
                "reading the manifest {}",
                checkout.join(crate::inputs::MANIFEST_FILE).display()
            )
        })
        .map_err(Refusal::Unreadable)?;
    let sources = sources_digest(built_root)
        .with_context(|| {
            format!(
                "reading the sources {}",
                built_root.join(crate::inputs::SOURCES_DIR).display()
            )
        })
        .map_err(Refusal::Unreadable)?;
    if MANIFEST.is_empty() || SOURCES.is_empty() {
        return Err(Refusal::Stale(anyhow!(
            "this build carries no digest of what it judges by: it was compiled without \
             seams.toml or xtask/src beside it and cannot say what its rules are. {REBUILD}"
        )));
    }
    if manifest != MANIFEST && !same_directory(checkout, root) {
        return Ok(Judge::Checkout(checkout.to_path_buf()));
    }
    if manifest != MANIFEST {
        return Err(Refusal::Stale(anyhow!(
            "this build was compiled against seams.toml {MANIFEST} and {} is {manifest}: it \
             would judge by rules it was not built against. {REBUILD}",
            checkout.join(crate::inputs::MANIFEST_FILE).display()
        )));
    }
    if sources != SOURCES {
        return Err(Refusal::Stale(anyhow!(
            "this build was compiled from xtask/src {SOURCES} and {} is {sources}: it would \
             judge with code the harness has since changed. {REBUILD}",
            built_root.join(crate::inputs::SOURCES_DIR).display()
        )));
    }
    Ok(Judge::This)
}

/// Whether two paths name one directory, however each is spelled: `root`
/// is as the command line gave it and `checkout` as the call resolved it.
pub(crate) fn same_directory(one: &Path, other: &Path) -> bool {
    match (one.canonicalize(), other.canonicalize()) {
        (Ok(one), Ok(other)) => one == other,
        _ => one == other,
    }
}

/// What a person or an agent does about it; the hook does it by walking to
/// the candidate that rebuilds.
const REBUILD: &str = "Rebuild it: `cargo build -p xtask`, or run it through `cargo run -p xtask`.";

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Judge, MANIFEST, Refusal, SOURCES, judges};
    use crate::built_root;
    use crate::inputs::{SOURCES_DIR, manifest_digest, source_files, sources_digest};
    use crate::scratch::Tree;

    fn scratch(name: &str) -> Tree {
        Tree::new(&format!("built-{name}")).unwrap()
    }

    /// `from` copied under `to`, every file and directory.
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    /// This build carries the digest of what it judges by, the manifest
    /// beside it and its own sources, each taken by `build.rs` and by
    /// `inputs` the same way, so a build standing over its own workspace
    /// judges.
    #[test]
    fn this_build_carries_the_digest_of_what_it_was_compiled_against() {
        let workspace = built_root();
        assert_eq!(
            MANIFEST,
            manifest_digest(&workspace).unwrap(),
            "build.rs and inputs take one digest of the manifest"
        );
        assert_eq!(
            SOURCES,
            sources_digest(&workspace).unwrap(),
            "and one of the harness's own sources"
        );
        assert_eq!(
            judges(&workspace, &workspace, &workspace).unwrap(),
            Judge::This
        );
    }

    /// A build stale in its own code judges by a matcher that has since
    /// been fixed, with no rule moved at all (harness revision 13): the
    /// half an exit code and a manifest digest both miss.
    #[test]
    fn a_build_the_harnesss_own_sources_moved_under_refuses_too() {
        let workspace = built_root();
        let root = scratch("stale-sources");
        let src = root.join("xtask/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("permit.rs"),
            "// a matcher of an earlier revision\n",
        )
        .unwrap();
        let refusal = judges(&workspace, &workspace, &root).unwrap_err();
        assert!(
            refusal.hands_the_call_on(),
            "a rebuild judges the call: {refusal}"
        );
        let err = refusal.to_string();
        assert!(err.contains("xtask/src"), "{err}");
        assert!(err.contains(SOURCES), "the digest compiled in: {err}");
        assert!(err.contains("cargo build -p xtask"), "the repair: {err}");
        // The manifest is the workspace's, and it is not what moved.
        assert_eq!(
            judges(&workspace, &workspace, &workspace).unwrap(),
            Judge::This
        );
    }

    /// The digest is of the files the crate is compiled from, `*.rs` under
    /// `xtask/src`, and of nothing else there: an editor's swap file
    /// or a `.DS_Store` left beside the sources is not a change to the
    /// code, so a built candidate over that tree still judges. A new `.rs`
    /// file is one, and it is in the list `build.rs` watches, so cargo
    /// rebuilds for it.
    #[test]
    fn a_file_that_is_not_rust_under_the_sources_does_not_move_the_digest() {
        let workspace = built_root();
        let root = scratch("sources-not-rust");
        let src = root.join("xtask/src");
        copy_tree(&workspace.join(SOURCES_DIR), &src);
        assert_eq!(
            sources_digest(&root).unwrap(),
            SOURCES,
            "a copy of the sources digests as the build did"
        );
        let before = source_files(&root).unwrap();
        std::fs::write(src.join(".DS_Store"), b"   Bud1").unwrap();
        std::fs::write(src.join(".permit.rs.swp"), "an editor's swap file").unwrap();
        std::fs::write(src.join("phpt/notes.md"), "a note").unwrap();
        assert_eq!(
            sources_digest(&root).unwrap(),
            SOURCES,
            "and still does with files the crate is not compiled from beside them"
        );
        assert_eq!(
            source_files(&root).unwrap(),
            before,
            "which build.rs does not watch either"
        );
        assert_eq!(judges(&workspace, &workspace, &root).unwrap(), Judge::This);

        std::fs::write(
            src.join("phpt/notes.rs"),
            "// a module the build has not seen\n",
        )
        .unwrap();
        assert_ne!(
            sources_digest(&root).unwrap(),
            SOURCES,
            "a new .rs file moves the digest"
        );
        assert!(
            source_files(&root)
                .unwrap()
                .contains(&src.join("phpt/notes.rs")),
            "and is in the list build.rs watches"
        );
        judges(&workspace, &workspace, &root).unwrap_err();
    }

    /// The two refusals are told apart by whether a rebuild repairs them.
    /// A digest apart from the tree is [`Refusal::Stale`]: the
    /// candidate that closes the hook's list rebuilds and judges the call.
    /// A manifest that is not there at all is [`Refusal::Unreadable`]:
    /// every candidate reads the same absence, so the walk ends with the
    /// call unjudged.
    #[test]
    fn a_digest_apart_hands_the_call_on_and_a_manifest_that_is_not_there_ends_the_walk() {
        let workspace = built_root();
        let root = scratch("refusal-kinds");
        std::fs::write(root.join("seams.toml"), "[oracle]\npin = \"oracle.toml\"\n").unwrap();
        let apart = judges(&root, &root, &workspace).unwrap_err();
        assert!(matches!(apart, Refusal::Stale(_)), "{apart}");
        assert!(apart.hands_the_call_on(), "{apart}");

        let empty = scratch("refusal-no-manifest");
        let absent = judges(&empty, &empty, &workspace).unwrap_err();
        assert!(matches!(absent, Refusal::Unreadable(_)), "{absent}");
        assert!(
            !absent.hands_the_call_on(),
            "no rebuild writes the manifest: {absent}"
        );
        assert!(absent.to_string().contains("seams.toml"), "{absent}");
    }

    /// A manifest apart in another checkout than the one this build was
    /// asked about is no refusal: that checkout's own build is
    /// compiled against it, so the answer names the checkout whose build
    /// judges the call. The same manifest in the checkout asked about is
    /// the refusal it was, since there the build that rebuilds is this
    /// checkout's and judges by it.
    #[test]
    fn a_manifest_apart_in_another_checkout_names_that_checkouts_build() {
        let workspace = built_root();
        let other = scratch("another-checkout");
        std::fs::write(
            other.join("seams.toml"),
            "[oracle]\npin = \"oracle.toml\"\n",
        )
        .unwrap();
        assert_eq!(
            judges(&other, &workspace, &workspace).unwrap(),
            Judge::Checkout(other.to_path_buf())
        );
        let asked = judges(&other, &other, &workspace).unwrap_err();
        assert!(matches!(asked, Refusal::Stale(_)), "{asked}");

        // The manifest this build was compiled against, in another
        // checkout, is judged here as it was.
        std::fs::copy(workspace.join("seams.toml"), other.join("seams.toml")).unwrap();
        assert_eq!(judges(&other, &workspace, &workspace).unwrap(), Judge::This);
    }
}
