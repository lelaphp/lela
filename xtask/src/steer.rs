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
//! is stated rather than hidden: 20 of the 22 `[[rule]]` rows carry no
//! `exempt`, so an agent may propose one on the rule that denies it, and
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
//! written when it is the union of what each side adds over the merge
//! base, and the reading names both sides' additions. A change or a drop on
//! either side, and a row neither side carries, is refused as any change
//! is, by its path.
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

/// The instrument name every reading of steer carries.
pub(crate) const INSTRUMENT: &str = "steer";

/// The files this instrument may write: the harness's own, the ones
/// `permit` denies the agent under `harness.self`. A path outside this
/// list is refused before anything is read, since steering is for the
/// files no other hand may touch and every other file is an `Edit` away.
pub const STEERABLE: [&str; 3] = ["seams.toml", "oracle.toml", "docs/divergence.toml"];

/// The keys an array-of-tables row is identified by, in order: a proposal
/// that adds a row whose identity an existing row already carries is
/// refused, since two rows of one name is how a row is shadowed rather
/// than added.
const IDENTITY: [&str; 3] = ["id", "name", "from"];

/// What a proposal does to the tracked file, as the judge reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing that stands was touched; the additions, by their path in
    /// the file, in the order the walk found them.
    Additive(Vec<String>),
    /// A merge stands with the file conflicted, and the proposal is the
    /// union of what each side adds over the merge base: `head`, what
    /// `HEAD` carries beyond it, and `merged`, what the write adds to
    /// `HEAD`'s file, each a value `MERGE_HEAD` adds (`Ground::Merge`).
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

/// Judge one proposal against the file it would replace: additive when
/// every value the tracked file carries stands unchanged in the proposal.
pub fn judge(tracked: &str, proposed: &str) -> Result<Verdict> {
    let walk = Walk::over(tracked, proposed)?;
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
    /// A merge stands (`MERGE_HEAD`) with the file unmerged in the index,
    /// its working copy holding git's conflict markers, which no reader
    /// parses: the three blobs git keeps for the conflict, the merge base
    /// (stage 1, empty where it carries no such file), `HEAD`'s (stage 2)
    /// and `MERGE_HEAD`'s (stage 3). Two branches that each append a ruled
    /// row conflict this way at the second's merge (#2880).
    Merge {
        base: String,
        head: String,
        merged: String,
    },
}

impl Ground {
    /// The ground of `file` in the checkout at `root`, whose text, outside
    /// a conflicted merge, is `text`: the working copy steer replaces, or
    /// the one `permit --after` kept from before the call.
    pub(crate) fn of(root: &Path, file: &str, text: String) -> Result<Ground> {
        let merging = git::has_ref(root, "MERGE_HEAD");
        let unmerged = git::git(root, &["ls-files", "--unmerged", "--", file], None)?;
        if !merging || unmerged.is_empty() {
            return Ok(Ground::File(text));
        }
        // Each entry is `<mode> <blob> <stage>\t<path>`; a file both sides
        // added has no stage 1, and its base carries nothing.
        let staged = |n: u8| unmerged.lines().any(|e| e.contains(&format!(" {n}\t")));
        let stage = |n: u8| git::git(root, &["show", &format!(":{n}:{file}")], None);
        Ok(Ground::Merge {
            base: if staged(1) { stage(1)? } else { String::new() },
            head: stage(2)?,
            merged: stage(3)?,
        })
    }

    /// The file a proposal replaces, as its reader is held to it: `HEAD`'s
    /// while a merge stands.
    fn tracked(&self) -> &str {
        match self {
            Ground::File(tracked) | Ground::Merge { head: tracked, .. } => tracked,
        }
    }

    /// Judge one proposal on this ground. In a merge it is held to both
    /// sides as to one tracked file each, and to the merge base: neither
    /// side may change or drop what the base carries, the proposal may
    /// change or drop nothing either side carries, and what it adds to
    /// `HEAD`'s file is only what `MERGE_HEAD` adds over the base, so a
    /// row neither side carries is refused by its path.
    pub(crate) fn judge(&self, proposed: &str) -> Result<Verdict> {
        let Ground::Merge { base, head, merged } = self else {
            return judge(self.tracked(), proposed);
        };
        let (ours, theirs) = (Walk::over(base, head)?, Walk::over(base, merged)?);
        let (over_head, over_merged) = (Walk::over(head, proposed)?, Walk::over(merged, proposed)?);
        let neither = over_head
            .added
            .iter()
            .filter(|path| !theirs.added.contains(path));
        let mut refused: Vec<String> = (ours.refused.iter().map(|p| format!("{p} on HEAD")))
            .chain(theirs.refused.iter().map(|p| format!("{p} on MERGE_HEAD")))
            .chain(over_head.refused)
            .chain(over_merged.refused)
            .chain(neither.map(|path| format!("{path}, which neither side carries")))
            .collect();
        refused.sort();
        refused.dedup();
        Ok(if refused.is_empty() {
            Verdict::Union {
                head: ours.added,
                merged: over_head.added,
            }
        } else {
            Verdict::Refused(refused)
        })
    }
}

/// Apply a proposal to one of the harness's own files, writing it only
/// when the judge finds it additive; the reading either way.
pub fn run(root: &Path, file: &str, proposal: &Path, dry_run: bool) -> Result<Reading> {
    if !STEERABLE.contains(&file) {
        bail!(
            "steer writes the harness's own files and no others: {file} is not one of {}; \
             every other file is an Edit away",
            STEERABLE.join(", ")
        );
    }
    let path = root.join(file);
    let working = fs::read_to_string(&path)
        .with_context(|| format!("reading the tracked {}", path.display()))?;
    let proposed = fs::read_to_string(proposal)
        .with_context(|| format!("reading the proposal at {}", proposal.display()))?;
    let ground = Ground::of(root, file, working)?;
    let verdict = ground.judge(&proposed)?;
    let stamped = || {
        let over = Path::new(file);
        Reading::stamped_over(INSTRUMENT, file, &Snapshot::disk_over(root, over)?, over)
    };
    let mut reading = stamped()?;
    if let Err(unread) = loads(file, ground.tracked(), &proposed) {
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
        _ if verdict.additions().is_empty() => {
            reading.status = Status::Skip;
            reading.message = format!("{file}: the proposal adds nothing to it; nothing to write");
        }
        Verdict::Additive(added) | Verdict::Union { merged: added, .. } => {
            if !dry_run {
                crate::write(&path, &proposed)?;
                // The reading is stamped over what steer left, a snapshot
                // taken after the write, so the sensor beside `permit` can
                // tell steer's write from one made beside it in the same
                // call (#1516).
                reading = stamped()?;
            }
            let dry = if dry_run {
                " (dry run, not written)"
            } else {
                ""
            };
            let union = match &verdict {
                Verdict::Union { head, .. } => format!(
                    ", judged against HEAD's file since a merge is in progress: the union of \
                     HEAD's {} and MERGE_HEAD's additions over their base, and nothing either \
                     side carried changed",
                    head.join(", ")
                ),
                _ => ", and nothing the file already carried changed".to_owned(),
            };
            reading.status = Status::Pass;
            reading.count("added", added.len() as u64)?;
            reading.message = format!(
                "{file}: {} added{dry}{union}: {}",
                added.len(),
                added.join(", ")
            );
        }
    }
    Ok(reading)
}

/// Whether the proposal loads as the file it would replace, through the
/// reader the harness reads that file with: the registry, whose rows
/// decode their ids, so a `covers` list on a new row is held to its kind
/// and to one row per id as part of the addition (harness-design.md §4.3,
/// revision 62); the pin and the manifest, whose readers refuse a key
/// they do not know, so a table the harness cannot read yet is refused
/// here rather than written to stop every instrument after it. The pin
/// and the manifest are held so only where the tracked file loads, since
/// a proposal can be held to no more than the file it replaces.
fn loads(file: &str, tracked: &str, proposed: &str) -> Result<()> {
    match file {
        divergence::FILE => {
            Registry::parse(proposed)?;
        }
        "oracle.toml" if Pin::parse(tracked).is_ok() => {
            Pin::parse(proposed)?;
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
    added: Vec<String>,
    refused: Vec<String>,
}

impl Walk {
    /// The walk of `tracked` against `proposed`, each parsed as TOML.
    fn over(tracked: &str, proposed: &str) -> Result<Walk> {
        let old: toml::Table = toml::from_str(tracked).context("the tracked file is not TOML")?;
        let new: toml::Table = toml::from_str(proposed).context("the proposed file is not TOML")?;
        let mut walk = Walk::default();
        walk.table("", &old, &new);
        Ok(walk)
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
        // the row by one path (#3141).
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
    /// blunt half this instrument keeps.
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
                    self.value(&format!("{at}[{name}]"), row, &new[n]);
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

/// What names one row of an array of tables: its `id`, else its `name`,
/// else the row itself rendered, which no two rows share unless they are
/// the same row.
fn identity(row: &Value) -> String {
    for key in IDENTITY {
        if let Some(Value::String(value)) = row.get(key) {
            return value.clone();
        }
    }
    row.to_string()
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
    use super::{Ground, Verdict, judge};

    fn verdict(old: &str, new: &str) -> Verdict {
        judge(old, new).expect("both parse")
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
        // The widening the maintainer accepted with its risk named (#387,
        // ledger 51): the exemption lands and every reader sees it.
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
    /// its identity as one appended beside a row is (#3141); a value of
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
    /// on both walks, so the union is a union (#3141).
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
        };
        assert_eq!(
            ground.judge(&format!("{head}{merged}")).expect("all parse"),
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
        }
    }

    #[test]
    fn a_merges_union_names_each_sides_addition_and_adds_what_merge_head_does() {
        let union = "[[divergence]]\nid = \"trace:base\"\nreason = \"r\"\n\
                     [[divergence]]\nid = \"trace:head\"\n\
                     [[divergence]]\nid = \"trace:merged\"\n";
        let verdict = merge().judge(union).expect("all parse");
        assert_eq!(
            verdict,
            Verdict::Union {
                head: vec!["divergence[trace:head]".to_owned()],
                merged: vec!["divergence[trace:merged]".to_owned()],
            }
        );
        assert_eq!(verdict.additions(), ["divergence[trace:merged]"]);
    }

    #[test]
    fn a_merge_refuses_a_side_that_changed_the_base_and_a_proposal_that_drops_a_side() {
        let Ground::Merge { base, head, .. } = merge() else {
            unreachable!("merge() is a merge")
        };
        let changed = Ground::Merge {
            merged: base.replace("\"r\"", "\"s\"") + "[[divergence]]\nid = \"trace:merged\"\n",
            base,
            head: head.clone(),
        };
        let union = format!("{head}[[divergence]]\nid = \"trace:merged\"\n");
        let Verdict::Refused(refused) = changed.judge(&union).expect("all parse") else {
            panic!("a side that changed the base is refused")
        };
        assert!(
            refused.contains(&"divergence[trace:base].reason on MERGE_HEAD".to_owned()),
            "{refused:?}"
        );
        assert_eq!(
            merge().judge(&head).expect("all parse"),
            Verdict::Refused(vec!["divergence[trace:merged] dropped".to_owned()]),
            "HEAD's file alone drops what MERGE_HEAD adds"
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
