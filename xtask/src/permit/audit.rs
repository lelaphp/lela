//! The audit record of the person's approvals (#2247, harness-design.md
//! §4.4), written by `permit` itself as #2281 ruled: one entry per change
//! an approval admitted ([`super::approval`]), carrying the approval whole
//! (the question and its options, the answer and notes, the file, the diff
//! and its hash, the file's blob before and after) beside the session, the
//! tool, the ids of the question and of the call, the time and the commit
//! the checkout stood on.
//!
//! When: an `Edit` or a `Write` at the call, since its write is known
//! before it runs and no hook follows it; a `Bash` line after it ran, from
//! `permit --after`, once what it wrote stands as the approval leaves it,
//! since before it runs only an approval pending for it is known. The two
//! adapters judge one `Edit` each, so an entry is named by the call and
//! the file, and the first one written stands.
//!
//! Where: the store of the main worktree, under `permit/approvals/`, one
//! file per entry. Not a committed file beside the change, as #2247 first
//! had it: an entry written at the call stands before the person has let
//! the call run, so one for a call they then refused would be an untracked
//! file in a checkout every session shares, which `git worktree remove`
//! refuses and a pathspec commit leaves behind; and not the worktree's own
//! store, which `worktree retire` removes with it. The commit that carried
//! a change is the one whose tree first holds its `after` blob, `git log
//! --find-object=<after> -- <file>`, and none where the change never stood.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use super::approval::Admission;
use super::call::Call;
use crate::reading::{self, Store};
use crate::{git, track};

/// One entry: the admission whole and the call it admitted.
#[derive(Debug, Serialize)]
struct Entry<'a> {
    #[serde(flatten)]
    admission: &'a Admission,
    session: &'a str,
    tool: &'a str,
    /// The admitted call's `tool_use_id`; the question's is `ask`.
    call: &'a str,
    time: String,
    /// The commit the file's checkout stood on when the entry was written.
    head: String,
}

/// Write the entry for `admission`, which admitted `call` on a file of
/// `checkout`, unless that call's entry for that file stands already.
pub(super) fn record(checkout: &Path, call: &Call, admission: &Admission) -> Result<()> {
    let file = entry_path(checkout, call, &admission.file);
    if file.exists() {
        return Ok(());
    }
    let entry = Entry {
        admission,
        session: &call.session_id,
        tool: &call.tool_name,
        call: &call.tool_use_id,
        time: reading::now()?.to_string(),
        head: git::rev_parse(checkout, &["HEAD"]).unwrap_or_default(),
    };
    let text = serde_json::to_string_pretty(&entry).context("rendering the approval's entry")?;
    reading::replace(&file, format!("{text}\n").as_bytes())
}

/// Where the entry for `call`'s change to `file` goes: the main worktree's
/// store, named by the session, the call and the file, so the two adapters
/// judging one call name one entry.
fn entry_path(checkout: &Path, call: &Call, file: &str) -> PathBuf {
    let main = track::main_worktree(checkout).unwrap_or_else(|_| checkout.to_path_buf());
    let name = [call.session_id.as_str(), &call.tool_use_id, file].join("\n");
    approvals(&main).join(format!("{}.json", git::blob_id(name.as_bytes())))
}

/// The directory of the entries in the store of the checkout at `root`.
pub(crate) fn approvals(root: &Path) -> PathBuf {
    Store::of_workspace(root)
        .root()
        .join("permit")
        .join("approvals")
}
