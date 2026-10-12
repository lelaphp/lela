//! What the build records about what it judges by (harness-design.md §4.4):
//! the digest of `seams.toml`, the rules, and of the `.rs` files under
//! `xtask/src`, the code that reads them, as they stood when this binary was
//! built, handed to the crate as `XTASK_MANIFEST_DIGEST` and
//! `XTASK_SOURCES_DIGEST`; and the cfg set of the target it builds for,
//! in `target.cfg` under `OUT_DIR`, which witness coverage reads (#1068).
//! `permit` compares
//! them with the tree and refuses to judge when either differs, so a build
//! older than what it judges by is the loud kind of stale rather than the
//! quiet kind that exits 0 and judges by rules, or with code, that have
//! moved (#16 item 1 and its residue).
//!
//! `src/inputs.rs` is included rather than imported: `build.rs` cannot
//! depend on the lib it builds, and one definition of the digest in two
//! compilations is what keeps the value taken here and the value taken at
//! run time the same.

/// The digests themselves, one definition shared with the lib; the `//!` of
/// that file needs a module to sit at the top of.
mod inputs {
    include!("src/inputs.rs");
}

use std::path::Path;

use inputs::{MANIFEST_FILE, SOURCES_DIR, manifest_digest, source_files, sources_digest};

fn main() {
    // Read when `build.rs` runs, not when it compiles: cargo runs one
    // compiled `build.rs` for every checkout sharing a target directory
    // (#2628).
    let dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let root = Path::new(&dir)
        .parent()
        .unwrap_or(Path::new(".."))
        .to_path_buf();
    // The inputs of this script: cargo re-runs it, and so rebuilds the
    // binary, when a person edits the manifest or anyone edits the harness.
    // Without them the cargo candidate the hook closes with would carry a
    // digest as stale as the built ones and every candidate would refuse.
    println!(
        "cargo::rerun-if-changed={}",
        root.join(MANIFEST_FILE).display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        root.join(SOURCES_DIR).display()
    );
    for path in &source_files(&root).expect("reading xtask/src") {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    // A build without them beside it carries no digest, which `built` reads
    // as a build that cannot say what it judges by.
    let manifest = manifest_digest(&root).unwrap_or_default();
    let code = sources_digest(&root).unwrap_or_default();
    println!("cargo::rustc-env=XTASK_MANIFEST_DIGEST={manifest}");
    println!("cargo::rustc-env=XTASK_SOURCES_DIGEST={code}");
    print_target_cfg();
}

/// The cfg set of the target this build is for, as rustc prints it under
/// the build's own flags, which is how cargo takes it: `src/cfg.rs` reads
/// the lines, so witness coverage holds a native to the `#[cfg]` of the
/// build that links it (#1068).
fn print_target_cfg() {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let target = std::env::var("TARGET").expect("cargo names the target");
    let mut command = std::process::Command::new(rustc);
    command.args(["--print", "cfg", "--target", &target]);
    if let Ok(flags) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        command.args(flags.split('\x1f').filter(|f| !f.is_empty()));
    }
    let printed = command.output().expect("running rustc --print cfg");
    assert!(
        printed.status.success(),
        "rustc --print cfg --target {target}: {}",
        String::from_utf8_lossy(&printed.stderr)
    );
    let out = std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR");
    std::fs::write(Path::new(&out).join("target.cfg"), printed.stdout).expect("writing target.cfg");
}
