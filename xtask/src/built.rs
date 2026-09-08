//! The manifest this build was compiled against (harness-design.md §4.4).
//!
//! The fallback of the two hook adapters keys on an exit code: `permit`'s
//! exit is 0 whatever it decides, so a non-zero one is a build that never
//! judged, and the call goes to the next candidate. An exit code sees only
//! the loud staleness — a build that cannot parse the manifest in front of
//! it, the way one from before `[telemetry]` landed could not. A build the
//! manifest moved under in a way that still parses, a rule removed or a
//! pattern loosened, exits 0 and judges by rules that are no longer the
//! rules (#16).
//!
//! So the build records which manifest it was compiled against: `build.rs`
//! takes the digest of `seams.toml` and hands it over as
//! `XTASK_MANIFEST_DIGEST`. `judges_by` compares it with the file on disk,
//! and the quiet staleness becomes the loud kind the fallback already
//! walks. `cargo` closing the candidate list is what makes this safe: its
//! build re-runs `build.rs` when the manifest changes, so the candidate
//! that rebuilds always carries the digest of the file it is about to read.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The digest of `seams.toml` as it stood when this binary was compiled.
/// Empty for a build made without the manifest beside it, which is a build
/// that cannot say which rules it judges by.
pub const MANIFEST: &str = env!("XTASK_MANIFEST_DIGEST");

/// The digest `build.rs` takes over the manifest's bytes. The two are one
/// function in two compilations; the test
/// `this_build_carries_the_digest_of_the_manifest_it_was_compiled_against`
/// holds them to the same value.
pub fn digest(bytes: &[u8]) -> String {
    let mut hash = sha1_smol::Sha1::new();
    hash.update(bytes);
    hash.digest().to_string()
}

/// The manifest at `path` against the one this build was compiled from:
/// `Ok` when they are the same file, and an error naming both digests when
/// they are not. The caller refuses to judge on that error rather than
/// deciding by rules it was not built against, whichever way the decision
/// would fall: a stale build that happens to agree is still a build whose
/// agreement means nothing.
///
/// Any byte apart is apart, a comment included. A build cannot say which
/// differences change a judgement without reading rules it may be too old
/// to understand, which is the whole of what went wrong in #16; the cost
/// of the blunt answer is a rebuild, which the candidate that closes the
/// hook's list does anyway.
pub fn judges_by(path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let on_disk = digest(&bytes);
    if MANIFEST.is_empty() {
        bail!(
            "this build carries no manifest digest: it was compiled without seams.toml beside it \
             and cannot say which rules it judges by. {REBUILD}"
        );
    }
    if on_disk != MANIFEST {
        bail!(
            "this build was compiled against seams.toml {MANIFEST} and {} is {on_disk}: it would \
             judge by rules it was not built against. {REBUILD}",
            path.display()
        );
    }
    Ok(())
}

/// What a person or an agent does about it; the hook does it by walking to
/// the candidate that rebuilds.
const REBUILD: &str = "Rebuild it: `cargo build -p xtask`, or run it through `cargo run -p xtask`.";
