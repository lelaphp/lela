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

pub use crate::inputs::digest;
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
