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
pub fn source_files(root: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    collect(&root.join(SOURCES_DIR), &mut files)?;
    files.sort();
    Ok(files)
}

/// Every file under `dir`, in no order.
fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> io::Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}
