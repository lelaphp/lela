//! `xtask packages propose`: the proposal of the list (R9, R13). It asks
//! the registry for its most-installed packages on the day it runs, takes
//! each one's newest stable version, and pins it as a run reads a row:
//! the lane fetches the source at the registry's commit and digests it,
//! resolves the lock of its development dependencies and digests that
//! ([`lane::propose`]). The rows the list already carries are kept as
//! they stand, its text first and unchanged, and the new rows follow
//! under a comment naming the day the registry's counts were taken, so
//! `steer` reads the proposal as rows added and nothing changed. A
//! package the registry gives no git source commit for, or whose source
//! or lock the lane could not take, is named on stdout and left out.
//!
//! The registry is asked through `curl`, as `lela-xtask`, with a pause
//! before each request, and the first answer that is not one, a rate
//! limit's among them, stops the proposal rather than being retried.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use php_lanes_packages as lane;
use serde_json::{Map, Value};

use crate::manifest::Manifest;

/// How many of the registry's most-installed packages a proposal takes.
const COUNT: usize = 500;

/// How long the proposal waits before each request of the registry.
const PAUSE: Duration = Duration::from_secs(1);

/// How curl asks: silent but for its errors, a failing answer an error,
/// redirects followed, a minute at most, and as a client named for xtask
/// with nothing of the person running it.
const CURL: [&str; 7] = [
    "-sS",
    "--fail",
    "-L",
    "--max-time",
    "60",
    "-A",
    "lela-xtask",
];

/// What one proposal was asked for.
#[derive(Debug, Clone, clap::Args)]
pub struct Ask {
    /// Where the proposal is written: `packages.toml`, and each new row's
    /// lock under `packages/`, laid out as the list's directory is.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// The list whose rows it keeps; default `data/packages.toml`.
    #[arg(long, value_name = "FILE")]
    pub list: Option<PathBuf>,
    /// The registry asked, which answers as Packagist does.
    #[arg(long, value_name = "URL", default_value = "https://packagist.org")]
    pub registry: String,
}

/// Write the proposal `ask` names, printing each package it proposes or
/// leaves out.
pub(crate) fn command(root: &Path, manifest: &Manifest, ask: &Ask) -> Result<()> {
    let list = root.join(ask.list.as_deref().unwrap_or(Path::new(super::LIST)));
    let standing = if list.is_file() {
        crate::read(&list)?
    } else {
        String::new()
    };
    let kept: Vec<String> = (super::parse(&standing)?.into_iter())
        .map(|row| row.pinned.name)
        .collect();
    let pin = manifest.pin()?;
    let oracle = pin.oracle();
    oracle.asserted()?;
    let mut sides = super::Sides {
        oracle: &oracle,
        php: &pin.oracle.path,
        engine: None,
    };
    let day = crate::reading::now()?
        .date
        .context("the clock gave no date")?;
    let (mut rows, mut proposed) = (String::new(), 0);
    for name in popular(&ask.registry)?.iter().filter(|n| !kept.contains(n)) {
        match row(name, ask, &mut sides)? {
            Ok(row) => {
                println!("proposed {name}");
                rows.push_str(&row);
                proposed += 1;
            }
            Err(why) => println!("left out {name}: {why}"),
        }
    }
    let written = ask.out.join("packages.toml");
    let text = format!(
        "{standing}{}# {proposed} row{} proposed from {}'s install counts of {day}, \
         its {COUNT} most-installed packages.\n{rows}",
        if standing.is_empty() { "" } else { "\n" },
        crate::reading::plural(proposed),
        ask.registry
    );
    fs::write(&written, text).with_context(|| format!("writing {}", written.display()))?;
    println!("{proposed} rows proposed in {}", written.display());
    Ok(())
}

/// The names of the registry's most-installed packages, its own order.
fn popular(registry: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut next = Some(format!("{registry}/explore/popular.json?per_page=100"));
    while let Some(url) = next.take()
        && names.len() < COUNT
    {
        let page: Value = serde_json::from_str(&fetch(&url)?)?;
        let listed = page["packages"].as_array().into_iter().flatten();
        names.extend(listed.filter_map(|p| p["name"].as_str().map(str::to_owned)));
        next = page["next"].as_str().map(str::to_owned);
    }
    names.truncate(COUNT);
    Ok(names)
}

/// The registry's answer at `url`, after the pause.
fn fetch(url: &str) -> Result<String> {
    std::thread::sleep(PAUSE);
    let out = std::process::Command::new("curl")
        .args(CURL)
        .arg(url)
        .output()
        .context("running curl")?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        bail!("the proposal stopped at {url}: {}", said.trim());
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// The row proposed for `name`, its lock kept under `ask.out`, or why it
/// is left out.
fn row(name: &str, ask: &Ask, sides: &mut super::Sides<'_>) -> Result<Result<String, String>> {
    let url = format!("{}/p2/{name}.json", ask.registry);
    let metadata: Value = serde_json::from_str(&fetch(&url)?)?;
    let Some(version) = stable(name, &metadata) else {
        return Ok(Err("the registry names no stable version".to_owned()));
    };
    let field = |key: &str| version.get(key).and_then(Value::as_str);
    let source = |key: &str| version.get("source").and_then(|s| s[key].as_str());
    let (Some(v), Some("git"), Some(repository), Some(commit)) = (
        field("version"),
        source("type"),
        source("url"),
        source("reference"),
    ) else {
        return Ok(Err("the registry gives no git source commit".to_owned()));
    };
    let requires: Vec<String> = (["require", "require-dev"].iter())
        .filter_map(|key| version.get(*key)?.as_object())
        .flat_map(|names| names.keys().cloned())
        .collect();
    let lock = format!("packages/{name}.lock");
    let kept = ask.out.join(&lock);
    fs::create_dir_all(kept.parent().unwrap_or(&ask.out))?;
    let candidate = lane::Candidate {
        repository,
        commit,
        version: v,
        lock: &kept,
        requires: &requires,
    };
    let work = crate::scratch::Tree::new("packages-propose")?;
    let proposed = match lane::propose(&candidate, &work, sides) {
        Ok(proposed) => proposed,
        Err(why) => return Ok(Err(why)),
    };
    let mut row = String::from("\n[[package]]\n");
    let fields = [
        ("name", name),
        ("version", v),
        ("repository", repository),
        ("commit", commit),
        ("digest", &proposed.digest),
        ("lock", &lock),
        ("lock_digest", &proposed.lock_digest),
    ];
    for (key, value) in fields
        .into_iter()
        .chain(proposed.runner.map(|r| ("runner", r)))
    {
        writeln!(row, "{key} = {}", toml::Value::from(value))?;
    }
    if !proposed.needs.is_empty() {
        writeln!(row, "needs = {}", toml::Value::from(proposed.needs))?;
    }
    Ok(Ok(row))
}

/// The newest stable version of `name` in the registry's metadata, its
/// entries expanded from Composer's minified form, where each carries
/// only what differs from the one before and `__unset` drops a key.
fn stable(name: &str, metadata: &Value) -> Option<Map<String, Value>> {
    let mut expanded = Map::new();
    for entry in metadata["packages"][name].as_array()? {
        for (key, value) in entry.as_object()? {
            if value == "__unset" {
                expanded.remove(key);
            } else {
                expanded.insert(key.clone(), value.clone());
            }
        }
        let normalized = expanded.get("version_normalized").and_then(Value::as_str);
        if normalized.is_some_and(|v| !v.contains('-')) {
            return Some(expanded);
        }
    }
    None
}
