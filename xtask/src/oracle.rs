//! The oracle module (harness-design.md §4.3, §8; design §8): the pinned
//! `php`, its recipe, and the two functions every comparison goes through.
//! `run` executes a program under the recipe and a regime and returns the
//! normalised output; `compare` gives the verdict, consulting the divergence
//! registry, so no instrument diffs on its own. The pin's grammar is this
//! module's: `oracle.toml` is refused on a key outside it.
//!
//! The version, the digest and the path are values of the pin and appear in
//! no source file (§10 rule 5).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::divergence::{Divergence, Registry};

/// `oracle.toml`: the pinned oracle and its invocation recipe, read through
/// the manifest's `[oracle]` row and nowhere else.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub oracle: Binary,
    pub source: Source,
    pub recipe: Recipe,
    #[serde(default)]
    pub regime: BTreeMap<String, Regime>,
}

/// `[oracle]`: the `php` binary by version, path on this machine, and digest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binary {
    pub version: String,
    pub path: PathBuf,
    pub md5: String,
}

/// `[source]`: the php-src tree at the same tag.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub tag: String,
    pub commit: String,
    pub path: PathBuf,
    pub phpt_files: u64,
}

/// `[recipe]`: how a program is run and how its output is normalised.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub script_name: String,
    pub preamble: String,
    pub args: Vec<String>,
    pub ini: Vec<String>,
    pub env_clear: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub limits: Limits,
    pub normalise: Normalise,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub timeout_s: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Normalise {
    pub crlf_to_lf: bool,
    pub trim: String,
    pub scratch_token: String,
}

/// `[regime.<name>]`: the compiled tier's regimes, `-d` overrides added
/// after the recipe's ini.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Regime {
    pub ini: Vec<String>,
}

impl Pin {
    /// Load the pin at `path`.
    pub fn load(path: &Path) -> Result<Pin> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading the oracle pin {}", path.display()))?;
        Pin::parse(&text).with_context(|| format!("{}", path.display()))
    }

    /// Parse the pin's text and hold it to the grammar.
    pub fn parse(text: &str) -> Result<Pin> {
        Ok(toml::from_str(text)?)
    }

    /// The regime by name; an unknown name is refused beside the known ones.
    pub fn regime(&self, name: &str) -> Result<&Regime> {
        self.regime.get(name).with_context(|| {
            format!(
                "no regime {name:?} in the pin; the regimes are {}",
                self.regime
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }

    /// The binary's digest against the pin's; a mismatch refuses the run.
    pub fn assert_digest(&self) -> Result<()> {
        let path = &self.oracle.path;
        let bytes = fs::read(path)
            .with_context(|| format!("reading the pinned php at {}", path.display()))?;
        let actual = format!("{:x}", md5::compute(bytes));
        if actual != self.oracle.md5 {
            bail!(
                "{} has md5 {actual}; the pin says {} for php {}. Report the drift; the pin moves only by a person's hand",
                path.display(),
                self.oracle.md5,
                self.oracle.version
            );
        }
        Ok(())
    }
}

/// What `run` executes: a snippet, written as the recipe's script with its
/// preamble, or a file that already is a program, run from its directory.
#[derive(Debug, Clone, Copy)]
pub enum Program<'a> {
    Snippet(&'a str),
    File(&'a Path),
}

/// The normalised output of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The exit code, or `None` when a signal ended the process.
    pub exit: Option<i32>,
    /// Wall time, disclosure only; the limit is the recipe's.
    pub wall: Duration,
}

/// Run `program` under the pin's recipe, with `regime`'s ini after it when
/// one is named, and return the normalised output. The digest is asserted
/// first; the process runs with the recipe's environment alone, in the
/// program's directory, under the recipe's timeout.
pub fn run(pin: &Pin, program: Program<'_>, regime: Option<&str>) -> Result<Output> {
    pin.assert_digest()?;
    let regime = regime.map(|name| pin.regime(name)).transpose()?;
    let recipe = &pin.recipe;

    let scratch = Scratch::for_program(recipe, program)?;
    let mut cmd = Command::new(&pin.oracle.path);
    cmd.args(&recipe.args);
    for ini in recipe
        .ini
        .iter()
        .chain(regime.into_iter().flat_map(|r| &r.ini))
    {
        cmd.arg("-d").arg(ini);
    }
    cmd.arg(&scratch.script).current_dir(&scratch.dir);
    if recipe.env_clear {
        cmd.env_clear();
    }
    cmd.envs(&recipe.env);
    let out_path = scratch.own.join("stdout");
    let err_path = scratch.own.join("stderr");
    cmd.stdin(Stdio::null())
        .stdout(File::create(&out_path).context("creating the stdout file")?)
        .stderr(File::create(&err_path).context("creating the stderr file")?);

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {}", pin.oracle.path.display()))?;
    let limit = Duration::from_secs(recipe.limits.timeout_s);
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for php")? {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "php ran past the recipe's limit of {} s on {}",
                recipe.limits.timeout_s,
                scratch.script.display()
            );
        }
        thread::sleep(Duration::from_millis(5));
    };
    let wall = started.elapsed();

    let stdout = fs::read(&out_path).context("reading php's stdout")?;
    let stderr = fs::read(&err_path).context("reading php's stderr")?;
    Ok(Output {
        stdout: scratch.normalise(&recipe.normalise, stdout),
        stderr: scratch.normalise(&recipe.normalise, stderr),
        exit: status.code(),
        wall,
    })
}

/// Where a program runs: its directory and script, and a directory of this
/// run's own for the streams, removed on drop.
struct Scratch {
    dir: PathBuf,
    script: PathBuf,
    own: PathBuf,
}

impl Scratch {
    fn for_program(recipe: &Recipe, program: Program<'_>) -> Result<Scratch> {
        let own =
            std::env::temp_dir().join(format!("xtask-oracle-{}-{}", std::process::id(), unique()));
        fs::create_dir_all(&own).with_context(|| format!("creating {}", own.display()))?;
        let (dir, script) = match program {
            Program::Snippet(code) => {
                let script = PathBuf::from(&recipe.script_name);
                fs::write(own.join(&script), format!("{}{code}", recipe.preamble))
                    .with_context(|| format!("writing {}", recipe.script_name))?;
                (own.clone(), script)
            }
            Program::File(path) => {
                let dir = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
                let script = path
                    .file_name()
                    .map(PathBuf::from)
                    .with_context(|| format!("{} names no file", path.display()))?;
                (dir, script)
            }
        };
        Ok(Scratch { dir, script, own })
    }

    /// The recipe's normalisation: CRLF to LF, the trim set off both ends,
    /// and the scratch directory, as given and as the kernel resolves it,
    /// replaced by the token.
    fn normalise(&self, rules: &Normalise, mut bytes: Vec<u8>) -> Vec<u8> {
        if rules.crlf_to_lf {
            bytes = replace(&bytes, b"\r\n", b"\n");
        }
        let trim: &[u8] = rules.trim.as_bytes();
        let start = bytes
            .iter()
            .position(|b| !trim.contains(b))
            .unwrap_or(bytes.len());
        let end = bytes
            .iter()
            .rposition(|b| !trim.contains(b))
            .map_or(start, |i| i + 1);
        bytes = bytes[start..end].to_vec();
        let mut forms = vec![self.dir.clone()];
        if let Ok(real) = self.dir.canonicalize() {
            forms.push(real);
        }
        forms.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
        for form in forms {
            let form = form.to_string_lossy();
            if !form.is_empty() {
                bytes = replace(&bytes, form.as_bytes(), rules.scratch_token.as_bytes());
            }
        }
        bytes
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.own);
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn replace(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// What a comparison found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Same,
    /// The bytes differ and the registry accepts it under this row.
    Diverged(Divergence),
    /// The bytes differ and nothing accepts it: the line diff, expected as
    /// `-`, actual as `+`.
    Differs(String),
}

/// Compare the oracle's bytes with the engine's for the divergence key `id`.
/// Equal bytes are `Same` whatever the registry says; a difference is
/// `Diverged` under an accepted row for `id` and `Differs` otherwise. An
/// `open` or `fixed` row accepts nothing. Proposing a row for a `Differs`
/// is the caller's, through `Registry::propose`.
pub fn compare(registry: &Registry, id: &str, expected: &[u8], actual: &[u8]) -> Verdict {
    if expected == actual {
        return Verdict::Same;
    }
    match registry.accepted(id) {
        Some(row) => Verdict::Diverged(row.clone()),
        None => Verdict::Differs(diff(expected, actual)),
    }
}

/// A line diff of `expected` against `actual`: common lines with a space,
/// expected-only with `-`, actual-only with `+`. Bytes that are not UTF-8
/// are shown lossily.
pub fn diff(expected: &[u8], actual: &[u8]) -> String {
    let expected = String::from_utf8_lossy(expected);
    let actual = String::from_utf8_lossy(actual);
    let a: Vec<&str> = expected.lines().collect();
    let b: Vec<&str> = actual.lines().collect();
    // Longest common subsequence by dynamic programming; outputs are short.
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        let line = if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
            (' ', a[i - 1])
        } else if j < b.len() && (i == a.len() || table[i][j + 1] >= table[i + 1][j]) {
            j += 1;
            ('+', b[j - 1])
        } else {
            i += 1;
            ('-', a[i - 1])
        };
        out.push(line.0);
        out.push_str(line.1);
        out.push('\n');
    }
    out
}
