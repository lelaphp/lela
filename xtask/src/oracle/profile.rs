//! The cargo profiles a lane's binaries are built in (harness-design.md
//! §7, revision 82): the one statement of each profile's name and
//! the directory cargo builds it into, of the order the phpt lane looks
//! for the engine's binary in, and of the profile the correctness lanes
//! run, [`LANE`]. `Runner::resolve`, the cargo line a stale binary's skip
//! names, and [`build`], which builds every binary the harness serves
//! through, all read it, and none spells a profile.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// One profile of the workspace's `Cargo.toml` a lane's binary is built in,
/// and every one it defines: a name on the command line that is none of
/// these is a usage error naming them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
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
/// `lela-linked` and the hosts' own, is built in the lanes' profile,
/// whatever profile the harness itself was built in (harness revision 92):
/// a reading's key covers the source and not the profile, so one
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
    let on = (features, true);
    built(root, LANE, (package, binary), on, (into, None))
}

/// The same in `release`, the bench lane's profile: what a bar's arm of
/// the engine's own runs, never a correctness lane's build; under
/// `into` as [`build`]'s, the engine with the tier's feature beside the
/// lanes' build of it ([`featured`]); and with `cfg` set where one is
/// named, a cfg only a bench build sets, added to the workspace's
/// rustflags ([`Added`]), which is why such a build goes into a directory
/// of its own.
pub(crate) fn released(
    root: &Path,
    (package, binary): (&str, &str),
    features: Option<&str>,
    into: (Option<&Path>, Option<&str>),
) -> Result<PathBuf> {
    let on = (features, true);
    built(root, Profile::Release, (package, binary), on, into)
}

/// [`released`] with the package's default features off, under `into`:
/// a host without the bundle's `jit`, which each host turns on by default,
/// the build G1 holds the tier's door cost against, in a target
/// directory of its own so the default one keeps the shipped build.
pub(crate) fn released_bare(
    root: &Path,
    (package, binary): (&str, &str),
    into: &Path,
) -> Result<PathBuf> {
    let (bare, into) = ((None, false), (Some(into), None));
    built(root, Profile::Release, (package, binary), bare, into)
}

/// `binary` of `package` built in `profile`: the cargo line [`build`] and
/// [`released`] each run in their profile, with the package's default
/// features where `defaults` says so, and where cargo put the binary.
fn built(
    root: &Path,
    profile: Profile,
    (package, binary): (&str, &str),
    (features, defaults): (Option<&str>, bool),
    (into, cfg): (Option<&Path>, Option<&str>),
) -> Result<PathBuf> {
    let mut cargo = Command::new(cargo());
    cargo.args([
        "build",
        "--quiet",
        "--profile",
        profile.name(),
        "--package",
        package,
        "--bin",
        binary,
    ]);
    let mut added = Added::inherited();
    if let Some(cfg) = cfg {
        added = added.with_cfg(cfg);
    }
    if !defaults {
        cargo.arg("--no-default-features");
    }
    cargo
        .args(features.into_iter().flat_map(|names| ["--features", names]))
        .args(
            into.into_iter()
                .flat_map(|dir| ["--target-dir".as_ref(), dir.as_os_str()]),
        );
    let what = format!("{binary}; its errors are on the harness's second stream");
    run(root, &mut cargo, &added, &what)?;
    let target = into.map_or_else(|| target(root), Path::to_path_buf);
    found(profile.binary(&target, binary))
}

/// The target directory cargo builds into in `root`: the one
/// `CARGO_TARGET_DIR` names, relative to `root`, else `root`'s own.
fn target(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), |dir| root.join(dir))
}

/// `built`, where cargo was to put a binary it built, refused where the
/// binary is not there.
pub(crate) fn found(built: PathBuf) -> Result<PathBuf> {
    if !built.is_file() {
        let binary = built.file_name().unwrap_or_default().to_string_lossy();
        bail!("cargo built {binary} and it is not at {}", built.display());
    }
    Ok(built)
}

/// Where the engine turning the bundle's `feature` on, or a build named
/// `feature` of other features than the default's, is built: a target
/// directory of its own under `root`'s, so `target/<profile>/lela` stays
/// the binary under test's default build.
pub(crate) fn featured(root: &Path, feature: &str) -> PathBuf {
    root.join("target").join("featured").join(feature)
}

/// The cargo the harness builds with: the one `CARGO` names, which
/// `cargo xtask` sets, else the one on `PATH`.
fn cargo() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// The arguments of the cargo line that builds every host of
/// [`super::hosts::HOSTS`] in `profile`, their crates by package
/// ([`super::engine_build`] says why by package).
pub(crate) fn hosts(profile: Profile) -> Vec<String> {
    let mut args = ["build", "--profile", profile.name()]
        .map(str::to_owned)
        .to_vec();
    for host in super::hosts::HOSTS {
        args.extend(["-p".to_owned(), host.package()]);
    }
    args
}

/// Every host built in `root` in `profile` by [`hosts`]' line, what cargo
/// prints on the harness's streams, and where cargo puts each one, in
/// [`super::hosts::HOSTS`]' order. A host not there is not refused here:
/// the phpt lane treats a host missing beside the binary under test as a
/// build the run is missing, and [`found`] refuses one where every host
/// must be there.
pub(crate) fn build_hosts(root: &Path, profile: Profile) -> Result<Vec<PathBuf>> {
    let mut cargo = Command::new(cargo());
    cargo.args(hosts(profile));
    let what = format!("the hosts in {}", root.display());
    run(root, &mut cargo, &Added::inherited(), &what)?;
    let target = target(root);
    let at = |host: &super::hosts::EngineHost| profile.binary(&target, host.binary);
    Ok(super::hosts::HOSTS.iter().map(at).collect())
}

/// `cargo`, a build line, run in `root` with `added` on it and the seat's
/// share of the machine, refused naming `what` it builds where cargo fails.
fn run(root: &Path, cargo: &mut Command, added: &Added, what: &str) -> Result<()> {
    added.onto(cargo);
    crate::seat::lend(cargo);
    let status = cargo
        .current_dir(root)
        .status()
        .with_context(|| format!("running cargo to build {what}"))?;
    if !status.success() {
        bail!("cargo could not build {what} ({status})");
    }
    Ok(())
}

/// The key of the workspace's rustflags table in `.cargo/config.toml`, as
/// a `--config` line names it: `cfg(all())` holds for every target, and
/// the table is the one that passes LLVM's
/// `--cgp-max-address-users-to-scan` to every profile (design §10
/// Profiles).
pub(crate) const RUSTFLAGS_TABLE: &str = "target.'cfg(all())'.rustflags";

/// What a build the harness runs adds to the workspace's rustflags table
/// ([`RUSTFLAGS_TABLE`]). Cargo joins a `--config` line's array for that
/// key to the file's, where a `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS`
/// variable replaces the table whole and the LLVM option with it; so what
/// the harness's own environment carries in either is taken into the
/// line, and neither variable is handed on. A build adds and never
/// replaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Added(Vec<String>);

impl Added {
    /// What the harness's environment carries, read as cargo reads it.
    pub(crate) fn inherited() -> Added {
        let var = |name| std::env::var(name).ok();
        Added::carried(
            var("CARGO_ENCODED_RUSTFLAGS").as_deref(),
            var("RUSTFLAGS").as_deref(),
        )
    }

    /// The flags of `encoded`, split at `0x1f`, where it is set, which
    /// cargo reads before `plain`; else `plain`'s, split at whitespace. A
    /// variable set and empty adds nothing, as an unset one does, where
    /// cargo would read it as an empty table and drop the option.
    fn carried(encoded: Option<&str>, plain: Option<&str>) -> Added {
        let flags: Vec<String> = match (encoded, plain) {
            (Some(encoded), _) => encoded.split('\x1f').map(str::to_owned).collect(),
            (None, Some(plain)) => plain.split_whitespace().map(str::to_owned).collect(),
            (None, None) => Vec::new(),
        };
        Added(flags.into_iter().filter(|f| !f.is_empty()).collect())
    }

    /// These and `--cfg <cfg>` after them.
    pub(crate) fn with_cfg(mut self, cfg: &str) -> Added {
        self.0.extend(["--cfg".to_owned(), cfg.to_owned()]);
        self
    }

    /// The `--config` value that adds these to the table, or none where
    /// nothing is added.
    pub(crate) fn line(&self) -> Option<String> {
        if self.0.is_empty() {
            return None;
        }
        let flags = self.0.iter().cloned().map(toml::Value::String).collect();
        Some(format!("{RUSTFLAGS_TABLE}={}", toml::Value::Array(flags)))
    }

    /// `cargo` handed [`Added::line`] and neither variable.
    pub(crate) fn onto(&self, cargo: &mut Command) {
        cargo.env_remove("RUSTFLAGS");
        cargo.env_remove("CARGO_ENCODED_RUSTFLAGS");
        if let Some(line) = self.line() {
            cargo.arg("--config").arg(line);
        }
    }
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
    #[cfg(tracked)]
    #[test]
    fn the_oracle_job_builds_its_binaries_in_the_lanes_profile() {
        let text =
            std::fs::read_to_string(workspace().join(".github/workflows/nightly.yml")).unwrap();
        let mut lines = text.lines().skip_while(|line| *line != "  oracle:");
        assert_eq!(
            lines.next(),
            Some("  oracle:"),
            "nightly.yml has an oracle job"
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
    /// profile the harness was built in (revision 92): this test
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

    /// The bare build turns the package's default features off and lands
    /// in the directory it is given, where the release build of the same
    /// package keeps them: the probe prints whether its default
    /// feature is on.
    #[test]
    fn the_bare_build_is_without_the_default_features_in_a_directory_of_its_own() {
        let root = crate::scratch::Tree::new("profile-bare").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"probe\"\npath = \"main.rs\"\n\n[features]\ndefault = [\"on\"]\non = []\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("main.rs"),
            "fn main() {\n    print!(\"{}\", cfg!(feature = \"on\"));\n}\n",
        )
        .unwrap();
        let printed = |built: &Path| {
            let out = Command::new(built).output().unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let into = root.join("target/featured/bare");
        let bare = released_bare(&root, ("probe", "probe"), &into).unwrap();
        assert!(bare.starts_with(&into), "{}", bare.display());
        assert_eq!(printed(&bare), "false");
        let full = released(&root, ("probe", "probe"), None, (None, None)).unwrap();
        assert_eq!(printed(&full), "true");
    }

    /// A cfg a bench build names reaches rustc beside the flags of the
    /// workspace's own rustflags table, which a `RUSTFLAGS` variable would
    /// have replaced: the probe's `build.rs` writes the flags
    /// cargo hands it, and the binary prints them.
    #[test]
    fn a_cfg_the_harness_names_is_added_to_the_workspaces_rustflags() {
        let root = crate::scratch::Tree::new("profile-rustflags").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[target.'cfg(all())']\nrustflags = [\"--cfg\", \"from_the_table\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("build.rs"),
            "fn main() {\n    let flags = std::env::var(\"CARGO_ENCODED_RUSTFLAGS\").unwrap();\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{out}/flags\"), flags.replace('\\x1f', \" \")).unwrap();\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    print!(\"{}\", include_str!(concat!(env!(\"OUT_DIR\"), \"/flags\")));\n}\n",
        )
        .unwrap();
        let built = released(
            &root,
            ("probe", "probe"),
            None,
            (None, Some("from_the_line")),
        )
        .unwrap();
        let out = Command::new(&built).output().unwrap();
        let printed = String::from_utf8(out.stdout).unwrap();
        assert!(
            printed.contains("--cfg from_the_table") && printed.contains("--cfg from_the_line"),
            "{printed}"
        );
    }

    /// What the environment carries is read as cargo reads it, the
    /// encoded variable before `RUSTFLAGS`, and goes into the `--config`
    /// line, which parses as the table's key; neither variable reaches
    /// the cargo the harness runs.
    #[test]
    fn what_the_environment_carries_is_added_and_handed_on_in_no_variable() {
        let added = Added::carried(Some("-C\x1fdebuginfo=1"), Some("--cfg ignored"));
        assert_eq!(added, Added(vec!["-C".into(), "debuginfo=1".into()]));
        let added = Added::carried(None, Some(" --cfg  a ")).with_cfg("dispatch=\"tailcall\"");
        let line = added.line().unwrap();
        let parsed: toml::Table = line.parse().unwrap();
        let flags = &parsed["target"]["cfg(all())"]["rustflags"];
        assert_eq!(
            flags.as_array().unwrap(),
            &["--cfg", "a", "--cfg", "dispatch=\"tailcall\""]
                .map(|f| toml::Value::String(f.into()))
        );
        assert_eq!(Added::default().line(), None);
        let mut cargo = Command::new("cargo");
        added.onto(&mut cargo);
        let removed: Vec<_> = cargo
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k)
            .collect();
        assert_eq!(removed, ["CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"]);
        let args: Vec<_> = cargo.get_args().collect();
        assert_eq!(args, [std::ffi::OsStr::new("--config"), line.as_ref()]);
    }

    /// The lane that holds the threaded dispatch adds its cfg as the
    /// harness does, through a `--config` line on each cargo line it runs,
    /// and sets no rustflags variable, which would replace the workspace's
    /// table: the line it names parses to the flags [`Added`]
    /// would add for the cfg.
    #[cfg(tracked)]
    #[test]
    fn the_threaded_dispatchs_lane_adds_its_cfg_through_a_config_line() {
        let text = std::fs::read_to_string(workspace().join("scripts/ci/tailcall.sh")).unwrap();
        let code: Vec<&str> = text
            .lines()
            .map(str::trim_start)
            .filter(|line| !line.starts_with('#'))
            .collect();
        for line in &code {
            assert!(
                !line.contains("RUSTFLAGS=") && !line.starts_with("export RUSTFLAGS"),
                "tailcall.sh sets a rustflags variable: {line}"
            );
        }
        let cfg = code
            .iter()
            .find_map(|line| line.strip_prefix("cfg=(--config \"")?.strip_suffix("\")"))
            .expect("tailcall.sh names its --config line as cfg=(--config \"...\")")
            .replace("\\\"", "\"");
        let parsed: toml::Table = cfg.parse().unwrap();
        let wanted: toml::Table = Added::default()
            .with_cfg("dispatch=\"tailcall\"")
            .line()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(parsed, wanted, "{cfg}");
        let cargo: Vec<&&str> = code
            .iter()
            .filter(|l| l.starts_with("cargo \"+$TOOLCHAIN\""))
            .collect();
        assert_eq!(cargo.len(), 2, "the check and the build: {cargo:#?}");
        for line in cargo {
            assert!(line.ends_with("\"${cfg[@]}\""), "{line}");
        }
    }

    /// The profiles the workspace's `Cargo.toml` defines are every
    /// [`Profile`] and no other, each by the name the command line takes,
    /// so a profile added there and not here is caught, and a name the
    /// command line refuses is one the workspace does not define.
    #[test]
    fn the_profiles_are_the_ones_the_workspace_defines() {
        use clap::ValueEnum;
        let text = std::fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
        let manifest: toml::Table = text.parse().unwrap();
        let mut defined: Vec<&str> = manifest["profile"]
            .as_table()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        defined.sort_unstable();
        let mut known: Vec<&str> = Profile::value_variants()
            .iter()
            .map(|p| {
                let named = p.to_possible_value().unwrap();
                assert_eq!(named.get_name(), p.name(), "{p:?}");
                p.name()
            })
            .collect();
        known.sort_unstable();
        assert_eq!(known, defined);
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
