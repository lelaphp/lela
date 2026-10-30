//! Whose a checkout is: one writing session per checkout, held by an
//! owner record (#3618 M1, #3621). A merge, cherry-pick, revert or rebase
//! commits the whole index, and an edit lands in whatever commit the
//! checkout makes next, so a write is only ever safe where one session
//! writes; which session made a change is what no sensor can read from
//! git. So the record says it before any change is made: `xtask worktree
//! add` names the session that lays the worktree down, and main's
//! checkout and the merge worktree are taken by `xtask worktree own`.
//! pre-commit and pre-merge-commit refuse a commit from another session
//! (`xtask worktree whose`), and `permit` refuses that session's writing
//! calls there (`where = "owned"`), so the owner's own `git merge
//! --abort` and commit pass and nothing else needs to know who started
//! what.
//!
//! The record names a session id, never a person: Claude Code's session,
//! as its hook carries it (`session_id`) and as it hands it to every
//! process a call starts (`CLAUDE_CODE_SESSION_ID`). A subagent's calls
//! carry the session that started it, so a session and its subagents are
//! one writer. A process with no session is a person's, as pre-push is the
//! person's run (harness-design.md §4.5): a person's commit is admitted
//! wherever it is made, and a person may name any session as an owner.
//!
//! The record is one file in the checkout's own git dir, so a linked
//! worktree's is its own and goes when `git worktree remove` takes the
//! worktree; a checkout with none is no session's, and nothing is refused
//! there.

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{git, reading};

/// The variable Claude Code sets for every process a session's call runs.
pub const SESSION_VARIABLE: &str = "CLAUDE_CODE_SESSION_ID";

/// The file in a checkout's git dir that names its owner.
const FILE: &str = "xtask-owner.json";

/// One Claude Code session, by its id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// The session `id` names; `None` for an empty one, which names none.
    pub fn new(id: &str) -> Option<SessionId> {
        let id = id.trim();
        (!id.is_empty()).then(|| SessionId(id.to_owned()))
    }

    /// The session this process runs in; `None` in a person's terminal.
    pub fn current() -> Option<SessionId> {
        SessionId::new(&std::env::var(SESSION_VARIABLE).ok()?)
    }

    /// A new session's id, a random UUID as Claude Code's `--session-id`
    /// takes one, for a session started by the harness whose worktree is
    /// recorded as its own before it starts.
    pub(crate) fn fresh() -> Result<SessionId> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0u8; 16];
        fs::File::open("/dev/urandom")
            .and_then(|mut random| random.read_exact(&mut bytes))
            .context("reading /dev/urandom for a session id")?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes
            .iter()
            .flat_map(|b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 0x0f)]])
            .map(char::from)
            .collect();
        let at = |range: std::ops::Range<usize>| &hex[range];
        Ok(SessionId(format!(
            "{}-{}-{}-{}-{}",
            at(0..8),
            at(8..12),
            at(12..16),
            at(16..20),
            at(20..32)
        )))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The owner record as it is kept.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    session: SessionId,
}

/// The checkout `place` stands in: the nearest directory at or above it
/// that holds a `.git`, canonical, a place inside a git dir standing in
/// the checkout that dir is under; `None` for a place in no checkout. A
/// place that does not exist yet stands where its nearest existing
/// ancestor does.
pub(crate) fn checkout_of(place: &Path) -> Option<PathBuf> {
    let existing = place.ancestors().find(|dir| dir.exists())?;
    let real = fs::canonicalize(existing).ok()?;
    real.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The owner record of `checkout`, the top of one, in its own git dir;
/// `None` where `.git` names no git dir.
fn record_file(checkout: &Path) -> Option<PathBuf> {
    git::own_dir(checkout).map(|dir| dir.join(FILE))
}

/// The session that owns `checkout`, the top of one; `None` for a
/// checkout no record names, or a directory that is no checkout. A record
/// that does not read is an error, never an owner nobody holds.
pub fn of(checkout: &Path) -> Result<Option<SessionId>> {
    let Some(file) = record_file(checkout) else {
        return Ok(None);
    };
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", file.display())),
    };
    let record: Record = serde_json::from_str(&text)
        .with_context(|| format!("{} is no owner record", file.display()))?;
    Ok(Some(record.session))
}

/// Record `session` as the owner of `checkout`, the top of one.
pub fn record(checkout: &Path, session: &SessionId) -> Result<()> {
    let file = record_file(checkout).with_context(|| {
        format!(
            "{} has no git dir to record an owner in",
            checkout.display()
        )
    })?;
    let record = Record {
        session: session.clone(),
    };
    let text = serde_json::to_string(&record).context("rendering the owner record")?;
    reading::replace(&file, format!("{text}\n").as_bytes())
}

/// Take `checkout`'s owner record away; whether there was one.
pub(crate) fn release(checkout: &Path) -> Result<bool> {
    match record_file(checkout) {
        Some(file) if file.exists() => crate::remove(&file).map(|()| true),
        _ => Ok(false),
    }
}

/// The session other than `caller` that owns `checkout`, whose writes
/// there are refused to `caller`; `None` where no session owns it or
/// `caller` does. A person, who names no session, is refused nowhere.
pub(crate) fn another(checkout: &Path, caller: Option<&SessionId>) -> Result<Option<SessionId>> {
    let Some(caller) = caller else {
        return Ok(None);
    };
    Ok(of(checkout)?.filter(|owner| owner != caller))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Tree;

    fn repository(name: &str) -> Tree {
        let dir = Tree::new(name).unwrap();
        let out = std::process::Command::new("git")
            .args(crate::scratch::UNHOOKED)
            .args(["init", "-q", "-b", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        dir
    }

    /// A record is the checkout's own: written and read back, refused to
    /// another session and to no person, and gone once released; a
    /// linked worktree's lives in its own git dir.
    #[test]
    fn a_checkout_s_owner_is_its_record_s_session_and_another_is_refused() {
        let dir = repository("owner-record");
        let (s1, s2) = (SessionId::new("s1").unwrap(), SessionId::new("s2").unwrap());
        assert_eq!(of(&dir).unwrap(), None);
        assert_eq!(another(&dir, Some(&s2)).unwrap(), None);
        record(&dir, &s1).unwrap();
        assert_eq!(of(&dir).unwrap(), Some(s1.clone()));
        assert_eq!(another(&dir, Some(&s1)).unwrap(), None);
        assert_eq!(another(&dir, Some(&s2)).unwrap(), Some(s1.clone()));
        assert_eq!(another(&dir, None).unwrap(), None);
        let inside = dir.join("a/b/new.txt");
        assert_eq!(checkout_of(&inside), Some(fs::canonicalize(&dir).unwrap()));
        assert!(release(&dir).unwrap());
        assert_eq!(of(&dir).unwrap(), None);
        assert!(!release(&dir).unwrap());
        fs::write(record_file(&dir).unwrap(), "not a record").unwrap();
        assert!(
            of(&dir).is_err(),
            "a record that does not read names no one"
        );
    }

    #[test]
    fn a_fresh_session_id_is_a_version_4_uuid() {
        let id = SessionId::fresh().unwrap();
        let parts: Vec<&str> = id.as_str().split('-').collect();
        let lengths: Vec<usize> = parts.iter().map(|p| p.len()).collect();
        assert_eq!(lengths, [8, 4, 4, 4, 12], "{id}");
        assert!(parts[2].starts_with('4'), "{id}");
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"), "{id}");
        assert_ne!(id, SessionId::fresh().unwrap());
        assert_eq!(SessionId::new("  "), None);
    }
}
