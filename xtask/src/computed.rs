//! What `computed run` answered, read once (#43; harness-design.md §5).
//! Four callers render regions and each read the exit and the lines by
//! hand: the interpretation is here, as [`Answer`], and the callers take
//! the value. `new-ext` and `worktree add` call [`render`] in process;
//! pre-commit calls `xtask regions`, which prints [`Answer::lines`], one
//! line per file, and refuses on anything but a rendering.
//!
//! What `computed` says, from its spec: exit 0 is every region fresh; 1
//! is a write, a loader failure or an untrusted region, one line per
//! region on stderr of the shape `path:line name loader state action`,
//! with `written` the action a re-stage reads and `untrusted` the state a
//! root without a trust grant answers with, tree regions still rendering;
//! 2 is a marker it could not read, the file line on stderr; and a
//! `computed` that is not on `PATH` never answers. An exit the spec does
//! not name is an error, not an answer. The grant is a
//! person's (§11), so an untrusted root is said with the line to run,
//! never run.
//!
//! One copy stays: the `SessionStart` line in
//! `harness-plugins/harness/hooks/hooks.json`, which is a person's file under
//! `harness.self` and runs before a built `xtask` is certain to be there;
//! its three arms say what this module's variants say, in the hook's own
//! words.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::git;

/// What `computed run` did, interpreted once.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// It ran under a trust grant: the files named, those with a region
    /// written, and its own lines.
    Rendered(Rendered),
    /// The root has no trust grant: its exec regions were kept, the tree
    /// regions among `rendered.written` still rendered, and the grant is
    /// the person's line [`Answer::lines`] prints.
    Untrusted { root: PathBuf, rendered: Rendered },
    /// A marker it could not read; its lines, which name the marker.
    BrokenMarker(String),
    /// No `computed` on `PATH`.
    NotInstalled,
}

/// The files named, those written, and what `computed` said about every
/// region.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Rendered {
    pub named: Vec<String>,
    pub written: Vec<String>,
    pub said: String,
}

/// The line that installs `computed`.
const INSTALL: &str = "computed did not run (cargo install computed)";

/// `computed run <files>` from `root`, the files named relative to it,
/// read once for `new-ext`, `worktree add` and, through `xtask regions`,
/// pre-commit. The one reader not here is the `SessionStart` line of
/// `harness-plugins/harness/hooks/hooks.json`, a person's file.
pub(crate) fn render(root: &Path, files: &[String]) -> Result<Answer> {
    let out = match Command::new("computed")
        .arg("run")
        .args(files)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) => out,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Answer::NotInstalled),
        Err(err) => return Err(err).context("running computed"),
    };
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    match out.status.code() {
        Some(0 | 1) => {}
        Some(2) => return Ok(Answer::BrokenMarker(said)),
        _ => bail!("computed run {}: {said}", files.join(" ")),
    }
    let written = files
        .iter()
        .filter(|file| {
            said.lines().any(|line| {
                line.strip_prefix(file.as_str())
                    .is_some_and(|rest| rest.starts_with(':') && rest.ends_with(" written"))
            })
        })
        .cloned()
        .collect();
    let rendered = Rendered {
        named: files.to_vec(),
        written,
        said,
    };
    let untrusted = said_untrusted(&rendered.said);
    Ok(if untrusted {
        Answer::Untrusted {
            root: root.to_path_buf(),
            rendered,
        }
    } else {
        Answer::Rendered(rendered)
    })
}

/// Whether a region line carries the state `untrusted`: the fourth word
/// of `path:line name loader state action`, never the region's name and
/// never a word of the loader's stderr, which is indented under it.
fn said_untrusted(said: &str) -> bool {
    said.lines().any(|line| {
        !line.starts_with(char::is_whitespace)
            && line.split_whitespace().nth(3) == Some("untrusted")
    })
}

/// The sentence for a root without a grant, with the line a person runs.
pub(crate) fn trust_line(root: &Path) -> String {
    format!(
        "this checkout is untrusted, so its exec regions are unrendered; a person runs, once (harness-design.md §11): computed trust {}",
        root.display()
    )
}

impl Answer {
    /// What was rendered, for the two answers that rendered anything.
    pub(crate) fn rendered(&self) -> Option<&Rendered> {
        match self {
            Answer::Rendered(rendered) | Answer::Untrusted { rendered, .. } => Some(rendered),
            Answer::BrokenMarker(_) | Answer::NotInstalled => None,
        }
    }

    /// One line per file named, `written` or `unchanged`, for the caller
    /// that re-stages; empty when nothing was rendered.
    pub(crate) fn lines(&self) -> String {
        let Some(rendered) = self.rendered() else {
            return String::new();
        };
        let mut lines = String::new();
        for file in &rendered.named {
            let action = if rendered.written.contains(file) {
                "written"
            } else {
                "unchanged"
            };
            writeln!(lines, "{file}: {action}").ok();
        }
        lines
    }

    /// What `computed` itself said, for the person reading.
    fn said(&self) -> &str {
        match self {
            Answer::BrokenMarker(said) => said,
            _ => self.rendered().map_or("", |rendered| &rendered.said),
        }
    }

    /// The refusal, when the answer is one: an untrusted root, a broken
    /// marker, or no `computed`; `None` for a rendering.
    pub(crate) fn refusal(&self) -> Option<String> {
        match self {
            Answer::Rendered(_) => None,
            Answer::Untrusted { root, .. } => Some(trust_line(root)),
            Answer::BrokenMarker(said) => Some(format!("a computed marker is broken:\n{said}")),
            Answer::NotInstalled => Some(INSTALL.to_owned()),
        }
    }
}

/// `xtask regions <files>`: the files rendered from `root`, each on its
/// own line on stdout, `computed`'s lines on stderr, and an error for
/// what is not a rendering. With `staged`, a region it would write that
/// reads an unstaged path is refused first, and nothing is rendered.
pub fn command(root: &Path, files: &[String], staged: bool) -> Result<()> {
    if staged {
        let refused = unstaged_reads(root, files)?;
        if !refused.is_empty() {
            bail!(
                "a region this commit would carry reads a path whose working copy it does not stage, \
                 so its render would carry that change (#2236); stage the path with it, or leave the \
                 path as it is staged:\n{}",
                refused.join("\n")
            );
        }
    }
    let answer = render(root, files)?;
    eprint!("{}", answer.said());
    print!("{}", answer.lines());
    match answer.refusal() {
        Some(refusal) => bail!("{refusal}"),
        None => Ok(()),
    }
}

/// The regions of `files` the commit would carry, those `computed` would
/// write and those of a file it stages, that read a path whose working
/// copy is not what is staged, one line each with the paths: `git diff` against the index the commit is written from, and
/// what is untracked and not ignored, a new directory as itself, which is
/// what a `tree` region lists and an input glob takes in. `computed`
/// renders from the working tree, so such a region's render would carry a
/// change the commit does not (#2236).
fn unstaged_reads(root: &Path, files: &[String]) -> Result<Vec<String>> {
    let mut unstaged: Vec<String> = Vec::new();
    for args in [
        &["diff", "--name-only", "-z"][..],
        &[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--directory",
        ],
    ] {
        unstaged.extend(git::listed(root, args)?);
    }
    if unstaged.is_empty() {
        return Ok(Vec::new());
    }
    let states: States = asked(root, "check", files)?;
    let unfresh: BTreeSet<(String, u64)> = states
        .files
        .into_iter()
        .flat_map(|file| {
            file.regions
                .into_iter()
                .filter(|region| region.state != State::Fresh)
                .map(move |region| (file.path.clone(), region.line))
        })
        .collect();
    // A file the commit stages carries its regions as they stand, a render
    // made earlier from the working tree among them.
    let staged = ["diff", "--cached", "--name-only", "-z"];
    let staged = BTreeSet::from_iter(git::listed(root, &staged)?);
    let carried = |paths: &[String]| -> Result<Vec<Region>> {
        let affected: Affected = asked(root, "affected", paths)?;
        Ok(affected
            .regions
            .into_iter()
            .filter(|r| {
                unfresh.contains(&(r.path.clone(), r.line)) || staged.contains(r.path.as_str())
            })
            .collect())
    };
    if carried(&unstaged)?.is_empty() {
        return Ok(Vec::new());
    }
    // The refusal names each region's paths: one question per path, asked
    // only once there is a refusal to name them in.
    let mut reads: BTreeMap<Region, Vec<&str>> = BTreeMap::new();
    for path in &unstaged {
        for region in carried(std::slice::from_ref(path))? {
            reads.entry(region).or_default().push(path);
        }
    }
    Ok(reads
        .into_iter()
        .map(|(r, paths)| {
            format!(
                "{}:{} {} reads {}",
                r.path,
                r.line,
                r.name,
                paths.join(", ")
            )
        })
        .collect())
}

/// What `computed check --format json` says of each file's regions.
#[derive(Deserialize)]
struct States {
    files: Vec<FileStates>,
}

#[derive(Deserialize)]
struct FileStates {
    path: String,
    regions: Vec<RegionState>,
}

#[derive(Deserialize)]
struct RegionState {
    line: u64,
    state: State,
}

/// A region's state: fresh, or one `computed run` acts on (stale,
/// unrendered, hand-edited, volatile), which the refusal does not tell
/// apart.
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum State {
    Fresh,
    #[serde(other)]
    Acted,
}

/// What `computed affected --format json` says: the regions that read a
/// path it was handed.
#[derive(Deserialize)]
struct Affected {
    regions: Vec<Region>,
}

/// One region, by its file and the line of its opener.
#[derive(Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct Region {
    path: String,
    line: u64,
    name: String,
}

/// `computed <command> <args> --format json` from `root`, its one JSON
/// document; exit 0, and 1 for a region not fresh, are answers, and any
/// other exit is `computed`'s refusal, its stderr carried.
fn asked<T: DeserializeOwned>(root: &Path, command: &str, args: &[String]) -> Result<T> {
    let out = Command::new("computed")
        .arg(command)
        .args(args)
        .args(["--format", "json"])
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running computed {command}"))?;
    if !matches!(out.status.code(), Some(0 | 1)) {
        bail!(
            "computed {command} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    serde_json::from_slice(&out.stdout).with_context(|| {
        format!(
            "computed {command} --format json: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_file_is_named_and_a_file_whose_prefix_matches_is_not() {
        let rendered = Rendered {
            named: vec!["AGENTS.md".to_owned(), "ext/AGENTS.md".to_owned()],
            written: vec!["AGENTS.md".to_owned()],
            said: String::new(),
        };
        let answer = Answer::Rendered(rendered);
        assert_eq!(
            answer.lines(),
            "AGENTS.md: written\next/AGENTS.md: unchanged\n"
        );
        assert_eq!(answer.refusal(), None);
    }

    #[test]
    fn untrusted_is_the_state_column_not_a_regions_name() {
        assert!(said_untrusted(
            "a.md:1 x exec untrusted skipped; run `computed trust`\n"
        ));
        assert!(!said_untrusted("a.md:1 x exec stale written\n"));
        assert!(!said_untrusted("a.md:1 untrusted exec stale written\n"));
        assert!(!said_untrusted(
            "a.md:1 x exec volatile failed; body kept\n    the loader said untrusted here\n"
        ));
    }
}
