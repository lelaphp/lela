//! One intended tool call as the hook receives it, and where it lands: the
//! file relative to the workspace, the workspace whose manifest judges the
//! call, and the checkout that workspace is in.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::manifest;

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

    /// The workspace whose manifest judges the call: the nearest ancestor
    /// of the file, or failing that of the call's `cwd`, that carries a
    /// `seams.toml` of its own under `root`, else `root`. A directory with
    /// a manifest is a workspace of its own, and a call into it is judged
    /// by that manifest (#57): a track's worktree under
    /// `.claude/worktrees/<branch>`, whose files are judged against that
    /// checkout and not as `.claude/**` of the main one, and a fixture
    /// under `xtask/fixtures/**` that carries one, whose sources are what
    /// its own rules describe and not what the workspace's do. One rule
    /// covers both, and a fixture without a manifest is the workspace's.
    pub fn workspace(&self, root: &Path) -> PathBuf {
        let root = canonical(root);
        self.nested(&root).unwrap_or(root)
    }

    /// The workspace of its own the call lands in, `None` for a call
    /// judged by `root`'s manifest. The file places the call; a call
    /// without one is placed by its `cwd` in a checkout alone, the nearest
    /// ancestor with a manifest that also has a `.git`: a `Bash` call whose
    /// shell stands in a fixture directory is the checkout's, whose
    /// manifest carries the shell rules, since a fixture's manifest
    /// describes the fixture's sources and not what a shell may run.
    pub(super) fn nested(&self, root: &Path) -> Option<PathBuf> {
        let root = canonical(root);
        let file = self.tool_input.file_path.as_deref();
        let cwd = self.cwd.as_deref();
        file.and_then(|f| nearest_under(&root, &canonical(&root.join(f)), &[manifest::FILE]))
            .or_else(|| {
                let cwd = canonical(&root.join(cwd?));
                nearest_under(&root, &cwd, &[manifest::FILE, ".git"])
            })
    }

    /// The checkout the call's workspace is in: the nearest ancestor of it
    /// under `root` that has a `.git`, else `root`. A worktree is one; a
    /// fixture workspace is not, so the build that judges by its manifest
    /// is held to the checkout's, the one it could have been compiled
    /// against, and its readings go to the checkout's store.
    pub(super) fn checkout(&self, root: &Path) -> PathBuf {
        let root = canonical(root);
        self.nested(&root)
            .and_then(|workspace| nearest_under(&root, &workspace, &[".git"]))
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

/// The nearest directory at or above `place` that is strictly under
/// `root` and holds every one of `names`: a workspace by its manifest, a
/// checkout by its `.git`. `root` itself is never the answer, being the
/// fallback; a place outside `root` has none.
fn nearest_under(root: &Path, place: &Path, names: &[&str]) -> Option<PathBuf> {
    place
        .ancestors()
        .take_while(|dir| *dir != root)
        .find(|dir| dir.starts_with(root) && names.iter().all(|name| dir.join(name).exists()))
        .map(Path::to_path_buf)
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
