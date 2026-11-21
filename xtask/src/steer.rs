//! `xtask steer`, the sanctioned writer of the harness's own files
//! (harness-design.md §4.4, §11). `permit` denies an agent the `Edit` and
//! the `Write` on `seams.toml`, `oracle.toml` and the divergence registry,
//! because an agent that edits them can lower the bar it is measured
//! against: an `exempt` on the rule that denies it, a ceiling over the
//! lines it just wrote, an edge that admits the crate it wants, a
//! divergence flipped to `accepted` so a disagreement with the oracle
//! stops being one.
//!
//! What that reasoning forbids is a *change to what already binds*, and
//! not every write to the file. A `[[crate]]` row for a crate that does
//! not exist yet brings one under a ceiling; a `[regime.dump]` block the
//! pin has never carried adds a recipe beside the ones it has; a
//! `porting = true` on a row that carries a name and a budget says one
//! thing more about that crate and moves neither. None of the three can
//! loosen what stands, because nothing stood on them. So this instrument
//! applies a proposed file only when it is **additive**: every value the
//! tracked file carries stands unchanged in the proposal, at the path it
//! stood at and in the order it stood in, and what the proposal adds
//! beside them is new. One changed ceiling, one widened `to`, one deleted
//! rule, one edited `ini`, one dropped key, one row moved past another,
//! and the whole proposal is refused, named at the path that changed.
//!
//! A key added to a row the file already carries is an addition like any
//! other (harness-design.md ledger 51): an array-of-tables row recurses
//! key by key exactly as a table does, so `porting = true` on `[[crate]]`
//! `ext/standard` lands the way `[regime.dump]` lands. What that widens
//! is stated rather than hidden: most `[[rule]]` rows carry no `exempt`,
//! so an agent may propose one on the rule that denies it, and
//! a `feature` added to an extension's row takes that extension out of
//! the per-commit lane. Both are additions here, and both are named — in
//! the reading, in what this instrument prints and in the commit's diff —
//! so a rule that gains an exemption is as visible as one that loses it,
//! and a person reads about it in the same commit.
//!
//! The judge is the whole of the safety, since nothing else stands between
//! an agent and these files: `permit` judges tool calls, and an instrument
//! writing from Rust makes none, exactly as `computed run`, `new-ext` and
//! `witness --accept` do. It is deliberately blunt. It refuses a tightening
//! as readily as a loosening, since a lowered ceiling is still a person's
//! call on what the harness holds; the agent's half is the row that was
//! never there.
//!
//! A merge is the one place two additions meet. Two branches that each
//! append a ruled row to the registry conflict at the second's merge, and
//! the working file then holds git's markers, which no reader parses. So
//! while `MERGE_HEAD` stands with the file unmerged, a proposal is judged
//! against the blobs git keeps for the conflict (`Ground::Merge`): it is
//! written when it is the union of what each side carries over the merge
//! base, and the reading names what each side brings. A side's own change
//! over the base, a reworded value or a dropped row, was settled where that
//! side landed, so the union carries it as it stands there. Two sides
//! that changed one path differently, and a change, drop or row beyond
//! what either side carries, are refused by the path.
//!
//! A file both sides changed that git merged cleanly is judged the same
//! way: its working copy is already the merged text, which no
//! side committed, so pre-commit refuses an agent's merge of it until a
//! steer reading covers it, and judged against that copy a
//! proposal of the same text adds nothing. The ground is then the merge
//! base's file, `HEAD`'s and `MERGE_HEAD`'s, read from their commits, and
//! the proposal is held as well to the text git's own three-way merge of
//! those three blobs gives (`Merged::Clean`): a proposal that differs from
//! it by a byte, and a merge git took cleanly where that three-way merge
//! conflicts, as `-X ours` takes one, are refused. Where `MERGE_HEAD`
//! changed comments alone, the union brings no value and its reading
//! still passes, since the merged text is neither side's commit. A
//! criss-cross merge, of several bases, was merged over a base no commit
//! carries, and is refused whatever the proposal (`Ground::Crossed`).
//!
//! The packages lane's list is the one file whose rows are not walked
//! key by key ([`Rows::Whole`]): a row is keyed by its package's name and
//! every other key is the row's value, so an agent adds a row and a person
//! changes or drops one (R13, spec lelaphp/lela#32), a key added to a row
//! as much a change as a version moved. Where no list stands, a proposal
//! of rows lays it down, as any of these files is laid down where none is.
//!
//! An addition is not a decision. What a new row should say is a person's,
//! settled on a ticket (`AGENTS.md`, *Documentation and where code lives*);
//! this instrument is how a settled row reaches the file, and the reading
//! it writes is what says so afterwards.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml::Value;

use crate::divergence::{self, Registry};
use crate::git;
use crate::key::Snapshot;
use crate::manifest::{self, Manifest};
use crate::oracle::Pin;
use crate::reading::{Reading, Status};
use crate::scratch::Tree;

/// The instrument name every reading of steer carries.
pub(crate) const INSTRUMENT: &str = "steer";

/// The rule whose scope names the harness's own files, the ones `permit`
/// denies the agent.
const RULE: &str = "harness.self";

/// Whether this instrument may write `file`: a TOML file the scope of the
/// manifest's `harness.self` names as written, not by a glob, and does not
/// exempt. That scope is the one list of the harness's own files,
/// and every reader of it asks here: `steer` before anything is read,
/// since steering is for the files no other hand may touch and every other
/// file is an `Edit` away, `permit`'s guard on what steer wrote, and a
/// ticket's sanctioned write.
pub(crate) fn steerable(manifest: &Manifest, file: &str) -> bool {
    let path = Path::new(file);
    path.extension().is_some_and(|ext| ext == "toml")
        && manifest
            .rule(RULE)
            .is_some_and(|rule| rule.scope.iter().any(|named| named == file) && !rule.exempts(path))
}

/// The manifest as `HEAD` carries it, whose `harness.self` says what
/// steer may write: the committed one, which no write steer judges has
/// moved, and which parses while a merge leaves the working file
/// conflicted.
pub(crate) fn binding(root: &Path) -> Result<Manifest> {
    let text = git::show(root, "HEAD", manifest::FILE)?.with_context(|| {
        format!(
            "HEAD carries no {}, so no {RULE} rule names a file steer writes",
            manifest::FILE
        )
    })?;
    Manifest::parse(&text)
}

/// The keys an array-of-tables row is identified by, in order: a proposal
/// that adds a row whose identity an existing row already carries is
/// refused, since two rows of one name is how a row is shadowed rather
/// than added. An `[[owed]]` row is one per instrument, so its
/// `instrument` names it, and a key added to one is an addition.
const IDENTITY: [&str; 4] = ["id", "name", "from", "instrument"];

/// How the judge walks the rows of one file's arrays of tables, the file
/// deciding which ([`Rows::of`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Rows {
    /// Key by key, a key a row did not carry being an addition (ledger
    /// 51): the manifest's, the pin's and the registry's rows.
    #[default]
    Keyed,
    /// Each row one value under its identity, so a key added, changed or
    /// dropped is the row changed: the packages list's rows (R13).
    Whole,
}

impl Rows {
    fn of(file: &str) -> Rows {
        if file == crate::packages::LIST {
            Rows::Whole
        } else {
            Rows::Keyed
        }
    }
}

/// What a proposal does to the tracked file, as the judge reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing that stands was touched; the additions, by their path in
    /// the file, in the order the walk found them.
    Additive(Vec<String>),
    /// A merge stands with the file conflicted, and the proposal is the
    /// union of what each side carries over the merge base: `head`, what
    /// `HEAD` adds or changes over it, and `merged`, what the write brings
    /// to `HEAD`'s file, each a value `MERGE_HEAD` adds or changes over the
    /// base (`Ground::Merge`).
    Union {
        head: Vec<String>,
        merged: Vec<String>,
    },
    /// Something that stands was changed or removed; each one named.
    Refused(Vec<String>),
}

impl Verdict {
    /// What the write adds to the tracked file, none for a refused one.
    pub fn additions(&self) -> &[String] {
        match self {
            Verdict::Additive(added) | Verdict::Union { merged: added, .. } => added,
            Verdict::Refused(_) => &[],
        }
    }
}

/// Judge one proposal against `file` as it would replace it: additive when
/// every value the tracked file carries stands unchanged in the proposal.
pub fn judge(file: &str, tracked: &str, proposed: &str) -> Result<Verdict> {
    let walk = Walk::over(Rows::of(file), tracked, proposed)?;
    Ok(if walk.refused.is_empty() {
        Verdict::Additive(walk.added)
    } else {
        Verdict::Refused(walk.refused)
    })
}

/// What a proposal is judged against, read once from the checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ground {
    /// The tracked file as it stands.
    File(String),
    /// A merge stands (`MERGE_HEAD`) and both sides changed the file over
    /// their base: the merge base's file (empty where it carries no such
    /// file), `HEAD`'s and `MERGE_HEAD`'s, and how git left the merge of
    /// them. Unmerged in the index, its working copy holding git's conflict
    /// markers, which no reader parses, they are the blobs git keeps for
    /// the conflict (stages 1, 2 and 3); two branches that each append a
    /// ruled row conflict this way at the second's merge. Merged cleanly,
    /// they are the three commits' files.
    Merge {
        base: String,
        head: String,
        merged: String,
        git: Merged,
    },
    /// A merge stands whose `HEAD` and `MERGE_HEAD` have several merge
    /// bases, a criss-cross, and git merged the file cleanly into a text
    /// neither side carries, the working copy here: git merged it over a
    /// base of its own making that no commit carries, so no union of the
    /// sides can be read, and every proposal is refused; a person commits
    /// the merge.
    Crossed(String),
}

/// How git left the file of a merge both sides changed, read once in
/// [`Ground::of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Merged {
    /// Unmerged in the index: what the proposal is, is the union alone.
    Conflicted,
    /// Merged cleanly, and this is the text `git merge-file` gives of the
    /// three blobs, git's own three-way merge: a proposal is that text,
    /// byte for byte, as well as the union of what each side carries.
    Clean(String),
    /// Merged cleanly where `git merge-file` of the three blobs conflicts:
    /// a strategy option such as `-X ours` took one side's hunk and dropped
    /// the other's, so what git staged is no three-way merge of the sides,
    /// and nothing is written over it.
    Overridden,
}

impl Merged {
    /// What git's own three-way merge of `head` and `merged` over `base`
    /// gives, run in `root` over the three written to a scratch tree,
    /// since `git merge-file` reads files: the text where it merges
    /// cleanly, [`Merged::Overridden`] where it conflicts or git cannot
    /// merge the three at all, so that what it cannot vouch for is never
    /// written over.
    fn of(root: &Path, base: &str, head: &str, merged: &str) -> Result<Merged> {
        let tree = Tree::new("steer-merge")?;
        let mut at = Vec::new();
        for (name, text) in [("head", head), ("base", base), ("merged", merged)] {
            let path = tree.join(name);
            fs::write(&path, text)
                .with_context(|| format!("writing {name}'s file to {}", path.display()))?;
            at.push(path.to_string_lossy().into_owned());
        }
        let args = [
            &["merge-file", "-p"][..],
            &at.iter().map(String::as_str).collect::<Vec<_>>(),
        ]
        .concat();
        Ok(git::git(root, &args).map_or(Merged::Overridden, Merged::Clean))
    }
}

impl Ground {
    /// The ground of `file` in the checkout at `root`, whose text, outside
    /// a merge in which both sides changed it, is `text`: the working copy
    /// steer replaces, or the one `permit --after` kept from before the
    /// call.
    pub(crate) fn of(root: &Path, file: &str, text: String) -> Result<Ground> {
        if !git::has_ref(root, "MERGE_HEAD") {
            return Ok(Ground::File(text));
        }
        let unmerged = git::git(root, &["ls-files", "--unmerged", "--", file])?;
        if unmerged.is_empty() {
            return Ground::clean(root, file, text);
        }
        // Each entry is `<mode> <blob> <stage>\t<path>`; a file both sides
        // added has no stage 1, and its base carries nothing.
        let staged = |n: u8| unmerged.lines().any(|e| e.contains(&format!(" {n}\t")));
        let stage = |n: u8| git::git(root, &["show", &format!(":{n}:{file}")]);
        Ok(Ground::Merge {
            base: if staged(1) { stage(1)? } else { String::new() },
            head: stage(2)?,
            merged: stage(3)?,
            git: Merged::Conflicted,
        })
    }

    /// The ground of `file` in a merge git merged it in cleanly:
    /// where `HEAD` and `MERGE_HEAD` have one merge base, or none, and
    /// both changed the file over it, differently, the three commits'
    /// files and the text git's three-way merge of them gives; otherwise
    /// the merge took one side's file whole, or the sides agree, and
    /// `text`, the working copy, is the file as it stands. A criss-cross
    /// merge, of several bases, whose working copy is neither side's file
    /// is [`Ground::Crossed`].
    fn clean(root: &Path, file: &str, text: String) -> Result<Ground> {
        let (head, merged) = (
            git::show(root, "HEAD", file)?,
            git::show(root, "MERGE_HEAD", file)?,
        );
        let (Some(head), Some(merged)) = (head, merged) else {
            return Ok(Ground::File(text));
        };
        // Histories with no common commit merge over an empty base, as git
        // merges them.
        let bases =
            git::git(root, &["merge-base", "--all", "HEAD", "MERGE_HEAD"]).unwrap_or_default();
        let base = match bases.split_whitespace().collect::<Vec<_>>()[..] {
            [] => None,
            [base] => git::show(root, base, file)?,
            _ if head == merged || text == head || text == merged => {
                return Ok(Ground::File(text));
            }
            _ => return Ok(Ground::Crossed(text)),
        };
        if head == merged
            || [&head, &merged]
                .iter()
                .any(|side| base.as_ref() == Some(side))
        {
            return Ok(Ground::File(text));
        }
        let base = base.unwrap_or_default();
        let git = Merged::of(root, &base, &head, &merged)?;
        Ok(Ground::Merge {
            base,
            head,
            merged,
            git,
        })
    }

    /// The file a proposal replaces, as its reader is held to it: `HEAD`'s
    /// while a merge stands.
    fn tracked(&self) -> &str {
        match self {
            Ground::File(tracked)
            | Ground::Crossed(tracked)
            | Ground::Merge { head: tracked, .. } => tracked,
        }
    }

    /// Judge one proposal on this ground. In a merge it is held to both
    /// sides as to one tracked file each, and to the merge base: where the
    /// proposal differs from one side's file it carries what the other
    /// side changed over the base, as it stands there, and nothing the
    /// first side changed; what it adds to one side's file is what the
    /// other adds over the base; and a path the two sides changed
    /// differently, or one inside what the other changed, is refused by
    /// both sides' paths.
    pub(crate) fn judge(&self, file: &str, proposed: &str) -> Result<Verdict> {
        let (base, head, merged, git) = match self {
            Ground::File(tracked) => return judge(file, tracked, proposed),
            Ground::Crossed(_) => {
                return Ok(Verdict::Refused(vec![
                    "HEAD and MERGE_HEAD have several merge bases, so git merged the file over a \
                     base no commit carries and no union of the sides can be read; a person \
                     commits the merge"
                        .to_owned(),
                ]));
            }
            Ground::Merge {
                base,
                head,
                merged,
                git,
            } => (base, head, merged, git),
        };
        let over = |tracked: &str, proposed: &str| Walk::over(Rows::of(file), tracked, proposed);
        let (ours, theirs) = (over(base, head)?, over(base, merged)?);
        let (over_head, over_merged) = (over(head, proposed)?, over(merged, proposed)?);
        let apart = over(head, merged)?;
        let mut refused = Vec::new();
        for p in ours.changes() {
            for q in theirs.changes().filter(|q| overlaps(p, q)) {
                if p != q || apart.changes().any(|d| overlaps(d, p)) {
                    refused.push(format!("{p} on HEAD and {q} on MERGE_HEAD"));
                }
            }
        }
        // A change to one side's file is the other side's own over the
        // base, the proposal standing where that side stands.
        let taken = |p: &String, by: &Walk, other: &Walk, over_by: &Walk| {
            by.refused.contains(p) && !other.refused.contains(p) && !over_by.refused.contains(p)
        };
        let (brought, changed): (Vec<_>, Vec<_>) = (over_head.refused.iter().cloned())
            .partition(|p| taken(p, &theirs, &ours, &over_merged));
        refused.extend(changed);
        (over_merged.refused.iter())
            .filter(|p| !taken(p, &ours, &theirs, &over_head))
            .for_each(|p| refused.push(p.clone()));
        let beyond = |added: &[String], side: &[String]| -> Vec<String> {
            (added.iter().filter(|p| !side.contains(p)))
                .map(|p| format!("{p}, which neither side adds over the base"))
                .collect()
        };
        refused.extend(beyond(&over_head.added, &theirs.added));
        refused.extend(beyond(&over_merged.added, &ours.added));
        match git {
            Merged::Conflicted => {}
            Merged::Clean(text) if text == proposed => {}
            Merged::Clean(_) => refused.push(
                "the proposal is not the text git's three-way merge of the base's, HEAD's and \
                 MERGE_HEAD's files gives, which git merged cleanly"
                    .to_owned(),
            ),
            Merged::Overridden => refused.push(
                "git merged the file cleanly where its three-way merge of the base's, HEAD's and \
                 MERGE_HEAD's files conflicts, so a strategy option took one side's hunk over \
                 the other's"
                    .to_owned(),
            ),
        }
        refused.sort();
        refused.dedup();
        Ok(if refused.is_empty() {
            Verdict::Union {
                head: ours.changes().cloned().collect(),
                merged: over_head.added.into_iter().chain(brought).collect(),
            }
        } else {
            Verdict::Refused(refused)
        })
    }
}

/// Apply a proposal to one of the harness's own files, writing it only
/// when the judge finds it additive; the reading either way.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    file: &str,
    proposal: &Path,
    dry_run: bool,
) -> Result<Reading> {
    if !steerable(manifest, file) {
        bail!(
            "steer writes the harness's own files and no others: {file} is no TOML file the \
             manifest's {RULE} names; every other file is an Edit away"
        );
    }
    let path = root.join(file);
    // A file not there is the empty file, which a proposal lays down.
    let working = match fs::read_to_string(&path) {
        Err(gone) if gone.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.with_context(|| format!("reading the tracked {}", path.display()))?,
    };
    let proposed = fs::read_to_string(proposal)
        .with_context(|| format!("reading the proposal at {}", proposal.display()))?;
    let ground = Ground::of(root, file, working)?;
    let verdict = ground.judge(file, &proposed)?;
    let stamped = || {
        let over = Path::new(file);
        Reading::stamped_over(INSTRUMENT, file, &Snapshot::disk_over(root, over)?, over)
    };
    let mut reading = stamped()?;
    if let Err(unread) = loads(manifest, file, ground.tracked(), &proposed) {
        reading.status = Status::Fail;
        reading.message = format!(
            "{file}: the proposal would not load, so every instrument that reads the file would \
             stop: {unread:#}. Nothing was written."
        );
        return Ok(reading);
    }
    match &verdict {
        Verdict::Refused(changed) => {
            reading.status = Status::Fail;
            reading.count("changed", changed.len() as u64)?;
            reading.message = format!(
                "{file}: {} of what the file already carries would change, and a change to what \
                 binds is a person's (harness §11): {}. Nothing was written.",
                changed.len(),
                changed.join(", ")
            );
        }
        // A clean merge whose MERGE_HEAD changed the file in comments or
        // layout alone brings no value to HEAD's file, and the text git
        // merged is still neither side's commit: its reading covers it.
        _ if verdict.additions().is_empty()
            && !matches!(
                (&verdict, &ground),
                (
                    Verdict::Union { .. },
                    Ground::Merge {
                        git: Merged::Clean(_),
                        ..
                    }
                )
            ) =>
        {
            reading.status = Status::Skip;
            reading.message = format!("{file}: the proposal adds nothing to it; nothing to write");
        }
        Verdict::Additive(added) | Verdict::Union { merged: added, .. } => {
            if !dry_run {
                crate::write(&path, &proposed)?;
                // The reading is stamped over what steer left, a snapshot
                // taken after the write, so the sensor beside `permit` can
                // tell steer's write from one made beside it in the same
                // call.
                reading = stamped()?;
            }
            let dry = if dry_run {
                " (dry run, not written)"
            } else {
                ""
            };
            let clean = match &ground {
                Ground::Merge {
                    git: Merged::Clean(_),
                    ..
                } => ", as git's three-way merge of the three files gives it cleanly",
                _ => "",
            };
            let union = match &verdict {
                Verdict::Union { head, .. } => format!(
                    ", judged against HEAD's file since a merge is in progress: the union of \
                     what HEAD ({}) and MERGE_HEAD each carry over their base, and nothing \
                     beyond it{clean}, {}",
                    head.join(", "),
                    if added.is_empty() {
                        "MERGE_HEAD bringing no value to HEAD's file, its change over the base \
                         being comments or layout alone"
                    } else {
                        "MERGE_HEAD's brought to HEAD's file"
                    }
                ),
                _ => ", and nothing the file already carried changed".to_owned(),
            };
            reading.status = Status::Pass;
            reading.count("added", added.len() as u64)?;
            reading.message = format!(
                "{file}: {} added{dry}{union}{}",
                added.len(),
                if added.is_empty() {
                    String::new()
                } else {
                    format!(": {}", added.join(", "))
                }
            );
        }
    }
    Ok(reading)
}

/// Each of the harness's own files the commits since `base` changed,
/// judged as a proposal against the file `base` carries, the land's steer
/// (harness revision 198): the files are those `base`'s `harness.self`
/// names, read with this build's judge, so a merge is held by main's tip
/// and not by the steer it carries. A proposal that adds alone passes; one
/// that would not load fails, and the land refuses it; one that changes
/// what binds is a skip that names each path, since a person's commit
/// does that, and nothing yet tells it from an agent's.
/// The readings are unstamped and the land records none of them: a
/// passing steer reading over a file is what `permit` reads as steer's
/// write of it, and the land writes nothing.
pub(crate) fn since(root: &Path, base: &str) -> Result<Vec<Reading>> {
    let text = git::show(root, base, manifest::FILE)?
        .with_context(|| format!("{base} carries no {}", manifest::FILE))?;
    let binding = Manifest::parse(&text)?;
    let files = (binding.rule(RULE).into_iter())
        .flat_map(|rule| rule.scope.iter())
        .filter(|file| steerable(&binding, file));
    let mut readings = Vec::new();
    for file in files {
        let (tracked, proposed) = (git::show(root, base, file)?, git::show(root, "HEAD", file)?);
        if tracked == proposed {
            continue;
        }
        let (tracked, proposed) = (tracked.unwrap_or_default(), proposed.unwrap_or_default());
        let mut reading = Reading::new(INSTRUMENT, file);
        if let Err(unread) = loads(&binding, file, &tracked, &proposed) {
            reading.status = Status::Fail;
            reading.message = format!(
                "{file} as the merge carries it would not load under main's tip: {unread:#}"
            );
            readings.push(reading);
            continue;
        }
        match judge(file, &tracked, &proposed)? {
            Verdict::Refused(changed) => {
                reading.status = Status::Skip;
                reading.count("changed", changed.len() as u64)?;
                reading.message = format!(
                    "{file}: {} of what main's tip carries changed since, which is a person's \
                     (harness §11), read as theirs until a person's signature can be asked \
                     for: {}",
                    changed.len(),
                    changed.join(", ")
                );
            }
            verdict => {
                let added = verdict.additions();
                reading.status = Status::Pass;
                reading.count("added", added.len() as u64)?;
                reading.message = format!(
                    "{file}: {} added since main's tip, and nothing it carried changed: {}",
                    added.len(),
                    added.join(", ")
                );
            }
        }
        readings.push(reading);
    }
    Ok(readings)
}

/// Whether the proposal loads as the file it would replace, through the
/// reader the harness reads that file with: the registry, whose rows
/// decode their ids, so a `covers` list on a new row is held to its kind
/// and to one row per id as part of the addition (harness-design.md §4.3,
/// revision 62); the packages list, whose rows decode as the lane reads
/// them, every one; the pin and the manifest, whose readers refuse a key
/// they do not know, so a table the harness cannot read yet is refused
/// here rather than written to stop every instrument after it. The pin
/// and the manifest are held so only where the tracked file loads, since
/// a proposal can be held to no more than the file it replaces.
fn loads(manifest: &Manifest, file: &str, tracked: &str, proposed: &str) -> Result<()> {
    match file {
        divergence::FILE => {
            Registry::parse(proposed)?;
        }
        crate::packages::LIST => {
            crate::packages::parse(proposed)?;
        }
        _ if manifest.pin_file() == Some(file)
            && Pin::parse(tracked).is_ok_and(|pin| pin.defects().is_empty()) =>
        {
            if let Some(defect) = Pin::parse(proposed)?.defects().first() {
                bail!("{defect}");
            }
        }
        manifest::FILE if Manifest::parse(tracked).is_ok() => {
            Manifest::parse(proposed)?;
        }
        _ => {}
    }
    Ok(())
}

/// The walk of the tracked file against the proposal: what was added,
/// and what was changed or removed, each by its path in the file.
#[derive(Debug, Default)]
struct Walk {
    rows: Rows,
    added: Vec<String>,
    refused: Vec<String>,
}

impl Walk {
    /// The walk of `tracked` against `proposed`, each parsed as TOML.
    fn over(rows: Rows, tracked: &str, proposed: &str) -> Result<Walk> {
        let old: toml::Table = toml::from_str(tracked).context("the tracked file is not TOML")?;
        let new: toml::Table = toml::from_str(proposed).context("the proposed file is not TOML")?;
        let mut walk = Walk {
            rows,
            ..Walk::default()
        };
        walk.table("", &old, &new);
        Ok(walk)
    }

    /// Everything the walk found moved, refused or added, by its path.
    fn changes(&self) -> impl Iterator<Item = &String> {
        self.refused.iter().chain(&self.added)
    }

    /// One value of the tracked file against the proposal's at the same
    /// path. A table recurses key by key, an array of tables recurses into
    /// every row it carries and admits new ones beside them, and every
    /// other value must be what it was.
    fn value(&mut self, at: &str, old: &Value, new: &Value) {
        match (old, new) {
            (Value::Table(old), Value::Table(new)) => self.table(at, old, new),
            (Value::Array(old), Value::Array(new)) if rows(old) => self.rows(at, old, new),
            _ if old == new => {}
            _ => self.refused.push(path_of(at, "")),
        }
    }

    fn table(&mut self, at: &str, old: &toml::Table, new: &toml::Table) {
        for (key, old_value) in old {
            match new.get(key) {
                Some(new_value) => self.value(&path_of(at, key), old_value, new_value),
                None => self.refused.push(path_of(at, key)),
            }
        }
        // An array of tables the tracked file does not carry is its rows,
        // each added by its identity as a row appended to one it carries
        // is: so a merge base with no row and the side that adds one name
        // the row by one path.
        for (key, new_value) in new.iter().filter(|(k, _)| !old.contains_key(*k)) {
            match new_value {
                Value::Array(added) if rows(added) => self.rows(&path_of(at, key), &[], added),
                _ => self.added.push(path_of(at, key)),
            }
        }
    }

    /// An array of tables: every row the tracked file carries must stand
    /// in the proposal, named by its identity, holding every key it
    /// carried at the value it carried and standing in the order it stood
    /// in; a key it did not carry is an addition, as it is in a table. A
    /// row the proposal adds must not carry the identity of one already
    /// there, two rows of one name being how a row is shadowed rather than
    /// added. A row whose identity is the row itself, carrying none of
    /// `IDENTITY`, has no key to recurse under, so a key added to one of
    /// those reads as the row dropped and another added, which is the
    /// blunt half this instrument keeps. A [`Rows::Whole`] row is not
    /// recursed into: any difference is the row changed, named by it.
    fn rows(&mut self, at: &str, old: &[Value], new: &[Value]) {
        let named = |rows: &[Value], name: &str| rows.iter().any(|r| identity(r) == name);
        for (n, row) in new.iter().enumerate() {
            let name = identity(row);
            if named(&new[..n], &name) {
                self.refused.push(format!("{at}[{name}] twice"));
            }
        }
        let mut behind = None;
        for row in old {
            let name = identity(row);
            match new.iter().position(|r| identity(r) == name) {
                Some(n) if behind.is_some_and(|last| n < last) => {
                    self.refused.push(format!("{at}[{name}] moved"));
                }
                Some(n) => {
                    behind = Some(n);
                    let at = format!("{at}[{name}]");
                    match self.rows {
                        Rows::Keyed => self.value(&at, row, &new[n]),
                        Rows::Whole if *row != new[n] => self.refused.push(at),
                        Rows::Whole => {}
                    }
                }
                None => self.refused.push(format!("{at}[{name}] dropped")),
            }
        }
        for (n, row) in new.iter().enumerate() {
            let name = identity(row);
            if !named(old, &name) && !named(&new[..n], &name) {
                self.added.push(format!("{at}[{name}]"));
            }
        }
    }
}

/// Whether an array is an array of tables, the one shape a proposal may
/// append to; every other array is a value and must be what it was, so
/// that an `ini` line, an `unsafe` home or an edge's `to` list cannot
/// grow by one under an additive verdict.
fn rows(array: &[Value]) -> bool {
    !array.is_empty() && array.iter().all(Value::is_table)
}

/// What names one row of an array of tables: the first of [`IDENTITY`]
/// it carries, else the row itself rendered, which no two rows share
/// unless they are the same row.
fn identity(row: &Value) -> String {
    for key in IDENTITY {
        if let Some(Value::String(value)) = row.get(key) {
            return value.clone();
        }
    }
    row.to_string()
}

/// Whether two of a walk's paths name one node, or one inside the other,
/// less the word that says what befell a row.
fn overlaps(a: &str, b: &str) -> bool {
    let node = |p: &'_ str| -> String {
        let bare = [" dropped", " moved", " twice"]
            .iter()
            .find_map(|w| p.strip_suffix(w));
        bare.unwrap_or(p).to_owned()
    };
    let (a, b) = (node(a), node(b));
    let inside = |outer: &str, inner: &str| {
        (inner.strip_prefix(outer))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '[']))
    };
    inside(&a, &b) || inside(&b, &a)
}

fn path_of(at: &str, key: &str) -> String {
    match (at.is_empty(), key.is_empty()) {
        (_, true) => at.to_owned(),
        (true, false) => key.to_owned(),
        (false, false) => format!("{at}.{key}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Ground, Merged, Verdict, judge};

    /// The file the walks below judge, one whose rows are keyed.
    const FILE: &str = "seams.toml";

    fn verdict(old: &str, new: &str) -> Verdict {
        judge(FILE, old, new).expect("both parse")
    }

    #[test]
    fn a_new_table_is_additive_and_an_edited_one_is_not() {
        let pin = "[regime.jit-off]\nini = [\"opcache.enable=0\"]\n";
        let added = format!("{pin}[regime.dump]\nini = [\"opcache.enable_cli=1\"]\n");
        assert_eq!(
            verdict(pin, &added),
            Verdict::Additive(vec!["regime.dump".to_owned()])
        );
        let edited = "[regime.jit-off]\nini = [\"opcache.enable=1\"]\n";
        assert_eq!(
            verdict(pin, edited),
            Verdict::Refused(vec!["regime.jit-off.ini".to_owned()])
        );
    }

    #[test]
    fn an_array_that_is_not_rows_must_be_what_it_was() {
        let old = "[[edge]]\nfrom = \"xtask\"\nto = [\"php_bundle\"]\n";
        let widened = "[[edge]]\nfrom = \"xtask\"\nto = [\"php_bundle\", \"php_unit\"]\n";
        assert_eq!(
            verdict(old, widened),
            Verdict::Refused(vec!["edge[xtask].to".to_owned()])
        );
    }

    #[test]
    fn a_row_may_be_appended_but_not_shadowed_or_removed() {
        let old = "[[crate]]\nname = \"xtask\"\nbudget = 14000\n";
        let appended = format!("{old}[[crate]]\nname = \"ext/demo\"\nbudget = 2000\n");
        assert_eq!(
            verdict(old, &appended),
            Verdict::Additive(vec!["crate[ext/demo]".to_owned()])
        );
        let shadowed = format!("{old}[[crate]]\nname = \"xtask\"\nbudget = 99000\n");
        assert_eq!(
            verdict(old, &shadowed),
            Verdict::Refused(vec!["crate[xtask] twice".to_owned()])
        );
        assert_eq!(
            verdict(old, "[[crate]]\nname = \"ext/demo\"\nbudget = 2000\n"),
            Verdict::Refused(vec!["crate[xtask] dropped".to_owned()])
        );
    }

    #[test]
    fn a_raised_ceiling_and_a_dropped_rule_are_both_refused() {
        let old = "[budget]\nworkspace = 272500\n\n[[rule]]\nid = \"harness.self\"\ndeny = true\n";
        let raised = old.replace("272500", "999999");
        let raised = raised.as_str();
        assert_eq!(
            verdict(old, raised),
            Verdict::Refused(vec!["budget.workspace".to_owned()])
        );
        let dropped = "[budget]\nworkspace = 272500\n";
        assert_eq!(
            verdict(old, dropped),
            Verdict::Refused(vec!["rule".to_owned()])
        );
        let two = format!("{old}\n[[rule]]\nid = \"lanes.nightly\"\ndeny = true\n");
        assert_eq!(
            verdict(&two, old),
            Verdict::Refused(vec!["rule[lanes.nightly] dropped".to_owned()])
        );
    }

    #[test]
    fn a_key_a_row_does_not_carry_is_an_addition_and_a_key_it_does_is_not() {
        let old = "[[crate]]\nname = \"ext/standard\"\nbudget = 30000\n";
        let marked = format!("{old}porting = true\n");
        assert_eq!(
            verdict(old, &marked),
            Verdict::Additive(vec!["crate[ext/standard].porting".to_owned()])
        );
        let raised = "[[crate]]\nname = \"ext/standard\"\nbudget = 40000\n";
        assert_eq!(
            verdict(old, raised),
            Verdict::Refused(vec!["crate[ext/standard].budget".to_owned()])
        );
        let dropped = "[[crate]]\nname = \"ext/standard\"\n";
        assert_eq!(
            verdict(old, dropped),
            Verdict::Refused(vec!["crate[ext/standard].budget".to_owned()])
        );
    }

    #[test]
    fn an_exempt_added_to_a_rule_that_binds_is_an_addition_the_reading_names() {
        // The widening the maintainer accepted with its risk named
        // (ledger 51): the exemption lands and every reader sees it.
        let old = "[[rule]]\nid = \"witness.exit-authored\"\ndeny_pattern = \"exit=\"\n";
        let exempted = format!("{old}exempt = [\"crates/php_engine/**\"]\n");
        assert_eq!(
            verdict(old, &exempted),
            Verdict::Additive(vec!["rule[witness.exit-authored].exempt".to_owned()])
        );
    }

    #[test]
    fn a_new_row_with_covers_is_one_addition_and_a_grown_covers_is_not() {
        // A covering row is one reason (revision 62): the ids it covers are
        // part of the row the proposal adds, and a list the file already
        // carries is a value, so one id more is a change to what binds.
        let old = "[[divergence]]\nid = \"trace:x\"\nkind = \"trace\"\n";
        let row = "[[divergence]]\nid = \"phpt:cause\"\nkind = \"phpt\"\n\
                   covers = [\"phpt:a.phpt\", \"phpt:b.phpt\"]\n";
        assert_eq!(
            verdict(old, &format!("{old}{row}")),
            Verdict::Additive(vec!["divergence[phpt:cause]".to_owned()])
        );
        let tracked = format!("{old}{row}");
        let grown = tracked.replace("\"phpt:b.phpt\"]", "\"phpt:b.phpt\", \"phpt:c.phpt\"]");
        assert_eq!(
            verdict(&tracked, &grown),
            Verdict::Refused(vec!["divergence[phpt:cause].covers".to_owned()])
        );
    }

    /// An array of tables a file does not carry is its rows, each added by
    /// its identity as one appended beside a row is; a value of
    /// another shape where none stood is one addition.
    #[test]
    fn rows_where_the_file_has_none_are_each_an_addition_by_identity() {
        let none = "# no row\n";
        let two = "[[divergence]]\nid = \"trace:a\"\n[[divergence]]\nid = \"trace:b\"\n";
        let named = ["divergence[trace:a]", "divergence[trace:b]"].map(str::to_owned);
        assert_eq!(verdict(none, two), Verdict::Additive(named.to_vec()));
        let doubled = format!("{two}[[divergence]]\nid = \"trace:a\"\n");
        assert_eq!(
            verdict(none, &doubled),
            Verdict::Refused(vec!["divergence[trace:a] twice".to_owned()])
        );
        assert_eq!(
            verdict(none, "[budget]\ntotal = 1\n"),
            Verdict::Additive(vec!["budget".to_owned()])
        );
    }

    /// A merge whose base holds no row: each side's row is named by its id
    /// on both walks, so the union is a union.
    #[test]
    fn a_merge_over_a_base_of_no_row_is_the_union_of_each_sides_row() {
        let (head, merged) = (
            "[[divergence]]\nid = \"trace:head\"\n",
            "[[divergence]]\nid = \"trace:merged\"\n",
        );
        let ground = Ground::Merge {
            base: "# no row\n".to_owned(),
            head: head.to_owned(),
            merged: merged.to_owned(),
            git: Merged::Conflicted,
        };
        assert_eq!(
            ground
                .judge(FILE, &format!("{head}{merged}"))
                .expect("all parse"),
            Verdict::Union {
                head: vec!["divergence[trace:head]".to_owned()],
                merged: vec!["divergence[trace:merged]".to_owned()],
            }
        );
    }

    /// A conflicted merge of the registry: the base's one row, HEAD's
    /// row and `MERGE_HEAD`'s beside it.
    fn merge() -> Ground {
        let base = "[[divergence]]\nid = \"trace:base\"\nreason = \"r\"\n";
        Ground::Merge {
            base: base.to_owned(),
            head: format!("{base}[[divergence]]\nid = \"trace:head\"\n"),
            merged: format!("{base}[[divergence]]\nid = \"trace:merged\"\n"),
            git: Merged::Conflicted,
        }
    }

    #[test]
    fn a_merges_union_names_each_sides_addition_and_adds_what_merge_head_does() {
        let union = "[[divergence]]\nid = \"trace:base\"\nreason = \"r\"\n\
                     [[divergence]]\nid = \"trace:head\"\n\
                     [[divergence]]\nid = \"trace:merged\"\n";
        let verdict = merge().judge(FILE, union).expect("all parse");
        assert_eq!(
            verdict,
            Verdict::Union {
                head: vec!["divergence[trace:head]".to_owned()],
                merged: vec!["divergence[trace:merged]".to_owned()],
            }
        );
        assert_eq!(verdict.additions(), ["divergence[trace:merged]"]);
    }

    /// A merge one side of which changed over the base: main reworded a
    /// rule's text and appended a row, the branch appended its own, and
    /// the union carries main's text.
    fn reworded() -> Ground {
        let rule = |id: &str, text: &str| format!("[[rule]]\nid = \"{id}\"\ntext = \"{text}\"\n");
        let base = rule("git.operation", "t");
        Ground::Merge {
            head: format!("{base}{}", rule("wait.foreground", "w")),
            merged: format!(
                "{}{}",
                rule("git.operation", "t2"),
                rule("checkout.owner", "o")
            ),
            base,
            git: Merged::Conflicted,
        }
    }

    #[test]
    fn a_merge_takes_a_change_one_side_made_over_the_base_and_refuses_another() {
        let union = "[[rule]]\nid = \"git.operation\"\ntext = \"t2\"\n\
                     [[rule]]\nid = \"checkout.owner\"\ntext = \"o\"\n\
                     [[rule]]\nid = \"wait.foreground\"\ntext = \"w\"\n";
        assert_eq!(
            reworded().judge(FILE, union).expect("all parse"),
            Verdict::Union {
                head: vec!["rule[wait.foreground]".to_owned()],
                merged: ["rule[checkout.owner]", "rule[git.operation].text"]
                    .map(str::to_owned)
                    .to_vec(),
            }
        );
        let Ground::Merge {
            base,
            head,
            merged,
            git,
        } = reworded()
        else {
            unreachable!("reworded() is a merge")
        };
        let swapped = Ground::Merge {
            base,
            head: merged,
            merged: head,
            git,
        };
        assert!(
            matches!(swapped.judge(FILE, union), Ok(Verdict::Union { .. })),
            "HEAD's change too"
        );
        for (proposal, path) in [
            (
                union.replace("\"t2\"", "\"t3\""),
                "rule[git.operation].text",
            ),
            (union.replace("\"t2\"", "\"t\""), "rule[git.operation].text"),
            (union.replace("\"o\"", "\"p\""), "rule[checkout.owner].text"),
        ] {
            assert_eq!(
                reworded().judge(FILE, &proposal).expect("all parse"),
                Verdict::Refused(vec![path.to_owned()]),
                "{proposal}"
            );
        }
    }

    #[test]
    fn a_merge_refuses_what_the_two_sides_changed_differently_and_a_proposal_that_drops_a_side() {
        let base = "[[rule]]\nid = \"a\"\ntext = \"t\"\n";
        let sides = |head: &str, merged: &str| Ground::Merge {
            base: base.to_owned(),
            head: head.to_owned(),
            merged: merged.to_owned(),
            git: Merged::Conflicted,
        };
        let both = sides(
            &base.replace("\"t\"", "\"h\""),
            &base.replace("\"t\"", "\"m\""),
        );
        let Verdict::Refused(refused) = both
            .judge(FILE, &base.replace("\"t\"", "\"h\""))
            .expect("all parse")
        else {
            panic!("two sides that reworded one text differently are refused")
        };
        assert!(
            refused.contains(&"rule[a].text on HEAD and rule[a].text on MERGE_HEAD".to_owned()),
            "{refused:?}"
        );
        let alike = sides(
            &base.replace("\"t\"", "\"h\""),
            &base.replace("\"t\"", "\"h\""),
        );
        assert!(matches!(
            alike.judge(FILE, &base.replace("\"t\"", "\"h\"")),
            Ok(Verdict::Union { .. })
        ));
        let dropped = sides(&base.replace("\"t\"", "\"h\""), "# no row\n");
        let Verdict::Refused(refused) = dropped.judge(FILE, "# no row\n").expect("all parse")
        else {
            panic!("a row one side dropped and the other reworded is refused")
        };
        assert!(
            refused.contains(&"rule[a].text on HEAD and rule on MERGE_HEAD".to_owned()),
            "{refused:?}"
        );
        let Ground::Merge { head, .. } = merge() else {
            unreachable!("merge() is a merge")
        };
        assert_eq!(
            merge().judge(FILE, &head).expect("all parse"),
            Verdict::Refused(vec!["divergence[trace:merged] dropped".to_owned()]),
            "HEAD's file alone drops what MERGE_HEAD adds"
        );
    }

    /// A merge git took cleanly: the union is written only as the
    /// text git's three-way merge of the three files gives it, and never
    /// where that merge conflicts and git staged the file cleanly anyway.
    #[test]
    fn a_clean_merge_is_the_union_only_as_gits_three_way_merge_gives_it() {
        let Ground::Merge {
            base, head, merged, ..
        } = merge()
        else {
            unreachable!("merge() is a merge")
        };
        let union = format!("{head}[[divergence]]\nid = \"trace:merged\"\n");
        let clean = |git: Merged| Ground::Merge {
            base: base.clone(),
            head: head.clone(),
            merged: merged.clone(),
            git,
        };
        assert!(matches!(
            clean(Merged::Clean(union.clone())).judge(FILE, &union),
            Ok(Verdict::Union { .. })
        ));
        // The same values in another text: the union as the walk reads it,
        // and not the text git merged.
        let spaced = union.replace("id = \"trace:merged\"", "id  =  \"trace:merged\"");
        for (ground, proposal, named) in [
            (
                clean(Merged::Clean(union.clone())),
                &spaced,
                "is not the text git's three-way merge",
            ),
            (
                clean(Merged::Overridden),
                &union,
                "a strategy option took one side's hunk",
            ),
        ] {
            let Verdict::Refused(refused) = ground.judge(FILE, proposal).expect("all parse") else {
                panic!("{named}: refused")
            };
            assert!(refused.iter().any(|r| r.contains(named)), "{refused:?}");
        }
    }

    /// An `[[owed]]` row is named by its instrument: a key added to
    /// one is an addition, a changed value and a second row are not.
    #[test]
    fn an_owed_row_is_named_by_its_instrument() {
        let old = "[[owed]]\ninstrument = \"phpt\"\ncrates = \"*\"\nwhen = \"phpt-subset\"\n";
        assert_eq!(
            verdict(old, &format!("{old}held_at = \"merge\"\n")),
            Verdict::Additive(vec!["owed[phpt].held_at".to_owned()])
        );
        assert_eq!(
            verdict(old, &old.replace("phpt-subset", "always")),
            Verdict::Refused(vec!["owed[phpt].when".to_owned()])
        );
        assert_eq!(
            verdict(
                old,
                &format!("{old}[[owed]]\ninstrument = \"phpt\"\ncrates = \"*\"\n")
            ),
            Verdict::Refused(vec!["owed[phpt] twice".to_owned()])
        );
    }

    #[test]
    fn a_row_moved_past_another_is_refused_however_much_the_proposal_adds() {
        let rules = "[[rule]]\nid = \"harness.self\"\ndeny = true\n\n\
                     [[rule]]\nid = \"lanes.nightly\"\ndeny = true\n";
        let swapped = "[[rule]]\nid = \"lanes.nightly\"\ndeny = true\n\n\
                       [[rule]]\nid = \"harness.self\"\ndeny = true\nexempt = [\"x\"]\n";
        assert_eq!(
            verdict(rules, swapped),
            Verdict::Refused(vec!["rule[lanes.nightly] moved".to_owned()])
        );
        let between = "[[rule]]\nid = \"harness.self\"\ndeny = true\n\n\
                       [[rule]]\nid = \"ticket.close\"\ndeny_argv = \"x\"\n\n\
                       [[rule]]\nid = \"lanes.nightly\"\ndeny = true\n";
        assert_eq!(
            verdict(rules, between),
            Verdict::Additive(vec!["rule[ticket.close]".to_owned()]),
            "a row inserted between two that keep their order is an addition"
        );
    }
}
