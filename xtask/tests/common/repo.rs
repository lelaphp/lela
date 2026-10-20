//! A scratch git repository holding the workspace's harness files, for
//! the tests that provoke readings.
//! Each file of `tests/common` is one topic: an integration test includes
//! the topics it calls by name, as `mod common { pub mod repo; }`, and
//! the unit tests under `src` include theirs once, in lib.rs, by `#[path]`.
//! A binary takes a topic whole and may call part of it.
#![allow(
    dead_code,
    reason = "a binary includes the topic whole and calls part of it"
)]

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use xtask::manifest::{self, Manifest};

/// A git repository at `dir`, rebuilt from scratch, holding the
/// workspace's manifest, its pin, the divergence registry and a README:
/// where a test provokes denials, so the readings land in its store and
/// not the workspace's. `permit --bench` builds the same repository for
/// the same reason, with the recipe stated there too.
pub fn scratch_workspace(root: &Path, dir: &Path) -> Result<()> {
    std::fs::remove_dir_all(dir).ok();
    std::fs::create_dir_all(dir.join("docs"))
        .with_context(|| format!("creating {}", dir.display()))?;
    let manifest = Manifest::load(root)?;
    for file in [
        manifest::FILE,
        manifest.oracle.pin.as_str(),
        "docs/divergence.toml",
        "README.md",
    ] {
        std::fs::copy(root.join(file), dir.join(file))
            .with_context(|| format!("copying {file}"))?;
    }
    git(dir, &["init", "-q"])?;
    git(dir, &["add", "."])?;
    git(
        dir,
        &[
            "-c",
            "user.name=xtask",
            "-c",
            "user.email=xtask@scratch",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
    )?;
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}
