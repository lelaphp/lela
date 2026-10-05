//! The cargo profiles a lane's binaries are built in (harness-design.md
//! §7, revision 82; #1994): the one statement of each profile's name and
//! the directory cargo builds it into, of the order the phpt lane looks
//! for the engine's binary in, and of the profile the correctness lanes
//! run, [`LANE`]. `Runner::resolve`, the cargo line a stale binary's skip
//! names, and the witness lane's build of a `process` fence's host all
//! read it, and none spells a profile.

use std::path::{Path, PathBuf};

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

    /// The profile whose directory is `dir`, where it is one of these.
    fn of_dir(dir: &str) -> Option<Profile> {
        PREFERRED.into_iter().find(|profile| profile.dir() == dir)
    }

    /// The profile this harness was built in, which `build.rs` reads off
    /// the directory cargo built it into: the witness lane serves its
    /// fences through the engine linked into this binary, and builds the
    /// host of a `process` fence in the same profile, so the fence runs
    /// the build the rest of the lane does and the two share the engine's
    /// compiled crates. A harness built in a profile this module does not
    /// name has none, and the build that would need one says which
    /// directory it was built in rather than guess.
    pub(crate) fn this_build() -> Result<Profile, String> {
        let dir = env!("XTASK_PROFILE_DIR");
        Profile::of_dir(dir).ok_or_else(|| {
            format!(
                "this harness was built into the profile directory `{dir}`, which names none of the profiles `oracle::profile` states; build it in one of them, as `cargo run --profile {} -p xtask`",
                LANE.name()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
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

    /// The directory each profile is built into names that profile back,
    /// and a directory no profile here names is none of them.
    #[test]
    fn a_profiles_directory_names_it() {
        assert_eq!(PREFERRED[0], LANE, "the lanes' build is looked for first");
        for profile in PREFERRED {
            assert_eq!(Profile::of_dir(profile.dir()), Some(profile));
        }
        assert_eq!(Profile::Dev.dir(), "debug");
        assert_eq!(Profile::of_dir("dev"), None);
        assert_eq!(Profile::of_dir("bench"), None);
    }
}
