//! The changes to `harness.self`'s files that `permit --after` kept, and
//! `done`'s backstop over them (#1931, #1972). A file that moved while a
//! `Bash` call ran, and that the call's line cannot be read to write, is
//! kept as it stands, since a person's edit made while a call runs is lost
//! by putting it back ([`super::after`]). That leaves a change no one
//! answered for, so the sensor records it here as unattributed, one record
//! per file in the store of the checkout it is in, and until a person
//! commits it or restores it to `HEAD` it is one of `done`'s unmet
//! conditions ([`kept_changes`]).
//!
//! What is recorded is the kept change alone. A write the person approved
//! in the session is admitted by the approval ([`super::approval`]), one
//! `steer` made is steer's, and one the line can write is put back and
//! denied; none of them writes a record, so `done` tells an approved
//! change from an unattributed one by the record and nothing else.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::after::on_disk;
use crate::git;
use crate::reading::{self, Store};

/// One kept change: the record the sensor writes and the line `done`
/// lists while it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Kept {
    /// The file, relative to its checkout.
    path: String,
    /// Its blob before the call; `None` where it was not on disk.
    was: Option<String>,
    /// The call it moved under: its tool and the head of its line.
    call: String,
}

impl std::fmt::Display for Kept {
    /// The file, the call it moved under, what it was before and how a
    /// person settles it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = &self.path;
        write!(
            f,
            "harness.self: {path} changed while `{}` ran and was kept, its line not being one \
             that writes it; {}; a person commits it or restores it to HEAD \
             (`git checkout HEAD -- {path}`) (harness §4.4, #1931)",
            self.call,
            before_the_call(self.was.as_deref())
        )
    }
}

/// What a kept file was before the call, in the words the hook's context
/// and `done`'s line both say it in.
pub(super) fn before_the_call(was: Option<&str>) -> String {
    match was {
        Some(id) => format!("it was blob {id} before the call (`git cat-file -p {id}`)"),
        None => "it was not on disk before the call".to_owned(),
    }
}

/// Record the change `--after` kept at `path` in `checkout`: `was` the
/// blob before the call, `call` the call's words. The first change kept
/// stands for the file until it is settled, since a second one moved a
/// file the first had already moved, and a change that is already
/// settled, the file at `HEAD`'s blob as a merge leaves it, is no record.
pub(super) fn keep(checkout: &Path, path: &str, was: Option<&str>, call: &str) -> Result<()> {
    let file = record_of(&Store::of_workspace(checkout), path);
    if let Some(earlier) = read(&file)?
        && stands(checkout, &earlier)?
    {
        return Ok(());
    }
    let kept = Kept {
        path: path.to_owned(),
        was: was.map(str::to_owned),
        call: call.to_owned(),
    };
    if !stands(checkout, &kept)? {
        return crate::remove(&file);
    }
    let text = serde_json::to_string(&kept).context("rendering the kept change")?;
    reading::replace(&file, text.as_bytes())
}

/// Every kept change in `store` that still stands in `checkout`, by path.
/// A record settled since, the file committed or restored, is spent and
/// removed, as a call's record is once `--after` took it, so a later edit
/// of the file is not read as this change; this is the one write a
/// reader of the records makes. A store that cannot be read is an error,
/// never an empty list: `done` does not pass on no evidence.
pub(crate) fn kept_changes(store: &Store, checkout: &Path) -> Result<Vec<Kept>> {
    let mut standing = Vec::new();
    for file in crate::entries(&dir_of(store))? {
        if file.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(kept) = read(&file)? else {
            continue;
        };
        if stands(checkout, &kept)? {
            standing.push(kept);
        } else {
            crate::remove(&file)?;
        }
    }
    standing.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(standing)
}

/// Whether the change stands: the file is not at `HEAD`'s blob. A commit
/// of the file and a restore to `HEAD` both meet it; a commit of part of
/// it, or one followed by another edit, leaves it standing, the backstop
/// erring toward holding.
fn stands(checkout: &Path, kept: &Kept) -> Result<bool> {
    Ok(!at_head(checkout, &kept.path)?)
}

/// Whether the file at `path` in `checkout` stands at `HEAD`'s blob, as a
/// `git switch` or a merge leaves it: a change [`keep`] writes no record
/// of.
pub(super) fn at_head(checkout: &Path, path: &str) -> Result<bool> {
    Ok(on_disk(&checkout.join(path)) == blob_at(checkout, "HEAD", path)?)
}

/// The blob of `path` at `commit`, `None` where `commit` has no such
/// file.
pub(super) fn blob_at(checkout: &Path, commit: &str, path: &str) -> Result<Option<String>> {
    let blobs = git::blobs(checkout, commit, &[path])?;
    Ok(blobs.into_iter().next().map(|(_, id)| id))
}

/// The record at `file`, `None` where there is none.
fn read(file: &Path) -> Result<Option<Kept>> {
    let Some(text) = crate::read_if(file)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("reading the kept change at {}", file.display()))
}

fn dir_of(store: &Store) -> PathBuf {
    store.root().join("permit").join("kept")
}

/// One file's record, named by the blob id of its path, so every path,
/// a nested one included, is one plain name in one directory.
fn record_of(store: &Store, path: &str) -> PathBuf {
    dir_of(store).join(format!("{}.json", git::blob_id(path.as_bytes())))
}
