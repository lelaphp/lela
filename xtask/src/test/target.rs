//! `xtask test --target <triple>`: a crate's tests built for a target that
//! is not the host's and run on this machine, as a reading over
//! `<crate>/<triple>` (#4462). The scope is a second reading of one crate,
//! as `clippy`'s `<crate>/release` is, keyed over the crate's closure as
//! every reading is (harness-design.md §4.2, §4.5 condition 1, ADR-0011):
//! the key holds no target, since the closure cargo metadata declares
//! counts a target's own dependencies whichever target is built.
//!
//! The reading says how this machine ran the code ([`Runs`]): x86-64 code
//! on Apple silicon runs translated by Rosetta 2, and a reading taken so
//! measured the translation and never Intel hardware.

use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

use crate::manifest::Crate;

/// The target a run builds for, and how this machine runs what it builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    triple: String,
    runs: Runs,
}

/// How this machine runs a target's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Runs {
    /// On hardware of the target's own architecture.
    Natively,
    /// x86-64 code on Apple silicon, translated by Rosetta 2.
    UnderRosetta,
}

/// Where Rosetta 2's runtime stands once `softwareupdate --install-rosetta`
/// has laid it down.
const ROSETTA: &str = "/Library/Apple/usr/libexec/oah/libRosettaRuntime";

impl Target {
    /// `triple` as this machine runs it, refused where it runs no code of
    /// that target, or where a crate of `crates` names the targets it is
    /// built for in its `[[crate]]` row and `triple` is not one of them.
    pub(crate) fn on_this_machine(triple: &str, crates: &[&Crate]) -> Result<Target> {
        if let Some(krate) = crates
            .iter()
            .find(|k| !k.targets.is_empty() && !k.targets.iter().any(|t| t == triple))
        {
            bail!(
                "{}: its row names the targets it is built for, {}, and {triple} is not one",
                krate.name,
                krate.targets.join(", ")
            );
        }
        let runs = Runs::of(triple, std::env::consts::OS, hardware())?;
        if runs == Runs::UnderRosetta && !Path::new(ROSETTA).exists() {
            bail!(
                "{triple}: this machine runs x86-64 code only under Rosetta 2, and {ROSETTA} is absent"
            );
        }
        Ok(Target {
            triple: triple.to_owned(),
            runs,
        })
    }

    /// The flags a cargo build or `cargo test` takes for it.
    pub(crate) fn flags(&self) -> [&str; 2] {
        ["--target", &self.triple]
    }

    /// The scope of the reading over the crate named `name`.
    pub(crate) fn scope(&self, name: &str) -> String {
        format!("{name}/{}", self.triple)
    }

    /// The head of the reading's message over the crate named `name`: its
    /// scope and how the code ran.
    pub(crate) fn label(&self, name: &str) -> String {
        let how = match self.runs {
            Runs::Natively => "run natively",
            Runs::UnderRosetta => {
                "run under Rosetta 2, x86-64 code translated on Apple silicon, so no Intel hardware was measured"
            }
        };
        format!("{} {how}", self.scope(name))
    }
}

impl Runs {
    /// How a machine of `os` on `hardware`, an architecture as Rust names
    /// it, runs `triple`'s code: refused where it runs none.
    fn of(triple: &str, os: &str, hardware: &str) -> Result<Runs> {
        let arch = triple.split('-').next().unwrap_or_default();
        let system = if triple.ends_with("-apple-darwin") {
            "macos"
        } else if triple.contains("-linux-") {
            "linux"
        } else {
            ""
        };
        match (system == os, arch) {
            (true, arch) if arch == hardware => Ok(Runs::Natively),
            (true, "x86_64") if os == "macos" && hardware == "aarch64" => Ok(Runs::UnderRosetta),
            _ => {
                bail!("{triple}: this machine, {hardware} under {os}, runs no code of that target")
            }
        }
    }
}

/// This machine's architecture, as Rust names it: on macOS, whether the
/// hardware is Apple silicon, which `hw.optional.arm64` says even to a
/// translated process, asked of `sysctl` by its path so a `PATH` without
/// `/usr/sbin` never labels a translated run native; elsewhere the one this
/// harness was built for.
fn hardware() -> &'static str {
    if cfg!(target_os = "macos") {
        let asked = Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.optional.arm64"])
            .output();
        if asked.is_ok_and(|out| out.stdout.trim_ascii() == b"1") {
            return "aarch64";
        }
    }
    std::env::consts::ARCH
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Runs, Target};
    use crate::cli::{Cli, Command};
    use crate::manifest::Crate;

    /// A target is asked of the crates named and of no other set, and a
    /// crate whose row names its targets is built for none beside them.
    #[test]
    fn a_target_is_asked_of_named_crates_built_for_it() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|cli| cli.command);
        let on = ["--target", "x86_64-apple-darwin"];
        assert!(parse(&[&["xtask", "test"][..], &on].concat()).is_err());
        assert!(parse(&[&["xtask", "test", "--staged"][..], &on].concat()).is_err());
        let named = parse(&[&["xtask", "test", "--crate", "php_jit"][..], &on].concat());
        assert!(
            matches!(&named, Ok(Command::Test { target: Some(t), .. }) if t == on[1]),
            "{named:?}"
        );
        let jit = Crate {
            name: "php_jit".into(),
            targets: vec!["aarch64-apple-darwin".into()],
            ..Crate::default()
        };
        let refused = Target::on_this_machine(on[1], &[&jit]).map_err(|e| e.to_string());
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("is not one")),
            "{refused:?}"
        );
    }

    /// A translated run's reading names Rosetta 2 and claims no Intel
    /// hardware (#4462).
    #[test]
    fn a_translated_reading_says_rosetta_and_claims_no_intel_hardware() {
        let target = |runs| Target {
            triple: "x86_64-apple-darwin".into(),
            runs,
        };
        let translated = target(Runs::UnderRosetta).label("php_jit");
        assert!(
            translated.starts_with("php_jit/x86_64-apple-darwin run under Rosetta 2,"),
            "{translated}"
        );
        assert!(translated.ends_with("so no Intel hardware was measured"));
        assert_eq!(
            target(Runs::Natively).label("php_jit"),
            "php_jit/x86_64-apple-darwin run natively"
        );
    }

    /// x86-64 macOS code on Apple silicon runs under Rosetta 2, a target of
    /// the hardware's own architecture natively, and a target of another
    /// system, or another architecture with no translator, not at all.
    #[test]
    fn how_a_machine_runs_a_target_is_decided_by_its_system_and_hardware() {
        let of = |triple, os, hw| Runs::of(triple, os, hw).map_err(|e| e.to_string());
        let rosetta = of("x86_64-apple-darwin", "macos", "aarch64");
        assert_eq!(rosetta, Ok(Runs::UnderRosetta));
        let intel = of("x86_64-apple-darwin", "macos", "x86_64");
        assert_eq!(intel, Ok(Runs::Natively));
        let linux = of("aarch64-unknown-linux-gnu", "linux", "aarch64");
        assert_eq!(linux, Ok(Runs::Natively));
        for (triple, os, hw) in [
            ("aarch64-apple-darwin", "macos", "x86_64"),
            ("x86_64-unknown-linux-gnu", "macos", "aarch64"),
            ("x86_64-unknown-linux-gnu", "linux", "aarch64"),
            ("wasm32-wasip1", "macos", "aarch64"),
        ] {
            let refused = of(triple, os, hw);
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|e| e.contains("runs no code of that target")),
                "{triple} on {hw} under {os}: {refused:?}"
            );
        }
    }
}
