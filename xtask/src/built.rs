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
//! fixed, with no rule moved at all (#16 item 1 and its residue).
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
//! The manifest is compared against the workspace the call belongs to,
//! since that is where the rules that would apply are; the sources against
//! the workspace this binary was built in, since that is the code it was
//! built from, and one binary cannot run another checkout's.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::inputs::{manifest_digest, sources_digest};

/// The digest of `seams.toml` as it stood when this binary was compiled.
/// Empty for a build made without the manifest beside it, which is a build
/// that cannot say which rules it judges by.
pub const MANIFEST: &str = env!("XTASK_MANIFEST_DIGEST");
/// The digest of `xtask/src` as it stood when this binary was compiled.
pub const SOURCES: &str = env!("XTASK_SOURCES_DIGEST");

/// This build against what it judges by: the manifest of `workspace`, and
/// its own sources in `built_root`, the workspace it was compiled in.
/// `Ok` when both are what it was compiled against, and an error naming the
/// one that is not when either has moved. The caller refuses to judge on
/// that error rather than deciding by rules or with code it was not built
/// from, whichever way the decision would fall: a stale build that happens
/// to agree is still a build whose agreement means nothing.
///
/// Any byte apart is apart, a comment included. A build cannot say which
/// differences change a judgement without reading rules it may be too old
/// to understand, which is the whole of what went wrong in #16; the cost of
/// the blunt answer is a rebuild, which the candidate that closes the
/// hook's list does anyway.
pub fn judges(workspace: &Path, built_root: &Path) -> Result<()> {
    let manifest = manifest_digest(workspace)
        .with_context(|| format!("reading the manifest of {}", workspace.display()))?;
    let sources = sources_digest(built_root)
        .with_context(|| format!("reading the sources of {}", built_root.display()))?;
    if MANIFEST.is_empty() || SOURCES.is_empty() {
        bail!(
            "this build carries no digest of what it judges by: it was compiled without \
             seams.toml or xtask/src beside it and cannot say what its rules are. {REBUILD}"
        );
    }
    if manifest != MANIFEST {
        bail!(
            "this build was compiled against seams.toml {MANIFEST} and {} is {manifest}: it \
             would judge by rules it was not built against. {REBUILD}",
            workspace.join(crate::inputs::MANIFEST_FILE).display()
        );
    }
    if sources != SOURCES {
        bail!(
            "this build was compiled from xtask/src {SOURCES} and {} is {sources}: it would \
             judge with code the harness has since changed. {REBUILD}",
            built_root.join(crate::inputs::SOURCES_DIR).display()
        );
    }
    Ok(())
}

/// What a person or an agent does about it; the hook does it by walking to
/// the candidate that rebuilds.
const REBUILD: &str = "Rebuild it: `cargo build -p xtask`, or run it through `cargo run -p xtask`.";

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{MANIFEST, SOURCES, judges};
    use crate::built_root;
    use crate::inputs::{SOURCES_DIR, manifest_digest, source_files, sources_digest};

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("xtask-built-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
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
        judges(&workspace, &workspace).unwrap();
    }

    /// A build stale in its own code judges by a matcher that has since
    /// been fixed, with no rule moved at all: the residue of #16 item 1,
    /// and the half an exit code and a manifest digest both miss.
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
        let err = judges(&workspace, &root).unwrap_err().to_string();
        assert!(err.contains("xtask/src"), "{err}");
        assert!(err.contains(SOURCES), "the digest compiled in: {err}");
        assert!(err.contains("cargo build -p xtask"), "the repair: {err}");
        // The manifest is the workspace's, and it is not what moved.
        judges(&workspace, &workspace).unwrap();
    }

    /// The digest is of the files the crate is compiled from, `*.rs` under
    /// `xtask/src`, and of nothing else there (#48): an editor's swap file
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
        judges(&workspace, &root).unwrap();

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
        judges(&workspace, &root).unwrap_err();
    }
}
