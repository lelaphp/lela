//! The land's judges, built at main's tip (#4372, #4348 ruling 5).
//!
//! The merge lanes in `_merge` run the xtask `staging` builds, so the
//! readings of `budget`, `api-check` and `unsafe-audit` that `done` held a
//! merge to were the merge's own harness's, and a land run from `_merge`
//! asked `done`, and the `permit --diff` inside it, of that same build: a
//! branch could weaken the judge that judged it. So [`super::land`] has an
//! xtask built at main's tip judge `_merge` ([`Judge::over`]), built once
//! per tip and kept under main's `target/judge/<tip>/` ([`Judge::at`]).
//! What that build runs is `xtask worktree judge` ([`run`]): it reads the
//! harness's own files with main's readers, takes `budget`, `api-check`
//! and `unsafe-audit` over `_merge`'s tree, judges each harness file the
//! merge changed with steer's judge ([`crate::steer::since`]), runs `cargo
//! deny` where `Cargo.lock` differs from main's ([`deny`]), and asks
//! `done` since main's tip. `test`, `clippy`, `witness` and `miri` still
//! build and measure the change, as the merge lanes take them, and `done`
//! holds their readings as ever.
//!
//! A merge whose harness files main's readers cannot read is refused with
//! the two steps that land it ([`TWO_STEPS`]): first the xtask that reads
//! the new grammar, then the row that uses it. #4343 was such a merge, and
//! landed from `_merge` by hand.
//!
//! A merge that changes the harness itself is judged the same way, and
//! what its own xtask wrote for the judge to read is not what main's
//! reads (#4378): a reading it keyed under a rule of `key.rs` it changed
//! is stale here, and one it wrote in a shape of `reading.rs` main's does
//! not read is missing. The judge keeps its own keying and its own
//! reader, since one that held a reading to the key the merge computes
//! would let a merge whose key is a constant land on readings of another
//! tree, which is the branch deciding its own land again. So the land
//! refuses, names the case and prints the commands that take those
//! readings by main's build ([`Retaken`]), over `_merge`'s tree and under
//! main's rules; the merge lands on them, and they are stale on `main`
//! once its xtask is the merge's.
//!
//! What the merge's xtask rendered into the tree is held the same way:
//! a lock or a generated file it renders otherwise fails main's
//! `api-check` or `budget`, which the judge takes itself, and no reading
//! taken again clears a file of the tree. The land refuses and names the
//! two steps such a change lands in ([`Held`]): first the xtask that
//! changes the instrument, over a tree main's passes, then the tree.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::divergence::Registry;
use crate::git::git;
use crate::layout::HARNESS;
use crate::manifest::Manifest;
use crate::reading::{Reading, Status, Store};
use crate::track::short;
use crate::worktree::say;
use crate::{api_check, budget, done, steer, unsafe_audit};

/// Where the judges are built and kept, under main's checkout: one
/// directory per tip holding its `xtask`, the cargo target directory they
/// are built in, `build`, and the tip's tree they are built from, `src`,
/// while a build stands.
const HOME: &str = "target/judge";

/// What a land refuses a merge whose harness files main's xtask cannot
/// read with: the two steps such a change lands in (merging.md).
pub(crate) const TWO_STEPS: &str = "a change to the grammar of the harness's own files lands in two \
     steps: first the xtask that reads the new grammar, merged and landed with no row that uses it; \
     then, once main's xtask reads it, the row that does (.claude/skills/specs/merging.md)";

/// What a land refuses with where the merge changes the harness's own
/// sources and `done` holds a reading stale or missing ([`Retaken`]): the
/// case, and what such a merge lands on (merging.md).
pub(crate) const RETAKEN: &str = "the merge changes the harness's own sources, so its xtask may key a \
     reading or write it otherwise than main's, which judges: main's xtask holds each reading to the \
     key it computes and reads it in the shape it writes, and a reading the merge's lanes took under \
     another keying or shape is stale or missing here and stays so however often they take it. Such \
     a merge lands on readings main's xtask takes (.claude/skills/specs/merging.md)";

/// The instruments the judge takes itself over the merged tree ([`run`]),
/// in the order it takes them.
const TAKEN: [&str; 3] = [
    budget::INSTRUMENT,
    api_check::INSTRUMENT,
    unsafe_audit::INSTRUMENT,
];

/// What a land refuses with where the merge changes the harness's own
/// sources and an instrument of [`TAKEN`] fails ([`Held`]): the case, and
/// the two steps such a change lands in (merging.md).
pub(crate) const HELD: &str = "where the merge's own xtask passes it over the merged tree, the merge \
     changes what the instrument holds a tree to, a lock's rendering or a generated file's among it, \
     and the land holds the tree to main's, which no run clears. Such a change lands in two steps: \
     first the xtask that changes the instrument, merged and landed over a tree main's xtask passes; \
     then, once main's xtask is that one, what the tree changes by, in a batch of its own \
     (.claude/skills/specs/merging.md)";

/// The cargo-deny release land runs and nightly.yml installs: one older
/// cannot read the advisory database's CVSS 4.0 scores, and so judges
/// nothing.
pub(crate) const DENY: &str = "0.20.2";

/// What the judges say a line as.
const JUDGE: &str = "worktree judge";

/// An xtask built at one tip of `main`, the binary that judges a merge
/// into it.
pub(crate) struct Judge {
    binary: PathBuf,
    tip: String,
}

impl Judge {
    /// The xtask built at `tip` of the repository whose main checkout is
    /// `main`: the one kept under [`HOME`] where it stands, else built
    /// there from the tip's tree, laid down by `git archive`, which no
    /// worktree's edits reach, and the builds kept for other tips removed.
    pub(crate) fn at(main: &Path, tip: &str) -> Result<Judge> {
        let home = main.join(HOME);
        let binary = home.join(tip).join("xtask");
        let judge = Judge {
            binary: binary.clone(),
            tip: tip.to_owned(),
        };
        if binary.is_file() {
            say!(
                super::LAND,
                "main's xtask at {} judges, built at {}",
                short(tip),
                binary.display()
            );
            return Ok(judge);
        }
        say!(
            super::LAND,
            "building main's xtask at {} in {} to judge the merge",
            short(tip),
            home.display()
        );
        let src = home.join("src");
        lay(main, tip, &src)?;
        let built = build(&src, &home.join("build"))?;
        crate::make_parent(&binary)?;
        // A copy cut short leaves a partial file, which the next land at
        // this tip would take for the judge; a rename in one directory
        // is whole or absent.
        let partial = binary.with_extension("partial");
        fs::copy(&built, &partial)
            .with_context(|| format!("copying {} to {}", built.display(), partial.display()))?;
        fs::rename(&partial, &binary)
            .with_context(|| format!("renaming {} to {}", partial.display(), binary.display()))?;
        remove_dir(&src)?;
        prune(&home, tip)?;
        Ok(judge)
    }

    /// [`run`] over `merge` since this judge's tip, by this judge: what it
    /// prints on this process's streams, and an error where it refuses.
    pub(crate) fn over(&self, merge: &Path) -> Result<()> {
        let status = Command::new(&self.binary)
            .arg("--root")
            .arg(merge)
            .args(["worktree", "judge", "--base", &self.tip])
            .current_dir(merge)
            .env_remove("CARGO_MANIFEST_DIR")
            .status()
            .with_context(|| format!("running {}", self.binary.display()))?;
        if !status.success() {
            bail!(
                "main's xtask at {} refuses {} ({status})",
                short(&self.tip),
                merge.display()
            );
        }
        Ok(())
    }
}

/// `tip`'s tree laid down at `src` afresh, from the object store of the
/// repository at `main`.
fn lay(main: &Path, tip: &str, src: &Path) -> Result<()> {
    remove_dir(src)?;
    crate::make_dir(src)?;
    let mut archive = Command::new("git")
        .args(["archive", "--format=tar", tip])
        .current_dir(main)
        .stdout(Stdio::piped())
        .spawn()
        .context("git did not run")?;
    let tar = archive.stdout.take().context("git archive's output")?;
    let unpacked = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(src)
        .stdin(tar)
        .status()
        .context("tar did not run")?;
    let archived = archive.wait().context("git archive")?;
    if !archived.success() || !unpacked.success() {
        bail!(
            "laying {} down at {} failed: git archive {archived}, tar {unpacked}",
            short(tip),
            src.display()
        );
    }
    Ok(())
}

/// `xtask` built from the tree at `src` into the target directory
/// `target`, on the cargo `CARGO` names or the one on `PATH`; where the
/// binary is.
fn build(src: &Path, target: &Path) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "--quiet",
            "--locked",
            "--package",
            "xtask",
            "--bin",
            "xtask",
        ])
        .arg("--target-dir")
        .arg(target)
        .current_dir(src)
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .context("cargo did not run")?;
    let built = target.join("debug").join("xtask");
    if !status.success() || !built.is_file() {
        bail!(
            "cargo build of xtask in {} failed ({status}); main is untouched",
            src.display()
        );
    }
    Ok(built)
}

/// Every judge kept under `home` for a tip other than `tip` removed, so
/// one build is kept, the current tip's.
fn prune(home: &Path, tip: &str) -> Result<()> {
    for dir in crate::entries(home)? {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let a_tip = name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit());
        if a_tip && name != tip {
            remove_dir(&dir)?;
        }
    }
    Ok(())
}

/// The directory `dir` removed with what it holds, where it stands.
fn remove_dir(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}

/// `xtask worktree judge --base <tip>`: the land's judgement of the
/// checkout at `root`, `_merge` when [`Judge::over`] asks it, since
/// `base`, main's tip, by this build. The harness's own files read with
/// this build's readers, refused with [`TWO_STEPS`] where one does not
/// read; `budget`, `api-check` and `unsafe-audit` taken over the tree and
/// recorded in its store, whichever way they read, since `done` decides
/// what a failing one holds; each harness file the commits since `base`
/// changed judged by steer, printed and not recorded, a proposal that
/// would not load refused; `cargo deny` where `Cargo.lock` moved; and
/// `done` since `base`, as land's caller, whose verdict is the exit.
/// Where the merge changes the harness, the refusal says more than the
/// count: [`HELD`] where an instrument taken here fails ([`Held`]), and
/// [`RETAKEN`] and the commands where a reading is stale or missing
/// ([`Retaken`]).
pub(crate) fn run(root: &Path, base: &str) -> Result<()> {
    let manifest = Manifest::load(root).map_err(|err| unread(root, base, &err))?;
    let registry = Registry::load(root).map_err(|err| unread(root, base, &err))?;
    let store = Store::of_workspace(root);
    let mut readings = budget::measure(root, &manifest, &store, false)?;
    readings.extend(api_check::hold(&manifest, root, false)?);
    readings.extend(unsafe_audit::measure(root, &manifest, &store, false)?);
    store.record(&readings)?;
    let steered = steer::since(root, base)?;
    readings.extend(steered.iter().cloned());
    tell(&readings);
    if let Some(unloaded) = steered.iter().find(|r| r.status == Status::Fail) {
        bail!("{}", unloaded.message);
    }
    deny(root, base)?;
    let land = (Some(base), done::Caller::Land);
    let verdict = done::run(root, &manifest, &registry, &store, land)?;
    let held = Held::of(&verdict).map(|held| held.to_string());
    let retaken = Retaken::of(root, &verdict)?.map(|retaken| retaken.to_string());
    let said: Vec<String> = held.into_iter().chain(retaken).collect();
    match verdict.exit() {
        Err(unmet) if !said.is_empty() => bail!("{unmet}. {}", said.join(". ")),
        exit => exit,
    }
}

/// Whether the merge `verdict` judges changes the harness, a file under
/// `xtask/`: only then can its own xtask differ from this build, main's,
/// in what it writes for the judge to read and in what it passes. The
/// verdict's crates do not say it, since `done` counts the harness as
/// touched by every file its tests read ([`crate::key::READ_BY_TESTS`]),
/// the manifest, the registry, `data/` and `scripts/` among them, and a
/// merge that changes one of those alone builds the xtask main has.
fn of_the_harness(verdict: &done::Verdict) -> bool {
    let under = |change: &crate::git::Change| Path::new(&change.path).starts_with(HARNESS);
    verdict.changed.iter().any(under)
}

/// The instruments of [`TAKEN`] a verdict holds failing where the merge
/// changes the harness: what a land that refuses such a merge says
/// beside `done`'s count ([`HELD`], #4378). The merge's commits passed
/// each under the merge's own xtask, in pre-commit, so one failing here
/// over the same tree is the two builds holding the tree to different
/// things, and the land holds it to main's.
#[derive(Debug, PartialEq)]
struct Held<'a> {
    /// Each failing instrument once, in the list's order.
    instruments: Vec<&'a str>,
}

impl<'a> Held<'a> {
    /// What of `verdict` is held to main's: none where the merge leaves
    /// the harness as main has it, a failing reading there being the
    /// tree's own to mend, and none where no instrument of [`TAKEN`]
    /// fails.
    fn of(verdict: &'a done::Verdict) -> Option<Held<'a>> {
        let mut instruments: Vec<&str> = Vec::new();
        for unmet in &verdict.unmet {
            let failing = unmet.kind == done::Kind::Failing;
            let taken = (unmet.instrument.as_deref())
                .filter(|instrument| failing && TAKEN.contains(instrument));
            if let Some(instrument) = taken.filter(|instrument| !instruments.contains(instrument)) {
                instruments.push(instrument);
            }
        }
        (of_the_harness(verdict) && !instruments.is_empty()).then_some(Held { instruments })
    }
}

impl std::fmt::Display for Held<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the merge changes the harness's own sources, and main's xtask, which judges, fails {} \
             here: {HELD}",
            self.instruments.join(" and ")
        )
    }
}

/// The readings of a verdict that no run of the merge's own xtask is
/// sure to clear, and the commands that take them by this build, main's:
/// what a land that refuses a merge of the harness says beside `done`'s
/// count ([`RETAKEN`], #4378).
#[derive(Debug, PartialEq)]
struct Retaken<'a> {
    /// The checkout judged, `_merge`.
    root: &'a Path,
    /// This build, as the land ran it: `target/judge/<tip>/xtask` of
    /// main's checkout.
    judge: PathBuf,
    /// The words of each command after `cargo xtask`, once each, in the
    /// list's order.
    asks: Vec<&'a str>,
}

impl<'a> Retaken<'a> {
    /// What of `verdict`, judged over `root`, is retaken: each stale or
    /// missing reading a command takes, where the merge changes the
    /// harness. None where the merge leaves `xtask/` as main has it,
    /// since its xtask then keys and writes a reading as this build does
    /// and the list's own commands clear it; and none where no reading is
    /// stale or missing, a failing one being the merge's to mend.
    fn of(root: &'a Path, verdict: &'a done::Verdict) -> Result<Option<Retaken<'a>>> {
        if !of_the_harness(verdict) {
            return Ok(None);
        }
        let mut asks: Vec<&str> = Vec::new();
        for unmet in &verdict.unmet {
            let taken = matches!(unmet.kind, done::Kind::Stale | done::Kind::Missing);
            if let Some(ask) = unmet.asks().filter(|ask| taken && !asks.contains(ask)) {
                asks.push(ask);
            }
        }
        if asks.is_empty() {
            return Ok(None);
        }
        let judge = std::env::current_exe().context("locating this binary")?;
        Ok(Some(Retaken { root, judge, asks }))
    }
}

impl std::fmt::Display for Retaken<'_> {
    /// [`RETAKEN`], each command on a line of its own as a shell takes
    /// it, and what follows the land.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (root, judge) = (self.root.display(), self.judge.display());
        writeln!(f, "{RETAKEN}: in {root}, run")?;
        for ask in &self.asks {
            writeln!(f, "  {judge} --root {root} {ask}")?;
        }
        write!(
            f,
            "then land again. Once main is fast-forwarded its xtask is the merge's own, and the push's \
             `done` names each reading that build holds stale, to take in main's checkout before the push"
        )
    }
}

/// The refusal of a merge whose harness files this build cannot read.
fn unread(root: &Path, base: &str, err: &anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "main's xtask at {} cannot read {}'s harness files: {err:#}. {TWO_STEPS}",
        short(base),
        root.display()
    )
}

/// What the judges measured, a line per instrument and a line per reading
/// that did not pass.
fn tell(readings: &[Reading]) {
    let mut instruments: Vec<&str> = readings.iter().map(|r| r.instrument.as_str()).collect();
    instruments.dedup();
    for instrument in instruments {
        let of: Vec<&Reading> = (readings.iter())
            .filter(|r| r.instrument == instrument)
            .collect();
        let passed = of.iter().filter(|r| r.status == Status::Pass).count();
        say!(JUDGE, "{instrument}: {passed} of {} pass", of.len());
        for reading in of.iter().filter(|r| r.status != Status::Pass) {
            println!("{reading}");
        }
    }
}

/// `cargo deny check advisories sources` over the checkout at `root`
/// under the `deny.toml` `base` carries, where `Cargo.lock` differs from
/// `base`'s: refused where it finds anything, where `cargo-deny` is not
/// the [`DENY`] release or is not installed, and where `base` carries no
/// `deny.toml`, since a land that cannot judge the graph does not pass
/// it. The `deny.toml` read is `base`'s, so a merge does not loosen what
/// judges it.
fn deny(root: &Path, base: &str) -> Result<()> {
    let moved = git(
        root,
        &["diff", "--name-only", base, "HEAD", "--", "Cargo.lock"],
    )?;
    if moved.trim().is_empty() {
        return Ok(());
    }
    let refuse = |why: String| -> Result<()> {
        bail!(
            "Cargo.lock differs from main's at {}, and {why}; land refuses rather than pass",
            short(base)
        )
    };
    let Some(config) = crate::git::show(root, base, "deny.toml")? else {
        return refuse("main carries no deny.toml for cargo deny to read".to_owned());
    };
    let installed = match Command::new("cargo-deny").arg("--version").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return refuse(format!(
                "cargo-deny is not installed: `cargo install --locked cargo-deny@{DENY}`"
            ));
        }
        Err(err) => return Err(err).context("running cargo-deny --version"),
    };
    if installed != format!("cargo-deny {DENY}") {
        return refuse(format!(
            "`cargo-deny --version` says {installed:?}, not cargo-deny {DENY}: `cargo install --locked cargo-deny@{DENY}`"
        ));
    }
    let path = root.join("target").join(format!("deny-{base}.toml"));
    crate::make_parent(&path)?;
    crate::write(&path, config)?;
    say!(JUDGE, "Cargo.lock moved since {}: cargo deny", short(base));
    // `--config` is cargo-deny's own option, before the subcommand: 0.20.2
    // refuses `check --config` as a usage error.
    let status = Command::new("cargo-deny")
        .arg("--config")
        .arg(&path)
        .args(["check", "advisories", "sources"])
        .current_dir(root)
        .status()
        .context("running cargo-deny check")?;
    crate::remove(&path)?;
    match Denied::of(status.code()) {
        Denied::Nothing => Ok(()),
        Denied::Found => refuse(format!(
            "cargo deny check advisories sources finds what main's deny.toml refuses ({status})"
        )),
        Denied::Unjudged => refuse(format!(
            "cargo-deny judged nothing ({status}): no exit of a finding in advisories or sources, so a usage error or a failure to run, which its output above names"
        )),
    }
}

/// What one `cargo-deny check advisories sources` said, read from its
/// exit once. cargo-deny exits with a bit per check that failed,
/// advisories 1 and sources 8 among them ([`DENY_FOUND`]), and clap's
/// usage error is 2, the bit of `bans`, which this check never runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Denied {
    /// Exit 0: neither check found anything.
    Nothing,
    /// An exit made only of the bits of the checks it ran.
    Found,
    /// Any other exit, or none (a signal): the run judged nothing.
    Unjudged,
}

/// The exit bits of the two checks the land runs: advisories and sources.
const DENY_FOUND: i32 = 1 | 8;

impl Denied {
    fn of(code: Option<i32>) -> Denied {
        match code {
            Some(0) => Denied::Nothing,
            Some(bits) if bits & !DENY_FOUND == 0 => Denied::Found,
            _ => Denied::Unjudged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nightly.yml installs the cargo-deny land runs, named once in
    /// [`DENY`], and runs the check land runs, so a release moved in one
    /// and not the other fails here.
    #[test]
    fn nightly_installs_the_cargo_deny_land_runs_and_runs_its_check() {
        let text =
            fs::read_to_string(crate::built_root().join(".github/workflows/nightly.yml")).unwrap();
        let at = text
            .find("\n  deny:\n")
            .expect("nightly.yml has a deny job");
        let job: Vec<&str> = (text[at + 1..].lines().skip(1))
            .take_while(|l| l.is_empty() || l.starts_with("    "))
            .collect();
        let job = job.join("\n") + "\n";
        assert!(
            job.contains(&format!("cargo install --locked cargo-deny@{DENY}\n")),
            "{job}"
        );
        assert!(
            job.contains("run: cargo deny check advisories sources\n"),
            "{job}"
        );
    }

    /// cargo-deny's exit is read once: its usage error is no finding.
    #[test]
    fn a_usage_error_is_no_finding_and_a_findings_bits_are_the_checks_run() {
        assert_eq!(Denied::of(Some(0)), Denied::Nothing);
        for found in [1, 8, 9] {
            assert_eq!(Denied::of(Some(found)), Denied::Found, "{found}");
        }
        for unjudged in [Some(2), Some(3), Some(101), None] {
            assert_eq!(Denied::of(unjudged), Denied::Unjudged, "{unjudged:?}");
        }
    }

    /// A verdict over a diff of `changed` whose unmet conditions are
    /// `unmet`, each a kind and the command that clears it, whose third
    /// word is its instrument. The harness is among its crates whatever
    /// changed, as `done` counts it for a file its tests read.
    fn verdict(changed: &[&str], unmet: &[(done::Kind, &str)]) -> done::Verdict {
        let base = "a".repeat(40);
        done::Verdict {
            base: done::Base {
                since: base.clone(),
                commit: base,
            },
            changed: (changed.iter())
                .map(|&path| crate::git::Change {
                    path: path.to_owned(),
                    deleted: false,
                })
                .collect(),
            crates: vec![HARNESS.to_owned()],
            ticket: None,
            unmet: (unmet.iter())
                .map(|&(kind, command)| done::Unmet {
                    kind,
                    instrument: command.split(' ').nth(2).map(str::to_owned),
                    scope: None,
                    message: String::new(),
                    command: Some(command.to_owned()),
                })
                .collect(),
            notes: Vec::new(),
            delegated: Vec::new(),
        }
    }

    /// Only a merge that changes a file under `xtask/` is told to take
    /// its readings by main's build, and only the stale and the missing
    /// ones, each command once: a merge that changes the registry and an
    /// engine source, which touches the harness crate for `done` and
    /// builds main's xtask all the same, clears its list by its own, and
    /// a failing reading is no run's to clear.
    #[test]
    fn a_merge_of_the_harness_retakes_its_stale_and_missing_readings_by_mains_build() {
        use done::Kind::{Failing, Missing, Refused, Stale};
        let root = Path::new("/the/merge");
        let list = [
            (Stale, "cargo xtask clippy --crate xtask"),
            (Stale, "cargo xtask clippy --crate xtask"),
            (Failing, "cargo xtask api-check"),
            (Missing, "cargo xtask test --crate xtask"),
            (Refused, "cargo xtask ticket read"),
        ];
        let read = [
            "docs/divergence.toml",
            "scripts/census.sh",
            "crates/php_engine/src/lib.rs",
        ];
        let source = "xtask/src/key.rs";
        assert_eq!(Retaken::of(root, &verdict(&read, &list)).unwrap(), None);
        let failing = verdict(&[source], &list[2..3]);
        assert_eq!(Retaken::of(root, &failing).unwrap(), None);

        let harness = verdict(&[read[0], source], &list);
        let retaken = Retaken::of(root, &harness).unwrap().expect("retaken");
        assert_eq!(retaken.asks, ["clippy --crate xtask", "test --crate xtask"]);
        let judge = retaken.judge.display().to_string();
        let said = retaken.to_string();
        assert!(said.starts_with(RETAKEN), "{said}");
        let lines: Vec<&str> = said.lines().filter(|l| l.starts_with("  ")).collect();
        assert_eq!(
            lines,
            [
                format!("  {judge} --root /the/merge clippy --crate xtask"),
                format!("  {judge} --root /the/merge test --crate xtask"),
            ],
            "{said}"
        );
        assert!(said.contains("then land again"), "{said}");
    }

    /// Only a merge that changes a file under `xtask/` is told the two
    /// steps, and only for an instrument the judge takes that fails, each
    /// named once: a merge that changes `seams.toml` alone fails `budget`
    /// by its tree, a failing `clippy` is the merge's code to mend, and a
    /// stale `api-check` is a reading to take.
    #[test]
    fn a_merge_of_the_harness_that_fails_an_instrument_the_judge_takes_is_held_to_mains() {
        use done::Kind::{Failing, Stale};
        let list = [
            (Failing, "cargo xtask api-check"),
            (Failing, "cargo xtask clippy --crate xtask"),
            (Failing, "cargo xtask api-check"),
            (Stale, "cargo xtask unsafe-audit"),
            (Failing, "cargo xtask budget"),
        ];
        let source = "xtask/src/api_check/mod.rs";
        assert_eq!(Held::of(&verdict(&["seams.toml"], &list)), None);
        assert_eq!(Held::of(&verdict(&[source], &list[1..2])), None);
        assert_eq!(Held::of(&verdict(&[source], &list[3..4])), None);

        let harness = verdict(&[source], &list);
        let held = Held::of(&harness).expect("held");
        assert_eq!(held.instruments, ["api-check", "budget"]);
        let said = held.to_string();
        assert!(
            said.contains("fails api-check and budget here: where the merge's own xtask passes"),
            "{said}"
        );
        assert!(said.ends_with(HELD), "{said}");
    }

    /// Building a judge keeps the current tip's alone: every other tip's
    /// directory goes, and what is not a tip's stays.
    #[test]
    fn a_prune_keeps_the_current_tips_judge_and_what_is_not_a_tips() {
        let tree = crate::scratch::Tree::new("judge-prune").unwrap();
        let home = tree.join("judge");
        let (tip, old) = ("a".repeat(40), "b".repeat(40));
        for dir in [tip.as_str(), old.as_str(), "build", "src"] {
            crate::make_dir(&home.join(dir)).unwrap();
        }
        prune(&home, &tip).unwrap();
        let mut left: Vec<String> = crate::entries(&home)
            .unwrap()
            .iter()
            .filter_map(|d| d.file_name()?.to_str().map(str::to_owned))
            .collect();
        left.sort();
        assert_eq!(left, [tip, "build".to_owned(), "src".to_owned()]);
    }
}
