//! The cargo profiles a lane's binaries are built in (harness-design.md
//! §7, revision 82; #1994): the one statement of each profile's name and
//! the directory cargo builds it into, of the order the phpt lane looks
//! for the engine's binary in, and of the profile the correctness lanes
//! run, [`LANE`]. `Runner::resolve`, the cargo line a stale binary's skip
//! names, and [`build`], which builds every binary the harness serves
//! through, all read it, and none spells a profile.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// One profile of the workspace's `Cargo.toml` a lane's binary is built in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    /// `dev`, cargo's default, built into `debug/`: no optimisation, with
    /// debug assertions and overflow checks.
    Dev,
    /// `release`, the bench lane's and the shipped binaries' build: thin
    /// LTO over one codegen unit, without either check.
    Release,
    /// `release-da`, every correctness lane's build: release's
    /// optimisation with debug assertions and overflow checks, without
    /// thin LTO or a single codegen unit.
    ReleaseDa,
}

/// The profile every correctness lane runs: witness, the per-commit phpt
/// lane, `Zend/tests`, `tests/*` and the nightly full run (revision 82).
pub(crate) const LANE: Profile = Profile::ReleaseDa;

/// The order the phpt lane looks for the engine's binary in, the lane's
/// own build first: a binary the tree has moved past is passed over for
/// the next, so a machine that built one profile at the tree's tip runs
/// that one, and one that built the lane's runs it before any other.
pub(crate) const PREFERRED: [Profile; 3] = [LANE, Profile::Release, Profile::Dev];

impl Profile {
    /// The name `cargo --profile` takes.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Profile::Dev => "dev",
            Profile::Release => "release",
            Profile::ReleaseDa => "release-da",
        }
    }

    /// The directory under the target directory cargo builds the profile
    /// into: its name, but `debug` for `dev`.
    pub(crate) fn dir(self) -> &'static str {
        match self {
            Profile::Dev => "debug",
            other => other.name(),
        }
    }

    /// Where cargo puts `binary` built in this profile, under `target`.
    pub(crate) fn binary(self, target: &Path, binary: &str) -> PathBuf {
        target.join(self.dir()).join(binary)
    }
}

/// `cargo build --profile <the lanes'> -p <package> --bin <binary>`,
/// with `--features <features>` where there are any, in `root`, and where
/// cargo put the binary: under `into` where it is named, a target
/// directory of the build's own ([`featured`]), else the target directory
/// cargo was told of, or `root`'s own. Every binary the harness builds,
/// `php-linked` and the hosts' own, is built in the lanes' profile,
/// whatever profile the harness itself was built in (harness revision 92,
/// #2217): a reading's key covers the source and not the profile, so one
/// key is read off one build, and the `php` the witness lane builds is the
/// one the phpt lane looks for first. The one binary built otherwise is a
/// bench bar's arm, in the bench lane's profile ([`released`]).
pub(crate) fn build(
    root: &Path,
    package: &str,
    binary: &str,
    features: Option<&str>,
    into: Option<&Path>,
) -> Result<PathBuf> {
    built(root, LANE, (package, binary), features, into)
}

/// The same in `release`, the bench lane's profile: what a bar's arm of
/// the engine's own runs, never a correctness lane's build (#2351); under
/// `into` as [`build`]'s, the engine with the tier's feature beside the
/// lanes' build of it ([`featured`]).
pub(crate) fn released(
    root: &Path,
    (package, binary): (&str, &str),
    features: Option<&str>,
    into: Option<&Path>,
) -> Result<PathBuf> {
    built(root, Profile::Release, (package, binary), features, into)
}

/// `binary` of `package` built in `profile`: the cargo line [`build`] and
/// [`released`] each run in their profile, and where cargo put the binary.
fn built(
    root: &Path,
    profile: Profile,
    (package, binary): (&str, &str),
    features: Option<&str>,
    into: Option<&Path>,
) -> Result<PathBuf> {
    let status = Command::new(cargo())
        .args([
            "build",
            "--quiet",
            "--profile",
            profile.name(),
            "--package",
            package,
            "--bin",
            binary,
        ])
        .args(features.into_iter().flat_map(|names| ["--features", names]))
        .args(
            into.into_iter()
                .flat_map(|dir| ["--target-dir".as_ref(), dir.as_os_str()]),
        )
        .current_dir(root)
        .status()
        .with_context(|| format!("running cargo to build {binary}"))?;
    if !status.success() {
        bail!(
            "cargo could not build {binary} ({status}); its errors are on the harness's second stream"
        );
    }
    let target = into.map_or_else(
        || {
            std::env::var_os("CARGO_TARGET_DIR")
                .map_or_else(|| root.join("target"), |dir| root.join(dir))
        },
        Path::to_path_buf,
    );
    let built = profile.binary(&target, binary);
    if !built.is_file() {
        bail!("cargo built {binary} and it is not at {}", built.display());
    }
    Ok(built)
}

/// Where the engine turning the bundle's `feature` on is built: a target
/// directory of its own under `root`'s, so `target/<profile>/php` stays
/// the binary under test's default build (#2029).
pub(crate) fn featured(root: &Path, feature: &str) -> PathBuf {
    root.join("target").join("featured").join(feature)
}

/// The cargo the harness builds with: the one `CARGO` names, which
/// `cargo xtask` sets, else the one on `PATH`.
fn cargo() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// The arguments of the cargo line that builds every host of
/// [`super::hosts::HOSTS`] in the lanes' profile, their crates by package
/// ([`super::engine_build`] says why by package).
pub(crate) fn hosts() -> Vec<String> {
    let mut args = ["build", "--profile", LANE.name()]
        .map(str::to_owned)
        .to_vec();
    for host in super::hosts::HOSTS {
        args.extend(["-p".to_owned(), host.package()]);
    }
    args
}

/// Every host built in `root` by [`hosts`]' line, what cargo prints on
/// the harness's streams.
pub(crate) fn build_hosts(root: &Path) -> Result<()> {
    let status = Command::new(cargo())
        .args(hosts())
        .current_dir(root)
        .status()
        .context("running cargo to build the hosts")?;
    if !status.success() {
        bail!(
            "cargo could not build the hosts in {} ({status})",
            root.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> PathBuf {
        crate::package_dir()
            .parent()
            .expect("xtask sits in the workspace")
            .to_path_buf()
    }

    /// Each profile but `dev` is a table of the workspace's `Cargo.toml`,
    /// and the lane's inherits release with both checks on and neither of
    /// release's two link-time settings: the statement here and the one
    /// cargo reads are one profile. The negative control is the bench's
    /// own profile, which turns neither check on.
    #[test]
    fn the_lanes_profile_is_release_with_both_checks_and_without_thin_lto() {
        let text = std::fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
        let manifest: toml::Table = text.parse().unwrap();
        let profiles = manifest["profile"].as_table().unwrap();
        for profile in PREFERRED {
            if profile != Profile::Dev {
                assert!(
                    profiles.contains_key(profile.name()),
                    "Cargo.toml has no [profile.{}]",
                    profile.name()
                );
            }
        }
        let lane = profiles[LANE.name()].as_table().unwrap();
        assert_eq!(lane["inherits"].as_str(), Some("release"));
        assert_eq!(lane["debug-assertions"].as_bool(), Some(true));
        assert_eq!(lane["overflow-checks"].as_bool(), Some(true));
        assert_eq!(lane["lto"].as_bool(), Some(false));
        assert_ne!(lane["codegen-units"].as_integer(), Some(1));
        let release = profiles[Profile::Release.name()].as_table().unwrap();
        assert!(!release.contains_key("debug-assertions"));
        assert!(!release.contains_key("overflow-checks"));
    }

    /// The oracle job builds and runs every binary its correctness lanes
    /// read in the lane's profile, and none in plain release: each cargo
    /// command of the job, its continuation lines joined, names
    /// `--profile` followed by [`LANE`]'s name, and none names `--release`
    /// or runs `cargo xtask`, which builds the harness in `dev`. The job is
    /// the lines from its key to the next job's key at the same indent, and
    /// comments are read past, so prose that cites a flag does not stand in
    /// for a command. Three commands build: the harness (`XTASK`), the
    /// hosts and the tree's test.
    #[test]
    fn the_oracle_job_builds_its_binaries_in_the_lanes_profile() {
        let text =
            std::fs::read_to_string(workspace().join(".github/workflows/dashboard.yml")).unwrap();
        let mut lines = text.lines().skip_while(|line| *line != "  oracle:");
        assert_eq!(
            lines.next(),
            Some("  oracle:"),
            "dashboard.yml has an oracle job"
        );
        let job: Vec<&str> = lines
            .take_while(|line| {
                let key = line.strip_prefix("  ").unwrap_or_default();
                !(key.ends_with(':') && key.chars().next().is_some_and(|c| c.is_ascii_lowercase()))
            })
            .map(str::trim_start)
            .filter(|line| !line.starts_with('#'))
            .collect();
        let mut commands = Vec::new();
        let mut open = String::new();
        for line in job {
            open.push_str(line.trim_end_matches('\\'));
            open.push(' ');
            if !line.ends_with('\\') {
                commands.push(std::mem::take(&mut open));
            }
        }
        let mut cargo = 0;
        for command in commands.iter().filter(|c| c.contains("cargo ")) {
            let words: Vec<&str> = command.split_whitespace().collect();
            assert!(!words.contains(&"--release"), "the oracle job: {command}");
            assert!(
                !words.windows(2).any(|w| w == ["cargo", "xtask"]),
                "the oracle job runs `$XTASK`, not `cargo xtask`: {command}"
            );
            assert!(
                words.windows(2).any(|w| w == ["--profile", LANE.name()]),
                "the oracle job's cargo commands name `--profile {}`: {command}",
                LANE.name()
            );
            cargo += 1;
        }
        assert_eq!(
            cargo, 3,
            "the harness, the hosts and the tree's test: {commands:#?}"
        );
    }

    /// What the harness builds is built in the lanes' profile whatever
    /// profile the harness was built in (revision 92, #2217): this test
    /// runs in a dev build, and a binary it builds lands under the lanes'
    /// directory and not under `debug/`.
    #[test]
    fn a_binary_the_harness_builds_is_built_in_the_lanes_profile() {
        let root = crate::scratch::Tree::new("profile-build").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"probe\"\npath = \"main.rs\"\n\n[profile.release-da]\ninherits = \"release\"\ndebug-assertions = true\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        let built = build(&root, "probe", "probe", None, None).unwrap();
        assert!(built.is_file(), "{}", built.display());
        assert_eq!(
            built.parent().and_then(Path::file_name),
            Some(std::ffi::OsStr::new(LANE.dir())),
            "{}",
            built.display()
        );
    }

    /// The lanes' build is looked for first, and `dev`'s directory is
    /// cargo's `debug`.
    #[test]
    fn the_lanes_build_is_looked_for_first() {
        assert_eq!(PREFERRED[0], LANE);
        assert_eq!(Profile::Dev.dir(), "debug");
        assert_eq!(LANE.dir(), LANE.name());
    }
}
