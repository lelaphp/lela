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

/// One invocation under the pin's recipe: which binary runs, which program,
/// which regime, and the four things a `.phpt` section adds to a run. Every
/// field past the program is empty for a witness, which is why `run` takes
/// none of them.
#[derive(Debug, Clone)]
pub struct Invocation<'a> {
    /// The pinned php for the oracle; the engine's own binary for a phpt
    /// lane, so that both run under one recipe.
    pub binary: &'a Path,
    pub program: Program<'a>,
    /// A `[regime.<name>]` of the pin, its ini after the recipe's.
    pub regime: Option<&'a str>,
    /// Ini after the recipe's and the regime's: a test's `--INI--`.
    pub ini: &'a [String],
    /// Arguments after the script: a test's `--ARGS--`.
    pub args: &'a [String],
    /// Environment entries added to the recipe's: a test's `--ENV--`.
    pub env: &'a [(String, String)],
    /// What the program reads on stdin: a test's `--STDIN--`, and nothing
    /// otherwise, which is what the recipe's own runs read.
    pub stdin: Option<&'a [u8]>,
    /// Whether the second stream is the first: upstream's `2>&1`, which is
    /// how a phpt run is captured unless `--CAPTURE_STDIO--` says
    /// otherwise (run-tests.php:1885-1889). A witness pins the two apart.
    pub merge_streams: bool,
}

impl<'a> Invocation<'a> {
    /// `binary` running `program` under the recipe and nothing more.
    pub fn new(binary: &'a Path, program: Program<'a>) -> Invocation<'a> {
        Invocation {
            binary,
            program,
            regime: None,
            ini: &[],
            args: &[],
            env: &[],
            stdin: None,
            merge_streams: false,
        }
    }
}

/// Run `program` under the pin's recipe, with `regime`'s ini after it when
/// one is named, and return the normalised output. The digest is asserted
/// first; the process runs with the recipe's environment alone, in the
/// program's directory, under the recipe's timeout.
pub fn run(pin: &Pin, program: Program<'_>, regime: Option<&str>) -> Result<Output> {
    pin.assert_digest()?;
    run_under(
        pin,
        &Invocation {
            regime,
            ..Invocation::new(&pin.oracle.path, program)
        },
    )
}

/// Run one invocation under the recipe and return the normalised output.
/// The digest is the pinned binary's alone and `run` asserts it; a phpt
/// lane runs the engine's binary here, under the same recipe, which is
/// what makes the two comparable (harness-design.md §4.3, design §8).
pub fn run_under(pin: &Pin, call: &Invocation<'_>) -> Result<Output> {
    let regime = call.regime.map(|name| pin.regime(name)).transpose()?;
    let recipe = &pin.recipe;

    let scratch = Scratch::for_program(recipe, call.program)?;
    let mut cmd = Command::new(call.binary);
    cmd.args(&recipe.args);
    for ini in recipe
        .ini
        .iter()
        .chain(regime.into_iter().flat_map(|r| &r.ini))
        .chain(call.ini)
    {
        cmd.arg("-d").arg(ini);
    }
    cmd.arg(&scratch.script)
        .args(call.args)
        .current_dir(&scratch.dir);
    if recipe.env_clear {
        cmd.env_clear();
    }
    cmd.envs(&recipe.env);
    cmd.envs(call.env.iter().map(|(name, value)| (name, value)));
    let out_path = scratch.own.join("stdout");
    let err_path = scratch.own.join("stderr");
    let out_file = File::create(&out_path).context("creating the stdout file")?;
    let err_file = if call.merge_streams {
        out_file.try_clone().context("sharing the stdout file")?
    } else {
        File::create(&err_path).context("creating the stderr file")?
    };
    cmd.stdin(stdin_of(&scratch, call.stdin)?)
        .stdout(out_file)
        .stderr(err_file);

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {}", call.binary.display()))?;
    let limit = Duration::from_secs(recipe.limits.timeout_s);
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for php")? {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} ran past the recipe's limit of {} s on {}",
                call.binary.display(),
                recipe.limits.timeout_s,
                scratch.script.display()
            );
        }
        thread::sleep(Duration::from_millis(5));
    };
    let wall = started.elapsed();

    let stdout = fs::read(&out_path).context("reading php's stdout")?;
    let stderr = if call.merge_streams {
        Vec::new()
    } else {
        fs::read(&err_path).context("reading php's stderr")?
    };
    Ok(Output {
        stdout: scratch.normalise(&recipe.normalise, stdout),
        stderr: scratch.normalise(&recipe.normalise, stderr),
        exit: status.code(),
        wall,
    })
}

/// The program's stdin: a file holding what a test's `--STDIN--` gives,
/// and nothing at all otherwise.
fn stdin_of(scratch: &Scratch, bytes: Option<&[u8]>) -> Result<Stdio> {
    let Some(bytes) = bytes else {
        return Ok(Stdio::null());
    };
    let path = scratch.own.join("stdin");
    fs::write(&path, bytes).context("writing the program's stdin")?;
    Ok(Stdio::from(
        File::open(&path).context("opening the program's stdin")?,
    ))
}

/// The recipe's normalisation of bytes that are not a stream of one run:
/// CRLF to LF and php's trim set off both ends. What a run's streams go
/// through is [`Scratch::normalise`], which is this and the scratch prefix;
/// what a `.phpt` expectation goes through is this alone, since an
/// expectation ran in no directory. The trim set is the recipe's and is
/// read nowhere else (design §8).
pub fn normalise(rules: &Normalise, bytes: Vec<u8>) -> Vec<u8> {
    let bytes = if rules.crlf_to_lf {
        replace(&bytes, b"\r\n", b"\n")
    } else {
        bytes
    };
    let trim: &[u8] = rules.trim.as_bytes();
    let start = bytes
        .iter()
        .position(|b| !trim.contains(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !trim.contains(b))
        .map_or(start, |i| i + 1);
    bytes[start..end].to_vec()
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

    /// The recipe's normalisation of a stream: [`normalise`], and then the
    /// scratch directory, as given and as the kernel resolves it, replaced
    /// by the token.
    fn normalise(&self, rules: &Normalise, bytes: Vec<u8>) -> Vec<u8> {
        let mut bytes = normalise(rules, bytes);
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
    verdict(registry, id, expected == actual, expected, actual)
}

/// The verdict when what stands for equality is a match the caller made
/// rather than the bytes themselves: the phpt runner's `--EXPECTF--` and
/// `--EXPECTREGEX--`, whose expectation is a pattern. `met` stands where
/// `expected == actual` stands in `compare`, so one place decides what the
/// registry means and no instrument diffs on its own.
pub fn verdict(
    registry: &Registry,
    id: &str,
    met: bool,
    expected: &[u8],
    actual: &[u8],
) -> Verdict {
    if met {
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
