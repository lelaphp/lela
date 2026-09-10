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
//! `harness-plugins/hooks/hooks.json`, which is a person's file under
//! `harness.self` and runs before a built `xtask` is certain to be there;
//! its three arms say what this module's variants say, in the hook's own
//! words.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

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
/// `harness-plugins/hooks/hooks.json`, a person's file.
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
            let _ = writeln!(lines, "{file}: {action}");
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
/// what is not a rendering.
pub fn command(root: &Path, files: &[String]) -> Result<()> {
    let answer = render(root, files)?;
    eprint!("{}", answer.said());
    print!("{}", answer.lines());
    match answer.refusal() {
        Some(refusal) => bail!("{refusal}"),
        None => Ok(()),
    }
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
