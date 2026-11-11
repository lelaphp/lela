//! `oracle propose` (#4384, ruled on #4373): the whole diff a bump of the
//! oracle writes, read off the keg a person has poured and the php-src
//! checkout at its tag, for the person to apply with `git apply` and
//! commit. The pin moves only by a person's hand, and this is what the hand
//! applies, so `[sapi]` and `TEST_PHP_EXECUTABLE`, which the last bump left
//! at the keg before it (#3374), are written from the same reading as
//! `[oracle]`.
//!
//! **What it reads** is the machine and the tracked files: the keg's
//! receipt, for php's version and its runtime kegs; each binary's md5;
//! `[closure]`, walked from the binaries as the pin's own check walks it
//! ([`Closure::walked`]); php's `Configure Command`, as
//! `scripts/oracle-build.py configure-line` reads it; and the checkout's
//! commit at `php-<version>`, its `.phpt` count and the SHA-256 of each ini
//! file `dist/inputs.toml` names. The pin is read with [`Pin::parse`] and
//! its defects are not refused: at a bump the pin is behind the keg.
//!
//! **What it writes** is the diff and the notes beside it, and nothing
//! else: no file and no reading. Each value is replaced where it stands,
//! so every comment and key order a person wrote is kept: `oracle.toml`'s
//! `[oracle]`, `[closure]`, `[source]`, `[recipe.env]`'s
//! `TEST_PHP_EXECUTABLE` and `[sapi]`; `ci/oracle/configure.toml` below its
//! header; `dist/inputs.toml`'s `[php]` and `[php.ini]`; and
//! `dist/Dockerfile`'s `ARG PHP_VERSION`. A `[variant.*]` build is made
//! from the tag and, on macOS, against `[closure]`, so it is rebuilt and
//! never proposed: a note names each, beside the defects the proposed pin
//! carries until it is.
//!
//! **Where the keg is** is `--keg`, any path in it, a formula's `opt` link
//! among them; without it, the `opt` link of the pinned formula under the
//! prefix the pinned php stands in. **Where the tree is** is `--source`;
//! without it, `[source].path` with the pinned version replaced by the
//! keg's. Pointed at the pinned keg and tree, the diff is empty.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::closure::in_cellar;
use super::formula::bottles;
use super::{Closure, Formula, Pin, Platform, Source, md5_of};
use crate::sha256::Sha256;

/// What `oracle propose` read: the diff a bump writes, empty where the keg
/// and the checkout are the pinned ones, and the notes beside it, one line
/// each.
#[derive(Debug, Clone, Default)]
pub struct Proposal {
    pub diff: String,
    pub notes: Vec<String>,
}

const PIN: &str = "oracle.toml";
const CONFIGURE: &str = "ci/oracle/configure.toml";
const INPUTS: &str = "dist/inputs.toml";
const DOCKERFILE: &str = "dist/Dockerfile";

/// The diff a bump to the keg at `keg` and the php-src checkout at
/// `source` writes over the tracked files at `root`, each place defaulted
/// as the module says; it reads them and writes nothing.
pub fn propose(root: &Path, keg: Option<&Path>, source: Option<&Path>) -> Result<Proposal> {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400);
    let (y, m, d) = crate::reading::civil_from_days(i64::try_from(days).unwrap_or_default());
    proposed(root, keg, source, &format!("{y:04}-{m:02}-{d:02}"))
}

/// [`propose`] on the day `today`, which a rewritten `[closure]` says it was
/// frozen on.
fn proposed(
    root: &Path,
    keg: Option<&Path>,
    source: Option<&Path>,
    today: &str,
) -> Result<Proposal> {
    let read =
        |file: &str| fs::read_to_string(root.join(file)).with_context(|| format!("reading {file}"));
    let text = read(PIN)?;
    let pin = Pin::parse(&text).context(PIN)?;
    let keg = Keg::read(&pin, keg)?;
    let tree = Checkout::read(&pin, &keg.version, source)?;
    let pinned = pinned(&text, &pin, &keg, &tree, today)?;
    let (configure, inputs, dockerfile) = (read(CONFIGURE)?, read(INPUTS)?, read(DOCKERFILE)?);
    let files = [
        (PIN, text.as_str(), pinned.clone()),
        (CONFIGURE, &configure, configured(&configure, &keg)?),
        (INPUTS, &inputs, inputs_of(&inputs, &tree)?),
        (
            DOCKERFILE,
            &dockerfile,
            dockerfile_of(&dockerfile, &keg.version)?,
        ),
    ];
    let diff: String = (files.iter())
        .map(|(file, was, now)| unified(file, was, now))
        .collect();
    let mut notes = vec![format!(
        "read the keg {}, php {} poured from the formula {}, and the php-src checkout {} at php-{} ({})",
        keg.dir.display(),
        keg.version,
        keg.formula,
        tree.dir.display(),
        keg.version,
        tree.commit
    )];
    if diff.is_empty() {
        notes.push(
            "the keg and the checkout are the ones the pin names: the bump writes nothing"
                .to_owned(),
        );
        return Ok(Proposal { diff, notes });
    }
    notes.extend(proposed_notes(&pin, &pinned, &keg)?);
    Ok(Proposal { diff, notes })
}

/// What a person reads beside a diff that is not empty: each variant left
/// for its rebuild, every defect the proposed pin carries until then, and
/// what the pin's keg check says of the keg, a keg not yet `brew pin`ned
/// among it. A proposed pin whose formula and paths disagree is refused,
/// since no diff written here may carry one.
fn proposed_notes(pin: &Pin, pinned: &str, keg: &Keg) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for (name, variant) in &pin.variant {
        let platforms: Vec<&str> = (Platform::ALL.into_iter())
            .filter(|&platform| variant.build(platform).is_some())
            .map(Platform::key)
            .collect();
        if !platforms.is_empty() {
            notes.push(format!(
                "[variant.{name}] pins builds for {}, each made from the tag the diff replaces and left as it stands: `scripts/oracle-build.py build <platform> {name}` rebuilds each once the diff is applied (#4373)",
                platforms.join(", ")
            ));
        }
    }
    if !keg.kept.is_empty() {
        notes.push(format!(
            "{} of the pin's [closure] the binaries do not load and the receipt does not name, carried at the version installed: a row leaves the record by a person's hand",
            keg.kept.join(", ")
        ));
    }
    let proposed = Pin::parse(pinned).context("the proposed oracle.toml")?;
    let poured = (proposed.oracle.formula).held(&proposed.oracle.version, &proposed.binaries());
    if let Some(defect) = poured.first() {
        bail!("the proposed oracle.toml is no pin its own check takes: {defect}");
    }
    let defects = proposed.defects().iter();
    notes.extend(
        defects.map(|defect| format!("the proposed pin carries a defect until then: {defect}")),
    );
    if let Err(err) = keg.closure.held(&keg.binaries()) {
        notes.push(format!("the keg check: {err:#}"));
    }
    Ok(notes)
}

/// The keg a bump moves the pin to, as this machine has it.
#[derive(Debug)]
struct Keg {
    /// `<prefix>/Cellar/<formula>/<bottle>`.
    dir: PathBuf,
    formula: Formula,
    /// php's version, as the receipt names it: the bottle with no revision.
    version: String,
    /// Each pinned binary's key in `oracle.toml` beside its path in this
    /// keg and its md5: `[oracle]`'s php first, then each host `[sapi]`
    /// names.
    binaries: Vec<(&'static str, PathBuf, String)>,
    closure: Closure,
    /// The kegs of the pin's `[closure]` that the binaries do not load and
    /// the receipt does not name, carried at the version installed, since a
    /// row leaves the record by a person's hand alone.
    kept: Vec<String>,
    /// php's `Configure Command`, less `./configure`.
    configure: Vec<String>,
}

/// What of a keg's `INSTALL_RECEIPT.json` the bump reads.
#[derive(Deserialize)]
struct Receipt {
    source: ReceiptSource,
    runtime_dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct ReceiptSource {
    versions: Versions,
}

#[derive(Deserialize)]
struct Versions {
    stable: String,
}

#[derive(Deserialize)]
struct Dependency {
    full_name: String,
}

impl Keg {
    /// The keg `given` stands in, or the pinned formula's `opt` link's;
    /// each pinned binary at its place in that keg, as the pin places it in
    /// the keg it names; the closure they load; and php's configure line.
    fn read(pin: &Pin, given: Option<&Path>) -> Result<Keg> {
        let Some((prefix, (name, bottle))) = in_cellar(&pin.oracle.path) else {
            bail!(
                "[oracle].path {} stands in no Homebrew keg, so the pin places no binary in one",
                pin.oracle.path.display()
            );
        };
        let pinned = prefix.join("Cellar").join(name).join(bottle);
        let place = given.map_or_else(
            || prefix.join("opt").join(pin.oracle.formula.to_string()),
            Path::to_path_buf,
        );
        let real = fs::canonicalize(&place)
            .with_context(|| format!("resolving the keg at {}", place.display()))?;
        let Some((prefix, (name, bottle))) = in_cellar(&real) else {
            bail!(
                "{} stands in no Homebrew keg, `<prefix>/Cellar/<formula>/<bottle>`",
                place.display()
            );
        };
        let dir = prefix.join("Cellar").join(&name).join(&bottle);
        let formula: Formula = name.parse()?;
        let file = dir.join("INSTALL_RECEIPT.json");
        let receipt =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let receipt: Receipt = serde_json::from_str(&receipt)
            .with_context(|| format!("reading {}", file.display()))?;
        let version = receipt.source.versions.stable;
        if !bottles(&version, &bottle) {
            bail!(
                "{} holds php {version}, and {bottle} is no bottle of it",
                dir.display()
            );
        }
        let hosts = [
            ("cgi", &pin.sapi.cgi),
            ("fpm", &pin.sapi.fpm),
            ("phpdbg", &pin.sapi.phpdbg),
        ];
        let hosts = hosts
            .into_iter()
            .filter_map(|(key, host)| Some((key, host.as_ref()?.path.as_path())));
        let mut binaries = Vec::new();
        for (key, path) in std::iter::once(("php", pin.oracle.path.as_path())).chain(hosts) {
            let Ok(within) = path.strip_prefix(&pinned) else {
                bail!(
                    "the pinned {key} {} stands outside the keg [oracle].path names, {}",
                    path.display(),
                    pinned.display()
                );
            };
            let path = dir.join(within);
            let md5 = md5_of(&path, &format!("the keg's {key} at "))?;
            binaries.push((key, path, md5));
        }
        let mut keg = Keg {
            dir,
            formula,
            version,
            binaries,
            closure: Closure::default(),
            kept: Vec::new(),
            configure: Vec::new(),
        };
        // The record's own kegs are named after the receipt's, so a row
        // a person keeps that nothing loads stays a row (#3454).
        let mut named: Vec<String> = (receipt.runtime_dependencies.into_iter())
            .map(|dependency| dependency.full_name)
            .collect();
        let recorded = pin.closure.iter().flat_map(|closure| closure.kegs().keys());
        let kept: Vec<String> = recorded
            .filter(|keg| !named.contains(keg))
            .cloned()
            .collect();
        named.extend(kept.iter().cloned());
        let (closure, unloaded) = Closure::walked(&keg.binaries(), &named)?;
        keg.closure = closure;
        keg.kept = kept
            .into_iter()
            .filter(|keg| unloaded.contains(keg))
            .collect();
        keg.configure = configure_line(keg.php())?;
        Ok(keg)
    }

    fn binaries(&self) -> Vec<&Path> {
        self.binaries
            .iter()
            .map(|(_, path, _)| path.as_path())
            .collect()
    }

    fn php(&self) -> &Path {
        &self.binaries[0].1
    }
}

/// php's `Configure Command` row of `php -n -i`, split into its words as
/// `scripts/oracle-build.py configure-line` splits it, less the
/// `./configure` it starts with.
fn configure_line(php: &Path) -> Result<Vec<String>> {
    let ran = Command::new(php)
        .args(["-n", "-i"])
        .output()
        .with_context(|| format!("running {} -n -i", php.display()))?;
    if !ran.status.success() {
        bail!("{} -n -i exited {}", php.display(), ran.status);
    }
    let info = String::from_utf8_lossy(&ran.stdout);
    let Some(row) = info
        .lines()
        .find_map(|line| line.strip_prefix("Configure Command => "))
    else {
        bail!("{} -n -i prints no Configure Command", php.display());
    };
    let mut args = words(row)?;
    if args.first().map(String::as_str) != Some("./configure") {
        bail!(
            "the Configure Command of {} starts {:?}, not ./configure",
            php.display(),
            args.first()
        );
    }
    args.remove(0);
    Ok(args)
}

/// `line` split into words as Python's `shlex.split` splits it, which
/// `scripts/oracle-build.py` reads the `Configure Command` with: blanks
/// part words; a single quote holds everything to the next; a double quote
/// holds everything to the next unescaped one, a backslash in it escaping
/// only `"` and itself; and a backslash outside them escapes any character.
fn words(line: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    let unclosed =
        || anyhow::anyhow!("the Configure Command {line:?} leaves a quote or an escape open");
    while let Some(c) = chars.next() {
        if matches!(c, ' ' | '\t' | '\r' | '\n') {
            words.extend(word.take());
            continue;
        }
        let w = word.get_or_insert_with(String::new);
        match c {
            '\'' => loop {
                match chars.next().ok_or_else(unclosed)? {
                    '\'' => break,
                    c => w.push(c),
                }
            },
            '"' => loop {
                match chars.next().ok_or_else(unclosed)? {
                    '"' => break,
                    '\\' => match chars.next().ok_or_else(unclosed)? {
                        c @ ('"' | '\\') => w.push(c),
                        c => w.extend(['\\', c]),
                    },
                    c => w.push(c),
                }
            },
            '\\' => w.push(chars.next().ok_or_else(unclosed)?),
            c => w.push(c),
        }
    }
    words.extend(word);
    Ok(words)
}

/// The php-src checkout at the keg's tag.
#[derive(Debug)]
struct Checkout {
    dir: PathBuf,
    tag: String,
    /// The tag's commit, abbreviated as `[source].commit` is.
    commit: String,
    full: String,
    phpt_files: usize,
}

impl Checkout {
    /// The checkout at `given`, or at `[source].path` with the pinned
    /// version replaced by `version`, held to stand at `php-<version>`.
    fn read(pin: &Pin, version: &str, given: Option<&Path>) -> Result<Checkout> {
        let tag = format!("php-{version}");
        let dir = if let Some(dir) = given {
            dir.to_path_buf()
        } else {
            let Some(pinned) = pin.source.path() else {
                bail!(
                    "the pin states no [source].path: name the php-src checkout at {tag} with --source"
                );
            };
            let (was, path) = (&pin.oracle.version, pinned.to_string_lossy());
            if was.is_empty() || !path.contains(was.as_str()) {
                bail!(
                    "[source].path {path} names no version to replace: name the php-src checkout at {tag} with --source"
                );
            }
            PathBuf::from(path.replace(was.as_str(), version))
        };
        if !super::checkout(&dir) {
            bail!(
                "{} is no php-src checkout: lay one down at {tag}, or name it with --source",
                dir.display()
            );
        }
        let commit = |name: &str| -> Result<String> {
            let spec = format!("{name}^{{commit}}");
            let out = crate::git::elsewhere(&dir, &["rev-parse", "--verify", &spec]).with_context(
                || format!("reading {name} in the php-src checkout {}", dir.display()),
            )?;
            Ok(out.trim().to_owned())
        };
        let (full, head) = (commit(&tag)?, commit("HEAD")?);
        if head != full {
            bail!(
                "the php-src checkout {} is at {head}, and {tag} is {full}: check the tag out",
                dir.display()
            );
        }
        let files = Source::files(&dir)?;
        let phpt = |file: &&PathBuf| file.to_string_lossy().ends_with(".phpt");
        let length = pin.source.commit.len().clamp(8, full.len());
        Ok(Checkout {
            commit: full[..length].to_owned(),
            phpt_files: files.iter().filter(phpt).count(),
            dir,
            tag,
            full,
        })
    }

    /// The SHA-256 of `file` at the tag's commit, hex alone, as
    /// `dist/inputs.toml` spells it.
    fn sha256(&self, file: &str) -> Result<String> {
        let mut git = Command::new("git");
        crate::batch::without_git(&mut git);
        let spec = format!("{}:{file}", self.full);
        let bytes = crate::git::run(git, &self.dir, &["show", &spec], None)
            .with_context(|| format!("reading {file} at {}", self.tag))?;
        let digest = Sha256::of(&bytes).to_string();
        Ok(digest.trim_start_matches("sha256:").to_owned())
    }
}

/// `oracle.toml` moved to the keg and the checkout.
fn pinned(text: &str, pin: &Pin, keg: &Keg, tree: &Checkout, today: &str) -> Result<String> {
    let mut file = Text::of(text);
    file.set("oracle", "version", &quoted(&keg.version))?;
    let formula = quoted(&keg.formula.to_string());
    if file.find("oracle", "formula")?.is_some() {
        file.set("oracle", "formula", &formula)?;
    } else if keg.formula != Formula::default() {
        file.insert_after("oracle", "version", &format!("formula = {formula}"))?;
    }
    let path = |path: &Path| quoted(&path.to_string_lossy());
    let (_, php, md5) = &keg.binaries[0];
    file.set("oracle", "path", &path(php))?;
    file.set("oracle", "md5", &quoted(md5))?;
    if pin
        .closure
        .as_ref()
        .is_some_and(|recorded| *recorded != keg.closure)
    {
        file.rows("closure", &keg.closure)?;
        file.dated("closure", today)?;
    }
    file.set("source", "tag", &quoted(&tree.tag))?;
    file.set("source", "commit", &quoted(&tree.commit))?;
    if pin.source.path().is_some() {
        file.set("source", "path", &path(&tree.dir))?;
    }
    file.set("source", "phpt_files", &tree.phpt_files.to_string())?;
    if pin.recipe.env.contains_key("TEST_PHP_EXECUTABLE") {
        file.set("recipe.env", "TEST_PHP_EXECUTABLE", &path(php))?;
    }
    for (key, at, md5) in &keg.binaries[1..] {
        let host = format!("{{ path = {}, md5 = {} }}", path(at), quoted(md5));
        file.set("sapi", key, &host)?;
    }
    Ok(file.text())
}

/// `ci/oracle/configure.toml` as `scripts/oracle-build.py configure-line`
/// prints it for the keg: the tracked header, the md5 and the arguments.
fn configured(text: &str, keg: &Keg) -> Result<String> {
    let Some(at) = text.find("\nmd5 = ") else {
        bail!("{CONFIGURE} has no `md5 = ` line below its header");
    };
    let mut out = text[..=at].to_owned();
    writeln!(out, "md5 = {}\nargs = [", quoted(&keg.binaries[0].2)).ok();
    for arg in &keg.configure {
        writeln!(out, "    {},", quoted(arg)).ok();
    }
    out.push_str("]\n");
    Ok(out)
}

/// `dist/inputs.toml` moved to the checkout: `[php]`'s tag and full
/// commit, and each ini file `[php.ini]` names at that commit.
fn inputs_of(text: &str, tree: &Checkout) -> Result<String> {
    let table: toml::Table = toml::from_str(text).with_context(|| format!("reading {INPUTS}"))?;
    let ini = (table
        .get("php")
        .and_then(|php| php.get("ini"))
        .and_then(toml::Value::as_table))
    .with_context(|| format!("{INPUTS} has no [php.ini]"))?;
    let mut file = Text::of(text);
    file.set("php", "tag", &quoted(&tree.tag))?;
    file.set("php", "commit", &quoted(&tree.full))?;
    for name in ini.keys() {
        file.set("php.ini", name, &quoted(&tree.sha256(name)?))?;
    }
    Ok(file.text())
}

/// `dist/Dockerfile` with its one `ARG PHP_VERSION=` naming `version`.
fn dockerfile_of(text: &str, version: &str) -> Result<String> {
    let mut file = Text::of(text);
    let lead = "ARG PHP_VERSION=";
    let named: Vec<usize> = (0..file.lines.len())
        .filter(|&at| file.lines[at].trim_start().starts_with(lead))
        .collect();
    let [at] = named[..] else {
        bail!(
            "{DOCKERFILE} names {} `{lead}` lines, where the apache stage names one",
            named.len()
        );
    };
    let line = &file.lines[at];
    let indent = &line[..line.len() - line.trim_start().len()];
    file.lines[at] = format!("{indent}{lead}{version}");
    Ok(file.text())
}

/// A TOML string as `scripts/oracle-build.py` writes one: `"` and `\`
/// escaped, and nothing else.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A key as TOML spells it: bare where it can be, quoted otherwise.
fn key(name: &str) -> String {
    let bare = !name.is_empty()
        && (name.bytes()).all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if bare { name.to_owned() } else { quoted(name) }
}

/// A file's text as lines, each value replaced where it stands, so what a
/// person wrote around it is kept.
struct Text {
    lines: Vec<String>,
    newline: bool,
}

impl Text {
    fn of(text: &str) -> Text {
        let body = text.strip_suffix('\n').unwrap_or(text);
        Text {
            lines: body.split('\n').map(str::to_owned).collect(),
            newline: text.ends_with('\n'),
        }
    }

    fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        if self.newline {
            text.push('\n');
        }
        text
    }

    /// Whether each line starts inside a multi-line string, so it is
    /// neither a header nor a key's line.
    fn quoted(&self) -> Vec<bool> {
        let mut inside = false;
        let toggles =
            |line: &str| (line.matches("'''").count() + line.matches("\"\"\"").count()) % 2 == 1;
        (self.lines.iter())
            .map(|line| {
                let was = inside;
                inside ^= toggles(line);
                was
            })
            .collect()
    }

    /// The lines of `[table]` that are no line of a multi-line string:
    /// from the one after its header to the next header.
    fn table(&self, table: &str) -> Result<Vec<usize>> {
        let quoted = self.quoted();
        let header = |at: usize| {
            let rest = self.lines[at]
                .trim()
                .strip_prefix('[')
                .filter(|_| !quoted[at])?;
            let (name, _) = rest
                .strip_prefix('[')
                .map_or(Some(rest), |_| None)?
                .split_once(']')?;
            Some(name.trim())
        };
        let start = (0..self.lines.len()).find(|&at| header(at) == Some(table));
        let start = start.with_context(|| format!("no [{table}] table"))? + 1;
        let end = (start..self.lines.len()).find(|&at| header(at).is_some());
        let end = end.unwrap_or(self.lines.len());
        Ok((start..end).filter(|&at| !quoted[at]).collect())
    }

    /// The line of `[table]` that sets `name`, none where it sets none,
    /// refused where two do.
    fn find(&self, table: &str, name: &str) -> Result<Option<usize>> {
        let spelled = key(name);
        let sets = |line: &String| {
            let rest = line.trim_start().strip_prefix(spelled.as_str())?;
            rest.trim_start().starts_with('=').then_some(())
        };
        let found: Vec<usize> = (self.table(table)?.into_iter())
            .filter(|&at| sets(&self.lines[at]).is_some())
            .collect();
        match found[..] {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            _ => bail!("[{table}] sets {name} on {} lines", found.len()),
        }
    }

    /// `[table]`'s `name` set to `value`, a TOML value as written, the
    /// line's indent, key, spacing and trailing comment kept.
    fn set(&mut self, table: &str, name: &str, value: &str) -> Result<()> {
        let at = self
            .find(table, name)?
            .with_context(|| format!("[{table}] sets no {name}"))?;
        let line = &self.lines[at];
        let lead = line.len() - line.trim_start().len() + key(name).len();
        let equals = lead + line[lead..].find('=').unwrap_or_default() + 1;
        let start = equals + (line.len() - equals - line[equals..].trim_start().len());
        let end = start + value_length(&line[start..]);
        self.lines[at] = format!("{}{value}{}", &line[..start], &line[end..]);
        Ok(())
    }

    /// `line` written after the line of `[table]` that sets `name`.
    fn insert_after(&mut self, table: &str, name: &str, line: &str) -> Result<()> {
        let at = self
            .find(table, name)?
            .with_context(|| format!("[{table}] sets no {name}"))?;
        self.lines.insert(at + 1, line.to_owned());
        Ok(())
    }

    /// `[table]`'s rows replaced by `closure`'s, in name order, where its
    /// rows stand: after the comment that heads them, and before the blank
    /// line after them.
    fn rows(&mut self, table: &str, closure: &Closure) -> Result<()> {
        let range = self.table(table)?;
        let row = |line: &String| !line.trim().is_empty() && !line.trim_start().starts_with('#');
        let rows: Vec<usize> = range
            .into_iter()
            .filter(|&at| row(&self.lines[at]))
            .collect();
        let (Some(&first), Some(&last)) = (rows.first(), rows.last()) else {
            bail!("[{table}] has no rows to replace");
        };
        if rows.len() != last - first + 1 {
            bail!(
                "[{table}] has a comment or a blank line between its rows, which the bump would move"
            );
        }
        let written = (closure.kegs().iter())
            .map(|(keg, version)| format!("{} = {}", key(keg), quoted(version)));
        self.lines.splice(first..=last, written);
        Ok(())
    }

    /// The date `[table]`'s comment says the record was frozen on, `stood
    /// on <date>`, moved to `today`.
    fn dated(&mut self, table: &str, today: &str) -> Result<()> {
        let date = regex_lite::Regex::new(r"stood on \d{4}-\d{2}-\d{2}")?;
        for at in self.table(table)? {
            let line = &self.lines[at];
            if line.trim_start().starts_with('#') && date.is_match(line) {
                self.lines[at] = date.replace(line, format!("stood on {today}")).into_owned();
            }
        }
        Ok(())
    }
}

/// The length of the TOML value `text` starts with: a string to its closing
/// quote, an inline table or array to its closing bracket, and anything else
/// to a comment or the line's end, its trailing blanks left out.
fn value_length(text: &str) -> usize {
    let (mut depth, mut quote, mut escaped) = (0usize, None, false);
    for (at, c) in text.char_indices() {
        match (quote, c) {
            (Some('"'), '\\') if !escaped => {
                escaped = true;
                continue;
            }
            (Some(q), c) if c == q && !escaped => {
                quote = None;
                if depth == 0 {
                    return at + 1;
                }
            }
            (None, '"' | '\'') => quote = Some(c),
            (None, '{' | '[') => depth += 1,
            (None, '}' | ']') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at + 1;
                }
            }
            (None, '#') if depth == 0 => return text[..at].trim_end().len(),
            _ => {}
        }
        escaped = false;
    }
    text.trim_end().len()
}

/// The unified diff from `was` to `now` of the file at `path`, three lines
/// of context to a hunk, as `git apply` takes it; empty where they are the
/// same.
fn unified(path: &str, was: &str, now: &str) -> String {
    if was == now {
        return String::new();
    }
    let was: Vec<&str> = was.split_inclusive('\n').collect();
    let now: Vec<&str> = now.split_inclusive('\n').collect();
    let (n, m) = (was.len(), now.len());
    // lcs[i][j]: the longest common run of was[i..] and now[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if was[i] == now[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    // Each line in order: its mark, and the line of `was` and of `now` it
    // stands before.
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && was[i] == now[j] {
            ops.push((' ', i, j, was[i]));
            (i, j) = (i + 1, j + 1);
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(('-', i, j, was[i]));
            i += 1;
        } else {
            ops.push(('+', i, j, now[j]));
            j += 1;
        }
    }
    let changed: Vec<usize> = (0..ops.len()).filter(|&at| ops[at].0 != ' ').collect();
    let mut out = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
    let mut at = 0;
    while at < changed.len() {
        let mut last = at;
        while last + 1 < changed.len() && changed[last + 1] - changed[last] <= 7 {
            last += 1;
        }
        let hunk = &ops[changed[at].saturating_sub(3)..(changed[last] + 4).min(ops.len())];
        let count = |mark: char| hunk.iter().filter(|op| op.0 == ' ' || op.0 == mark).count();
        let (old, new) = (count('-'), count('+'));
        let start = |line: usize, count: usize| if count == 0 { line } else { line + 1 };
        let (from, to) = (start(hunk[0].1, old), start(hunk[0].2, new));
        writeln!(out, "@@ -{from},{old} +{to},{new} @@").ok();
        for (mark, _, _, line) in hunk {
            out.push(*mark);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
        at = last + 1;
    }
    out
}

#[cfg(test)]
mod tests;
