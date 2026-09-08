//! What the build records about the manifest it was compiled against
//! (harness-design.md §4.4): the digest of `seams.toml` as it stood when
//! this binary was built, handed to the crate as `XTASK_MANIFEST_DIGEST`.
//! `permit` compares it with the file on disk and refuses to judge when
//! they differ, so a build older than the manifest is the loud kind of
//! stale rather than the quiet kind that exits 0 and judges by rules that
//! have moved (#16). `src/built.rs` reads it and takes the same digest;
//! the pair is held by a test.

use std::path::Path;

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new(".."))
        .join("seams.toml");
    // The one input of this script: cargo re-runs it, and so rebuilds the
    // binary, when a person edits the manifest. Without this the cargo
    // candidate the hook closes with would carry a digest as stale as the
    // built ones and every candidate would refuse.
    println!("cargo::rerun-if-changed={}", manifest.display());
    // A build without the manifest beside it carries no digest, which
    // `built` reads as a build that cannot say which rules it judges by.
    let digest = std::fs::read(&manifest).map_or_else(
        |_| String::new(),
        |bytes| {
            let mut hash = sha1_smol::Sha1::new();
            hash.update(&bytes);
            hash.digest().to_string()
        },
    );
    println!("cargo::rustc-env=XTASK_MANIFEST_DIGEST={digest}");
}
