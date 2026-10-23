// What a build of the harness judges by, digested (harness-design.md §4.4,
// #16): the seam manifest, and `xtask`'s own sources. `build.rs` includes
// this file textually and writes each digest into the binary; `built`
// compares them with the tree at run time. One definition, two
// compilations, so the digest taken at build time and the one taken at run
// time cannot drift apart; the file names nothing outside `std` and
// `sha1_smol` for that reason.

use std::fs;
use std::io;
use std::path::Path;

/// Where the manifest is, relative to the workspace root.
pub const MANIFEST_FILE: &str = "seams.toml";
/// Where the harness's own sources are, relative to the workspace root.
pub const SOURCES_DIR: &str = "xtask/src";

/// The digest of some bytes: sha1, as `git::blob_id` takes one, without the
/// blob header, since nothing here is a git object.
pub fn digest(bytes: &[u8]) -> String {
    let mut hash = sha1_smol::Sha1::new();
    hash.update(bytes);
    hash.digest().to_string()
}

/// The digest of the manifest at `root`: the rules a build judges by.
pub fn manifest_digest(root: &Path) -> io::Result<String> {
    Ok(digest(&fs::read(root.join(MANIFEST_FILE))?))
}

/// The digest of `xtask`'s own sources at `root`: the code that reads those
/// rules. `<digest> <path>` per file in path order, digested again, which is
/// the shape `git::tree_hash` uses for a crate's tree. The path is the one
/// relative to `root`, so a checkout is what it holds and not where it
/// stands: a clone at another path, or a worktree, digests the same.
pub fn sources_digest(root: &Path) -> io::Result<String> {
    let mut listing = String::new();
    for path in source_files(root)? {
        let bytes = fs::read(&path)?;
        let relative = path.strip_prefix(root).unwrap_or(&path);
        listing.push_str(&digest(&bytes));
        listing.push(' ');
        listing.push_str(&relative.to_string_lossy());
        listing.push('\n');
    }
    Ok(digest(listing.as_bytes()))
}

/// The harness's own source files at `root`, in path order: what
/// `sources_digest` digests, and what `build.rs` asks cargo to watch. A
/// directory that is not there holds no files, which a build made outside
/// its workspace reads as a digest it cannot match.
///
/// The files are the ones the crate is compiled from, `*.rs` under
/// `SOURCES_DIR`, and nothing else there: an editor's swap file or a
/// `.DS_Store` left beside them is not a change to the code, and before this
/// rule it moved the digest after the build, so every built candidate
/// refused until cargo rebuilt (#48). The rule is stated here once, for both
/// compilations. It is an extension filter and not a git call, since this
/// file names nothing outside `std` and `sha1_smol`; a `.rs` file git ignores
/// is still compiled, so it is still digested.
pub fn source_files(root: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    collect(&root.join(SOURCES_DIR), &mut files)?;
    files.sort();
    Ok(files)
}

/// Whether `path` is one the crate is compiled from.
fn is_source(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "rs")
}

/// Every source under `dir`, in no order.
fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> io::Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if is_source(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// The files of the workspace the harness is written in that its own tests
/// read: dax-php's pin, glossary, map, readme and divergence registry.
/// Where one is absent, as in a repository that copied `xtask/` in, the
/// tests that read them are not built: `build.rs` leaves `cfg(tracked)`
/// unset and names the absent files, and `test` counts what it left out
/// (#3188). libtest has no skip at run time, so the build leaves them out.
pub const TRACKED: [&str; 5] = [
    "oracle.toml",
    "CONTEXT.md",
    "AGENTS.md",
    "README.md",
    "docs/divergence.toml",
];

/// Which of [`TRACKED`] the workspace at `root` lacks, as the sentence the
/// build's warning and `test`'s reading both say; none where it carries
/// every one.
pub fn untracked(root: &Path) -> Option<String> {
    let absent: Vec<&str> = TRACKED
        .into_iter()
        .filter(|file| !root.join(file).is_file())
        .collect();
    (!absent.is_empty()).then(|| format!("{} has no {}", root.display(), absent.join(", ")))
}
