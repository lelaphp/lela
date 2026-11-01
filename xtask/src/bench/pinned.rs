//! The pinned php a bar measures on this machine (#3799): on macOS the
//! keg `[oracle]` names, asserted as every lane asserts it; on Linux, php
//! built from the pinned tag by `scripts/oracle-build.py build <platform>
//! main`, which bench.yml's `baseline` job runs on dax-build (#3774). That
//! build is a performance baseline and not an oracle (#2253, #3741): it
//! is held to the md5 the recipe recorded beside it, its `pin.toml`, and
//! a bar is refused, naming the path and the md5 it expected, wherever
//! the binary is absent or another.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::oracle::{Build, Pin, Platform};

/// Where the recipe lands a build, `<platform>/<variant>/` under the
/// checkout (`out_dir`, `scripts/oracle-build.py`): the binary as `php`
/// and its table as `pin.toml`. The table's `path` is the install prefix
/// a runner would lay it at; nothing lays it there on dax-build, so the
/// bench takes it where the recipe lands it, and the runner keeps
/// `target/` from the `baseline` job to the `bench` job.
const LANDED: &str = "target/oracle";

/// The pinned php for a bar on this machine, or why it has none, the
/// bar's skip: [`Platform::this`] says which.
pub(super) fn php(root: &Path, pin: &Pin) -> Result<Result<PathBuf, String>> {
    match Platform::this() {
        Some(linux @ (Platform::LinuxX86_64 | Platform::LinuxAarch64)) => {
            let dir = root.join(LANDED).join(linux.key()).join("main");
            built(&dir, linux).map(Ok)
        }
        Some(Platform::DarwinArm64) | None => {
            if let Some(why) = pin.oracle.absent() {
                return Ok(Err(why));
            }
            pin.oracle().asserted()?;
            Ok(Ok(pin.oracle.path.clone()))
        }
    }
}

/// The php the recipe built for `platform` into `dir`, held to the md5 of
/// `platform`'s table in the `pin.toml` beside it.
fn built(dir: &Path, platform: Platform) -> Result<PathBuf> {
    let (php, table) = (dir.join("php"), dir.join("pin.toml"));
    let rebuild = format!(
        "bench.yml's baseline job builds it, `scripts/oracle-build.py build {} main`",
        platform.key()
    );
    let text = fs::read_to_string(&table).with_context(|| {
        format!(
            "no php built from the pinned tag is recorded at {}; {rebuild}",
            table.display()
        )
    })?;
    let mut tables: BTreeMap<String, Build> =
        toml::from_str(&text).with_context(|| format!("reading {}", table.display()))?;
    let Some(Build { md5, .. }) = tables.remove(platform.key()) else {
        bail!(
            "{} names no {} build; {rebuild}",
            table.display(),
            platform.key()
        );
    };
    let Ok(bytes) = fs::read(&php) else {
        bail!(
            "the php built from the pinned tag is not at {}, which {} expects at md5 {md5}; {rebuild}",
            php.display(),
            table.display()
        );
    };
    let actual = format!("{:x}", md5::compute(bytes));
    if actual != md5 {
        bail!(
            "{} has md5 {actual}, and {} expects md5 {md5}; {rebuild} again",
            php.display(),
            table.display()
        );
    }
    Ok(php)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Tree;

    /// On Linux a bar takes the php the recipe landed, held to the md5 its
    /// `pin.toml` records; absent, another or unrecorded, the bar is
    /// refused naming the path and the md5 it expected (#3799).
    #[test]
    fn the_build_from_the_tag_is_held_to_its_recorded_md5_and_refused_otherwise() {
        let tree = Tree::new("bench-pinned").unwrap();
        let (dir, linux) = (tree.path(), Platform::LinuxX86_64);
        let refused = |platform| format!("{:#}", built(dir, platform).unwrap_err());
        let table = dir.join("pin.toml").display().to_string();
        assert!(refused(linux).contains(&table), "{}", refused(linux));
        let md5 = format!("{:x}", md5::compute(b"php"));
        let row =
            format!("linux-x86_64 = {{ path = \"/opt/php\", md5 = \"{md5}\", image = \"i\" }}");
        tree.write("pin.toml", row).unwrap();
        let php = dir.join("php");
        let at = php.display().to_string();
        for (contents, said) in [(None, "is not at"), (Some("elf"), "has md5")] {
            if let Some(contents) = contents {
                tree.write("php", contents).unwrap();
            }
            let why = refused(linux);
            assert!(why.contains(&at) && why.contains(said), "{why}");
            assert!(why.contains(&format!("md5 {md5}")), "{why}");
        }
        tree.write("php", "php").unwrap();
        assert_eq!(built(dir, linux).unwrap(), php);
        let other = refused(Platform::LinuxAarch64);
        assert!(other.contains("names no linux-aarch64 build"), "{other}");
    }
}
