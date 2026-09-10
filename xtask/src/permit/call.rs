//! One intended tool call as the hook receives it, and where it lands: the
//! file relative to the workspace, and the checkout the call belongs to
//! when a worktree is one of its own.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::worktree::WORKTREES;

/// One intended tool call, as the hook receives it on stdin. `cwd` places
/// a call without a file in a worktree; `session_id` is what a reading
/// carries so denials count per session (§10 rule 6); the other keys of
/// the recorded shape (`permission_mode`, …) are carried by the JSON and
/// not read.
#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: ToolInput,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub session_id: String,
}

/// The fields of `tool_input` a rule can read: an `Edit`'s, a `Write`'s or
/// a `Bash` call's.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolInput {
    pub file_path: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
    #[serde(default)]
    pub replace_all: bool,
    pub content: Option<String>,
    pub command: Option<String>,
}

impl Call {
    /// The call from the hook's JSON.
    pub fn parse(text: &str) -> Result<Call> {
        serde_json::from_str(text).context("reading the tool call")
    }

    /// The `Write` that would put `content` at `path`.
    pub(super) fn write_of(path: &str, content: Option<&str>) -> Call {
        Call {
            tool_name: "Write".to_owned(),
            tool_input: ToolInput {
                file_path: Some(path.to_owned()),
                content: content.map(str::to_owned),
                ..ToolInput::default()
            },
            cwd: None,
            session_id: String::new(),
        }
    }

    /// The text the call would put in the file: an `Edit`'s `new_string` or
    /// a `Write`'s `content`.
    pub(super) fn written(&self) -> Option<&str> {
        self.tool_input
            .new_string
            .as_deref()
            .or(self.tool_input.content.as_deref())
    }

    pub(super) fn command(&self) -> Option<&str> {
        self.tool_input.command.as_deref()
    }

    /// The file the call touches, relative to the workspace root; `None`
    /// for a call without one or for a file outside the workspace, which
    /// no rule of the manifest describes.
    pub fn path(&self, root: &Path) -> Option<PathBuf> {
        let given = Path::new(self.tool_input.file_path.as_deref()?);
        let relative = if given.is_absolute() {
            let root = canonical(root);
            canonical(given).strip_prefix(&root).ok()?.to_path_buf()
        } else {
            given.to_path_buf()
        };
        let clean: PathBuf = relative
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        Some(clean)
    }

    /// The checkout the call belongs to: a worktree under
    /// [`WORKTREES`] when the file, or failing that the call's `cwd`, lies
    /// in one, else `root`. An agent's worktree is a checkout of its own
    /// (it has a `.git`), and a path in it is judged against that checkout,
    /// not as `.claude/**` of the main one. A track's worktree is named by
    /// its branch, whose name is a path, so the checkout is the first
    /// directory on the way down that has a `.git`.
    pub fn workspace(&self, root: &Path) -> PathBuf {
        let root = canonical(root);
        let worktrees = root.join(WORKTREES);
        let places = [self.tool_input.file_path.as_deref(), self.cwd.as_deref()];
        places
            .into_iter()
            .flatten()
            .map(|p| canonical(Path::new(p)))
            .find_map(|p| {
                let rest = p.strip_prefix(&worktrees).ok()?;
                let mut checkout = worktrees.clone();
                rest.components().find_map(|component| {
                    checkout.push(component);
                    checkout.join(".git").exists().then(|| checkout.clone())
                })
            })
            .unwrap_or(root)
    }

    /// What a reading names: the path, or the command's first line.
    pub(super) fn subject(&self, root: &Path) -> String {
        match (self.path(root), self.command()) {
            (Some(path), _) => path.display().to_string(),
            (None, Some(command)) => {
                let line = command.lines().next().unwrap_or_default();
                let mut head: String = line.chars().take(80).collect();
                if head.len() < line.len() {
                    head.push('…');
                }
                head
            }
            (None, None) => String::new(),
        }
    }
}

/// The path with its deepest existing ancestor resolved, so that a file
/// under `/tmp` and one under `/private/tmp` compare equal on macOS and a
/// file that does not exist yet still has a place.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
}
