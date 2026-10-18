//! One cargo run over many crates, read back crate by crate (#2165).
//!
//! `clippy` and `test` once ran one cargo per crate, one after another:
//! each call resolved the whole graph, scanned every fingerprint and built
//! alone, so cargo never compiled two crates at once. Here a run is one
//! cargo invocation over every crate asked for, `--keep-going` so a crate
//! that does not compile stops only the crates built on it, and
//! `--message-format=json` so each artifact and each diagnostic names the
//! package it is of. [`Run::of`] reads that back as one [`Built`] per
//! crate: its diagnostics, the executables of its test targets, and
//! whether every target the crate declares was built. The instruments
//! then judge each crate on its own part of the one run.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::manifest::Crate;

/// What the run made of one crate.
#[derive(Debug, Default)]
pub(crate) struct Built {
    /// Each diagnostic rustc gave over the crate's targets, rendered as
    /// rustc prints it, with its level.
    pub(crate) messages: Vec<Message>,
    /// The test executables of the crate, in the order cargo built them.
    pub(crate) executables: Vec<Executable>,
    /// Whether every target the run was asked to build of the crate was
    /// built: false for a crate that did not compile, and for one a
    /// dependency's failure kept from compiling, which has no diagnostic
    /// of its own.
    pub(crate) whole: bool,
    /// Whether the crate's library carries doctests.
    pub(crate) doctests: bool,
    /// The crate's library target's name, which `Doc-tests` names it by.
    pub(crate) lib: Option<String>,
    /// The `OUT_DIR` its `build.rs` wrote to, where it has one.
    pub(crate) out_dir: Option<PathBuf>,
    /// The package as its manifest declares it.
    pub(crate) package: Package,
    /// The targets the run must build of the crate for it to be whole.
    wanted: Vec<String>,
    /// The targets the run built of it.
    made: Vec<String>,
}

impl Built {
    /// What a run is to make of `package`, before it has made anything.
    fn of(package: &Package, targets: Targets) -> Built {
        let wanted = package
            .targets
            .iter()
            .filter(|t| match targets {
                Targets::Tests => t.test && t.required_features.is_empty(),
                Targets::All => !t.kind.iter().any(|k| k == "custom-build"),
                Targets::Shipped => t
                    .kind
                    .iter()
                    .any(|k| k.contains("lib") || k == "proc-macro" || k == "bin"),
            })
            .map(|t| t.name.clone())
            .collect();
        let lib = package.targets.iter().find(|t| {
            t.kind
                .iter()
                .any(|k| k.contains("lib") || k == "proc-macro")
        });
        Built {
            doctests: lib.is_some_and(|t| t.doctest),
            lib: lib.map(|t| t.name.clone()),
            package: package.clone(),
            wanted,
            ..Built::default()
        }
    }

    /// One of cargo's messages about the crate: a diagnostic kept, a
    /// target counted as made and a test executable kept, and
    /// `build.rs`'s `OUT_DIR` kept.
    fn absorb(&mut self, message: Line) {
        match message.reason.as_str() {
            "compiler-message" => {
                // A target checked twice, the library as itself and as its
                // test, says one thing twice; cargo counts it once, as a
                // duplicate, and so does this.
                if let Some(diagnostic) = message.message.filter(|d| !d.is_summary()) {
                    let rendered = diagnostic.rendered.unwrap_or_default();
                    if !self.messages.iter().any(|m| m.rendered == rendered) {
                        self.messages.push(Message {
                            level: diagnostic.level,
                            rendered,
                        });
                    }
                }
            }
            "compiler-artifact" => {
                if let Some(target) = message.target {
                    if let Some(path) = message.executable
                        && message.profile.as_ref().is_some_and(|p| p.test)
                    {
                        self.executables.push(Executable {
                            path,
                            kind: target.kind.first().cloned().unwrap_or_default(),
                        });
                    }
                    self.made.push(target.name);
                }
            }
            "build-script-executed" => self.out_dir = message.out_dir,
            _ => {}
        }
    }

    /// How many diagnostics of `level` (`warning`, `error`) the crate has.
    pub(crate) fn count(&self, level: &str) -> usize {
        self.messages.iter().filter(|m| m.level == level).count()
    }

    /// Every diagnostic, rendered, in the order rustc gave them.
    pub(crate) fn rendered(&self) -> String {
        self.messages.iter().map(|m| m.rendered.as_str()).collect()
    }
}

/// One diagnostic of one crate.
#[derive(Debug, Clone)]
pub(crate) struct Message {
    pub(crate) level: String,
    pub(crate) rendered: String,
}

/// One test executable and the target it is of.
#[derive(Debug, Clone)]
pub(crate) struct Executable {
    pub(crate) path: PathBuf,
    /// The target's kind, `lib`, `bin` or `test`: an integration test's
    /// executable runs with `CARGO_TARGET_TMPDIR` set.
    pub(crate) kind: String,
}

/// A package as `cargo metadata` declares it: what a test binary is run
/// with, cargo's own environment for it.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Package {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) manifest_path: PathBuf,
    #[serde(default)]
    pub(crate) authors: Vec<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) homepage: Option<String>,
    #[serde(default)]
    pub(crate) repository: Option<String>,
    #[serde(default)]
    pub(crate) license: Option<String>,
    #[serde(default)]
    pub(crate) license_file: Option<String>,
    #[serde(default)]
    pub(crate) rust_version: Option<String>,
    #[serde(default)]
    pub(crate) readme: Option<String>,
    #[serde(default)]
    targets: Vec<Target>,
}

impl Package {
    /// The directory the package's manifest is in.
    pub(crate) fn dir(&self) -> &Path {
        self.manifest_path.parent().unwrap_or(Path::new("."))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
    #[serde(default)]
    test: bool,
    #[serde(default)]
    doctest: bool,
    /// The features the target is built only with, which a run that
    /// turns on none never builds: `sapi/cli`'s `php-linked`.
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>,
}

/// Which of a package's targets a run builds, and under which features.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Targets {
    /// The targets `cargo test` runs in the default build, `--tests`: each
    /// with `test = true` and no required feature.
    Tests,
    /// Every target but `build.rs` under every feature each crate
    /// declares, `--all-targets --all-features`: the set `clippy` lints
    /// under (#140). A module behind a feature is a target the run must
    /// reach as any other: `php_bundle::test`, each extension's line in
    /// the bundle's `enabled()`, `sapi/cli`'s `php-linked`. Every feature,
    /// because no two in the workspace conflict and the lanes already
    /// build each but one: the `php` binary turns on the bundle's
    /// extensions, and `php-linked` its `linked` and `test`. The one is
    /// `php_bundle`'s `jit`, in the set on purpose: it links `php_jit` on
    /// the four targets, so the tier is linted with the rest.
    /// A selected crate's features no longer depend on what else the run
    /// selects, so its reading out of the one run is the one a run over
    /// it alone gives.
    All,
    /// The library and binaries under every feature, cargo's default
    /// targets, `--lib --bins` without its refusal of a package with no
    /// library: what the release lint reads (#2392).
    Shipped,
}

impl Targets {
    /// The flags that select these targets, and their features.
    fn flags(self) -> &'static [&'static str] {
        match self {
            Targets::Tests => &["--tests"],
            Targets::All => &["--all-targets", "--all-features"],
            Targets::Shipped => &["--all-features"],
        }
    }
}

/// One run over a set of crates.
#[derive(Debug, Default)]
pub(crate) struct Run {
    /// Each crate asked for, by its name in the manifest.
    pub(crate) crates: BTreeMap<String, Built>,
    /// Every directory a `build.rs` named for the linker, which a test
    /// binary's dynamic library path carries, as cargo's does.
    pub(crate) linked: Vec<PathBuf>,
    /// The toolchain's own library directories, `<sysroot>/lib` and the
    /// host's `<sysroot>/lib/rustlib/<host>/lib`: a proc-macro crate's test
    /// executable links the toolchain's `libstd` dynamically and finds it
    /// there, as cargo's dynamic library path lets it.
    pub(crate) toolchain: Vec<PathBuf>,
    /// The workspace's target directory.
    pub(crate) target_dir: PathBuf,
    /// What cargo said on stderr, for a run that failed for no crate's
    /// reason: a manifest it could not read, a lock it could not take.
    pub(crate) stderr: String,
}

impl Run {
    /// `cargo <args> <targets' flags> --message-format=json -p <each
    /// crate>` at `root`, read back crate by crate. `targets` says which
    /// of each package's targets the run builds, and must build for the
    /// crate to be whole.
    pub(crate) fn of(
        root: &Path,
        args: &[&str],
        crates: &[&Crate],
        targets: Targets,
    ) -> Result<Run> {
        let metadata = Metadata::read(root)?;
        let mut run = Run {
            target_dir: metadata.target_directory.clone(),
            toolchain: toolchain(root)?,
            ..Run::default()
        };
        // Each crate by the absolute directory a package id names.
        let mut by_dir: BTreeMap<PathBuf, String> = BTreeMap::new();
        for krate in crates {
            let (dir, package) = metadata.package_of(root, krate)?;
            run.crates
                .insert(krate.name.clone(), Built::of(package, targets));
            by_dir.insert(dir, krate.name.clone());
        }
        let mut command = Command::new("cargo");
        command
            .args(args)
            .args(targets.flags())
            .args(["--message-format=json", "--color", "never"])
            .current_dir(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for krate in crates {
            command.args(["-p", &krate.package()]);
        }
        without_git(&mut command);
        // Set, it points clippy past every generated clippy.toml for every
        // crate (`generated`'s module, #2475); rustc reads it not at all.
        command.env_remove(crate::generated::CONF_DIR);
        let mut child = command
            .spawn()
            .with_context(|| format!("running cargo {}", args.join(" ")))?;
        let stderr = child.stderr.take();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = stderr {
                std::io::Read::read_to_string(&mut pipe, &mut text).ok();
            }
            text
        });
        let stdout = child.stdout.take().context("cargo's stdout")?;
        for line in BufReader::new(stdout).lines() {
            let line = line.context("reading cargo's messages")?;
            let Ok(message) = serde_json::from_str::<Line>(&line) else {
                continue;
            };
            for path in &message.linked_paths {
                // `native=/path` or a bare path, as the script printed it.
                let path = PathBuf::from(path.split_once('=').map_or(path.as_str(), |(_, p)| p));
                if !run.linked.contains(&path) {
                    run.linked.push(path);
                }
            }
            if let Some(dir) = dir_of(&message.package_id)
                && let Some(name) = by_dir.get(&dir.canonicalize().unwrap_or(dir))
                && let Some(krate) = run.crates.get_mut(name)
            {
                krate.absorb(message);
            }
        }
        child.wait().context("waiting for cargo")?;
        run.stderr = stderr.join().unwrap_or_default();
        for krate in run.crates.values_mut() {
            krate.whole =
                krate.count("error") == 0 && krate.wanted.iter().all(|t| krate.made.contains(t));
        }
        Ok(run)
    }
}

/// `crates` as the cargo runs that build each as it ships (#2606). Cargo
/// unifies the features of every package one run selects, so a binary
/// built beside a crate that turns more of a shared dependency on is not
/// the binary its own package builds: `sapi/makala`'s, built beside
/// `sapi/cli`'s, loads cli's `pcntl` and `posix`, and makala's tests start
/// it. So a crate whose package has a binary a test run builds, one that
/// requires no feature, is a run of its own, and every other crate is in
/// one run, first.
pub(crate) fn runs<'c>(root: &Path, crates: &[&'c Crate]) -> Result<Vec<Vec<&'c Crate>>> {
    let metadata = Metadata::read(root)?;
    let mut shared = Vec::new();
    let mut runs = Vec::new();
    for krate in crates {
        let (_, package) = metadata.package_of(root, krate)?;
        let ships =
            |t: &Target| t.kind.iter().any(|k| k == "bin") && t.required_features.is_empty();
        if package.targets.iter().any(ships) {
            runs.push(vec![*krate]);
        } else {
            shared.push(*krate);
        }
    }
    if !shared.is_empty() {
        runs.insert(0, shared);
    }
    Ok(runs)
}

/// The toolchain's library directories, from `rustc --print sysroot` and
/// the host `rustc -vV` names, asked at `root` so its toolchain file holds.
fn toolchain(root: &Path) -> Result<Vec<PathBuf>> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let ask = |args: &[&str]| -> Result<String> {
        let out = Command::new(&rustc)
            .args(args)
            .current_dir(root)
            .output()
            .with_context(|| format!("running rustc {}", args.join(" ")))?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let sysroot = PathBuf::from(ask(&["--print", "sysroot"])?.trim());
    let mut dirs = vec![sysroot.join("lib")];
    if let Some(host) = ask(&["-vV"])?
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
    {
        dirs.push(sysroot.join("lib/rustlib").join(host.trim()).join("lib"));
    }
    Ok(dirs)
}

/// `cargo metadata --no-deps`, as much of it as a run reads.
#[derive(Debug, Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    target_directory: PathBuf,
}

impl Metadata {
    fn read(root: &Path) -> Result<Metadata> {
        let out = Command::new("cargo")
            .args([
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--offline",
            ])
            .current_dir(root)
            .output()
            .context("running cargo metadata")?;
        if !out.status.success() {
            bail!(
                "cargo metadata at {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        serde_json::from_slice(&out.stdout).context("cargo metadata wrote no metadata")
    }

    /// The package of `krate` and its directory, as cargo names it: a root
    /// reached through a symlink, as a macOS temporary directory is, names
    /// its packages by the path the link resolves to.
    fn package_of(&self, root: &Path, krate: &Crate) -> Result<(PathBuf, &Package)> {
        let dir = root.join(krate.path());
        let dir = dir.canonicalize().unwrap_or(dir);
        let package = self
            .packages
            .iter()
            .find(|p| p.dir().canonicalize().is_ok_and(|d| d == dir))
            .with_context(|| format!("cargo metadata names no package at {}", dir.display()))?;
        Ok((dir, package))
    }
}

/// One line of cargo's JSON messages, as much of it as a run reads.
#[derive(Deserialize)]
struct Line {
    reason: String,
    #[serde(default)]
    package_id: String,
    #[serde(default)]
    message: Option<Diagnostic>,
    #[serde(default)]
    target: Option<ArtifactTarget>,
    #[serde(default)]
    profile: Option<Profile>,
    #[serde(default)]
    executable: Option<PathBuf>,
    #[serde(default)]
    out_dir: Option<PathBuf>,
    #[serde(default)]
    linked_paths: Vec<String>,
}

#[derive(Deserialize)]
struct Diagnostic {
    level: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    spans: Vec<serde_json::Value>,
    #[serde(default)]
    rendered: Option<String>,
}

impl Diagnostic {
    /// Whether this is rustc's count of what it said rather than a
    /// diagnostic of its own: `2 warnings emitted`, `aborting due to 1
    /// previous error`, which name no place in the source.
    fn is_summary(&self) -> bool {
        self.spans.is_empty()
            && (self.message.ends_with(" emitted") || self.message.starts_with("aborting due to"))
    }
}

#[derive(Deserialize)]
struct ArtifactTarget {
    name: String,
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct Profile {
    test: bool,
}

/// The directory a workspace package id names: `path+file:///a/b#0.0.0`
/// or `path+file:///a/b#name@0.0.0` are both `/a/b`.
fn dir_of(package_id: &str) -> Option<PathBuf> {
    let rest = package_id.strip_prefix("path+file://")?;
    let path = rest.split('#').next()?;
    Some(PathBuf::from(path))
}

/// Where [`without_git`] keeps the index git handed a hook, as an absolute
/// path: the one a commit's tree is written from, which in a commit by
/// pathspec is a second index and not the checkout's (#2747).
pub(crate) const COMMIT_INDEX: &str = "XTASK_COMMIT_INDEX";

/// `command` with every `GIT_` variable taken out. Pre-commit runs the
/// instruments, and git exports `GIT_DIR`, `GIT_INDEX_FILE` and their kin
/// to a hook; a test that lays a scratch repository down would otherwise
/// write into the repository being committed to. `GIT_INDEX_FILE` is kept
/// as [`COMMIT_INDEX`], which only a reader of the commit's index reads
/// ([`crate::test::committed`]).
pub fn without_git(command: &mut Command) {
    for (key, value) in std::env::vars_os() {
        if key == "GIT_INDEX_FILE" {
            let index = std::path::absolute(&value).unwrap_or_else(|_| value.into());
            command.env(COMMIT_INDEX, index);
        }
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(&key);
        }
    }
}

/// The last `n` lines of `text`.
pub(crate) fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{dir_of, tail};

    #[test]
    fn a_package_id_names_its_directory_whatever_follows_the_hash() {
        assert_eq!(
            dir_of("path+file:///w/ext/json#php_ext_json@0.0.0"),
            Some(PathBuf::from("/w/ext/json"))
        );
        assert_eq!(
            dir_of("path+file:///w/xtask#0.0.0"),
            Some(PathBuf::from("/w/xtask"))
        );
        assert_eq!(
            dir_of("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"),
            None
        );
    }

    #[test]
    fn the_tail_is_the_last_lines() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
    }
}
