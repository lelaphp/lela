//! The rules: one call against the manifest's `[[rule]]` rows in order,
//! after the region-body rule, and the judgement that comes out, with the
//! hook's JSON it renders to. The deep part of the judge: the type
//! mentions, the `unsafe` homes and the `pub` count live here, and a shell
//! line's argv in [`super::argv`].

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regex_lite::Regex;
use serde_json::{Value, json};

use super::approval::{Admission, LIFTED};
use super::argv::{Argv, Line};
use super::call::{Call, Touched};
use super::label;
use super::operation;
use super::owned::{self, Owned, Scope};
use super::region::region_body;
use super::shell::{Places, Unplaced, Walk};
use super::unless;
use super::writes::{self, changes};
use crate::manifest::{Manifest, Rule, Runs, Where, scope_matches};
use crate::owner::{self, SessionId};
use crate::rust;

/// The hook event the shipped adapter wires (§6).
pub(super) const HOOK_EVENT: &str = "PreToolUse";

/// A rule's verdict on one call: a deny stops the call, a note rides on
/// the allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Deny,
    Note,
}

impl Kind {
    /// What a rule does when it fires: an `on` rule notes, every other
    /// predicate permit judges denies.
    pub fn of(rule: &Rule) -> Kind {
        if rule.predicate.on.is_some() {
            Kind::Note
        } else {
            Kind::Deny
        }
    }
}

/// One thing permit found: the rule, its text and its ADR numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: Kind,
    pub rule: String,
    pub text: String,
    pub adr: Vec<String>,
    /// The person's approval that turned the rule's denial into this note.
    pub(crate) admission: Option<Admission>,
    /// Where a denial judged the call, which its reason names.
    pub(crate) placement: Option<Placement>,
    /// What to do instead, the rule's `remedy`, which a denial names.
    pub(crate) remedy: Option<String>,
}

/// Where permit judged a call it denies: the checkout it asked, or a line
/// whose command it could not place, which it says rather than guess a
/// checkout (#3619).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Placement {
    In(PathBuf),
    Unplaced(Unplaced),
}

impl std::fmt::Display for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Placement::In(checkout) => write!(f, "Judged in {}.", checkout.display()),
            Placement::Unplaced(Unplaced { command, past }) => write!(
                f,
                "Not placed: permit cannot tell which checkout `{command}` runs in past `{past}`, which does not say where it goes; name the directory in the line, or run the command in a line of its own."
            ),
        }
    }
}

impl Finding {
    fn of(kind: Kind, rule: &Rule) -> Finding {
        Finding {
            kind,
            rule: rule.id.clone(),
            text: rule.text.clone(),
            adr: rule.adr.iter().map(str::to_owned).collect(),
            admission: None,
            placement: None,
            remedy: (kind == Kind::Deny).then(|| rule.remedy.clone()).flatten(),
        }
    }

    /// The note a denial becomes where the person approved the change.
    fn admitted(rule: &Rule, admission: Admission) -> Finding {
        Finding {
            text: admission.said(),
            admission: Some(admission),
            ..Finding::of(Kind::Note, rule)
        }
    }

    /// The sentence the model reads: what the finding says, then the
    /// rule id and ADR numbers in parentheses.
    pub fn reason(&self) -> String {
        let mut tail = self.rule.clone();
        for adr in &self.adr {
            tail.push_str(", ADR-");
            tail.push_str(adr);
        }
        format!("{} ({tail})", self.said())
    }

    /// The text, then what to do instead and where a denial judged the
    /// call (#3618 ruling 3).
    pub(crate) fn said(&self) -> String {
        let said = crate::manifest::instead(&self.text, self.remedy.as_deref());
        match &self.placement {
            Some(placement) => format!("{said} {placement}"),
            None => said,
        }
    }
}

/// Every finding over one call, in evaluation order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub findings: Vec<Finding>,
    /// The rules that judged the call, whether or not they found: the
    /// denominator a rule's denials are read against (#3619).
    pub judged: Vec<String>,
    /// What the person's files carry that does not validate, which the
    /// decision notes beside itself while it judges by what loaded (#3618
    /// ruling 4).
    pub(crate) defects: Vec<String>,
}

impl Judgement {
    /// The first denial, which is the one the hook prints.
    pub fn denial(&self) -> Option<&Finding> {
        self.findings.iter().find(|f| f.kind == Kind::Deny)
    }

    /// The notes, which ride on the allow.
    pub fn notes(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.kind == Kind::Note)
    }

    /// The hook's stdout: the deny, the allow with its notes, or nothing.
    pub fn hook_stdout(&self) -> Option<Value> {
        if let Some(denial) = self.denial() {
            let reason = [denial.reason()]
                .into_iter()
                .chain(self.defects.iter().cloned());
            return Some(deny_output(&reason.collect::<Vec<_>>().join("\n")));
        }
        let notes = self
            .notes()
            .map(Finding::reason)
            .chain(self.defects.iter().cloned());
        let notes: Vec<String> = notes.collect();
        if notes.is_empty() {
            return None;
        }
        Some(json!({
            "hookSpecificOutput": {
                "hookEventName": HOOK_EVENT,
                "additionalContext": notes.join("\n"),
            }
        }))
    }
}

/// The deny shape recorded in data/pre-day-1.md §4.
pub(super) fn deny_output(reason: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": HOOK_EVENT,
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
}

/// Whether a rule describes a write or a command, so permit can judge it
/// before the call runs.
pub fn judged_at_the_call(rule: &Rule) -> bool {
    rule.predicate.describes_a_call()
}

/// Judge one call: the region-body rule, then the manifest's rules in
/// order, each reading the one walk of the call's line ([`Walk`]).
pub fn judge(manifest: &Manifest, root: &Path, call: &Call) -> Result<Judgement> {
    let touched = call.touched(root);
    let path = touched.path();
    let line = call.command().map(Line::read);
    let walk = Walk::of(&call.start(root), line.as_ref());
    let mut findings = Vec::new();
    let mut judged = Vec::new();
    if let Some(path) = path {
        findings.extend(region_body(root, path, call));
    }
    for rule in manifest.rules.iter().filter(|r| applies(r, call, &touched)) {
        judged.push(rule.id.clone());
        let finding = evaluate(manifest, root, rule, call, path, &walk)?;
        findings.extend(finding);
    }
    let defects = manifest.defects().iter().map(ToString::to_string).collect();
    Ok(Judgement {
        findings,
        judged,
        defects,
    })
}

/// Whether `rule` judges the call at all: its tool, a file its exempt does
/// not cover, what the call touches within the row's reach ([`reaches`]),
/// and a predicate that describes a call.
fn applies(rule: &Rule, call: &Call, touched: &Touched) -> bool {
    (rule.tool.is_empty() || rule.tool.iter().any(|t| t == call.judged_as()))
        && !(rule.runs == Runs::Foreground && call.in_background())
        && !touched.path().is_some_and(|path| rule.exempts(path))
        && reaches(rule, touched)
        && rule.predicate.describes_a_call()
}

/// Whether what the call touches is within the row's reach: a row without
/// a scope reaches a call without a file and every file of the workspace,
/// a row with one the files it covers (#23), and a row's `adding` every
/// file of the workspace past its scope (#4363). A file outside the
/// workspace is reached by a row with neither scope nor `adding` alone,
/// so no `adding` judges a write outside the workspace, where
/// [`Call::rewrite`] would read the file's standing text as empty and
/// count every hit as added (#4679).
fn reaches(rule: &Rule, touched: &Touched) -> bool {
    let adding = rule.predicate.adding.is_some();
    match touched {
        Touched::NoFile => rule.scope.is_empty(),
        Touched::Workspace(path) => rule.scope.is_empty() || adding || rule.covers(path),
        Touched::Outside => rule.scope.is_empty() && !adding,
    }
}

/// One rule that [`applies`] over the call, with the file it touches and
/// the walk of the shell line it runs, each read once for every rule.
fn evaluate(
    manifest: &Manifest,
    root: &Path,
    rule: &Rule,
    call: &Call,
    path: Option<&Path>,
    walk: &Walk,
) -> Result<Option<Finding>> {
    let p = &rule.predicate;
    let line = walk.line();
    let source = path.is_some_and(is_rust_source);
    let fired = if p.deny_argv.is_some() || p.require_argv.is_some() {
        line.is_some_and(|l| l.commands().iter().any(|a| fires(rule, a)))
    } else if let Some(pattern) = &p.deny_pattern {
        let regex = Regex::new(pattern)
            .with_context(|| format!("rule \"{}\": deny_pattern is not a regex", rule.id))?;
        let unless_pattern = p
            .unless_pattern
            .as_deref()
            .map(Regex::new)
            .transpose()
            .with_context(|| format!("rule \"{}\": unless_pattern is not a regex", rule.id))?;
        // A hit whose own command the `unless_pattern` matches is let
        // through; the rule fires on any other.
        let unexcused = |subject: &str, hits: Vec<std::ops::Range<usize>>| {
            hits.into_iter().any(|hit| {
                unless_pattern
                    .as_ref()
                    .is_none_or(|unless| !unless.is_match(command_around(subject, hit)))
            })
        };
        // A hit the rule's `unless` accounts for is the named instrument's
        // own and not this call's ([`unless::captured`]).
        let excused = |subject: &str| {
            p.unless.as_deref().is_some_and(|unless| {
                unless::captured(manifest, root, unless, &regex, subject, path)
            })
        };
        // A shell line is what it would run, so the pattern is asked of
        // its commands ([`Line::subject`]): less the heredoc bodies it
        // carries as text, and no hit starting in a quote, since what a
        // command quotes is not what it runs (#1771, #1832, #3635). A write is a
        // change to a file, so the pattern is asked of what the change
        // adds and not of what the file already carries (#510): a write
        // that leaves every hit where it stood adds none and fires on
        // nothing. It is asked of the file the write leaves, never of the
        // text the call types alone, since an edit can finish a hit its
        // own text does not hold, and a pattern over a block is never
        // whole in an edit inside it (#2413).
        match line {
            Some(line) => unexcused(line.subject(), line.hits(&regex)) && !excused(line.subject()),
            None => call.rewrite(root, path).is_some_and(|(before, after)| {
                let hits = regex.find_iter(&after).map(|hit| hit.range()).collect();
                adds(&regex, &after, &before) && unexcused(&after, hits) && !excused(&after)
            }),
        }
    } else if p.deny == Some(true) {
        // The scope whole; past it, with `adding`, a write that adds a hit
        // of its pattern to the file it leaves, as a `deny_pattern` over a
        // write is read (#510, #4363).
        let added = |pattern: &str| -> Result<bool> {
            let regex = Regex::new(pattern)
                .with_context(|| format!("rule \"{}\": adding is not a regex", rule.id))?;
            Ok(call
                .rewrite(root, path)
                .is_some_and(|(before, after)| adds(&regex, &after, &before)))
        };
        match &p.adding {
            Some(pattern) if !path.is_some_and(|path| rule.covers(path)) => added(pattern)?,
            _ => true,
        }
    } else if p.writes == Some(true) {
        // A write: an edit's or a write's file, or a shell line that
        // lands a write somewhere, by a command or a redirection (#3621,
        // #4250).
        match line {
            Some(_) => !writes::of(walk).is_empty(),
            None => call.tool_input.file_path.is_some(),
        }
    } else if let Some(types) = &p.deny_types {
        // The file before and after the write, each read whole, since a
        // fragment of an edit cannot say whether it stands in a comment
        // or a string.
        let macros = p.deny_macros.as_deref().unwrap_or_default();
        source
            && call
                .rewrite(root, path)
                .is_some_and(|(before, after)| adds_a_denied_use(&after, &before, types, macros))
    } else if let Some(homes) = &p.allow {
        source
            && call.written().is_some_and(has_unsafe)
            && !path.is_some_and(|path| homes.iter().any(|h| in_home(manifest, h, path)))
    } else if let Some(on) = &p.on {
        on == "pub" && source && adds_a_pub_item(root, call, path)
    } else {
        return Ok(None);
    };
    // A call the rule fired on that names an issue carrying one of its
    // `exempt_label` labels is one the rule does not judge ([`label`]).
    if fired
        && !rule.exempt_label.is_empty()
        && let Some(line) = line
        && label::exempted(root, rule, line.commands())
    {
        return Ok(None);
    }
    // A rule scoped by `where` judges a call only in the checkout it
    // names ([`judges_where`]).
    let mut standing = Standing::Judged;
    if fired {
        standing = judges_where(root, rule, walk, call)?;
        if matches!(standing, Standing::Outside) {
            return Ok(None);
        }
    }
    // A denial of the agent's hand on a harness file is lifted from the
    // one change the person approved in the session ([`super::approval`]).
    if fired
        && LIFTED.contains(&rule.id.as_str())
        && let Some(admission) = admitted(root, call, path, walk)
    {
        return Ok(Some(Finding::admitted(rule, admission)));
    }
    Ok(fired.then(|| {
        let mut finding = Finding::of(Kind::of(rule), rule);
        if finding.kind == Kind::Note {
            return finding;
        }
        finding.placement = Some(match standing {
            Standing::Foreign(foreign) => {
                finding.text = format!("{} {foreign}", rule.text);
                Placement::In(foreign.checkout)
            }
            Standing::Unplaced(lost) => Placement::Unplaced(lost),
            Standing::Judged | Standing::Outside => Placement::In(judged_in(rule, walk)),
        });
        finding
    }))
}

/// Where a call stands for a rule scoped by `where`.
enum Standing {
    /// Outside the checkouts the rule names: it does not judge the call.
    Outside,
    /// Inside them, a row without `where` among them.
    Judged,
    /// In a checkout another session owns, an operation standing there
    /// where the rule asks for one, which the denial names (#3621).
    Foreign(Owned),
    /// Past a change the line does not say where it goes, where the rule
    /// would judge: the denial says so, and guesses no checkout.
    Unplaced(Unplaced),
}

/// Where the call stands for the rule's `where`: asked where a command it
/// fires on runs ([`Walk::fired_in`]), whose branch records a ticket, which is
/// the main checkout (#2751), which is the merge worktree (#3622), or
/// whose checkout it is, and whether an operation stands there ([`owned`], #3621). An owner record that does
/// not read is an error, so the call is judged by no build.
fn judges_where(root: &Path, rule: &Rule, walk: &Walk, call: &Call) -> Result<Standing> {
    let judged = |inside: bool| {
        if inside {
            Standing::Judged
        } else {
            Standing::Outside
        }
    };
    let owned = |scope| {
        let found = owned::owned(root, call, walk, scope, |argv| fires(rule, argv))?;
        Ok(found.map_or(Standing::Outside, Standing::Foreign))
    };
    let lost = walk.unplaced(|argv| fires(rule, argv));
    Ok(match (rule.within, lost) {
        (Where::Anywhere, _) => Standing::Judged,
        // Where each write lands is the owner record's to judge (#3621).
        (Where::Owned, _) => return owned(Scope::Owned),
        (Where::Operating, Some(lost)) if operating_elsewhere(root, call)? => {
            Standing::Unplaced(lost)
        }
        (Where::Operating, Some(_)) => Standing::Outside,
        (_, Some(lost)) => Standing::Unplaced(lost),
        (Where::Ticketed, None) => judged(ticketed(rule, walk)),
        (Where::Operating, None) => return owned(Scope::Operating),
        (Where::Main, None) => judged(in_main(rule, walk)),
        (Where::Staging, None) => judged(on_staging(rule, walk)),
        (Where::Checkout, None) => judged(in_a_checkout(root, walk)),
    })
}

/// Whether an operation stands in a checkout of `root`'s repository that
/// another session than the caller's owns (#3621), or the repository
/// cannot be listed: what makes an unplaced command one `where =
/// "operating"` must refuse. A call that names no session is judged by no
/// owner record, so it is none.
fn operating_elsewhere(root: &Path, call: &Call) -> Result<bool> {
    let Some(caller) = SessionId::new(&call.session_id) else {
        return Ok(false);
    };
    let Ok(all) = crate::track::checkouts(root) else {
        return Ok(true);
    };
    for checkout in all {
        if operation::standing_in(&checkout).is_some()
            && owner::another(&checkout, Some(&caller))?.is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The checkout a denial judged the call in: where the first command the
/// rule fires on runs ([`Walk::fired_in`]) or else where the call acts
/// ([`Walk::acts_in`]), at the top of its worktree.
fn judged_in(rule: &Rule, walk: &Walk) -> PathBuf {
    let fired = walk.fired_in(Places::Every, |argv| fires(rule, argv));
    let dir = fired.into_iter().next().unwrap_or_else(|| walk.acts_in());
    let top = crate::git::rev_parse(&dir, &["--show-toplevel"]).map_or(dir, PathBuf::from);
    fs::canonicalize(&top).unwrap_or(top)
}

/// Whether a shell line runs anything inside a checkout of `root`'s
/// repository ([`crate::track::checkouts`]): where it stands
/// ([`Walk::runs_in`]) or where [`Walk::placed`] puts any of its commands, each by its
/// canonical path, so a scratch repository's `docs/divergence.toml` is not
/// this repository's (#2887), and a relative path the line names that
/// resolves into one, through a link, is in one. It fails safe, reading
/// as inside one: a line that changes directory, or moves a program,
/// where it does not say ([`Walk::lost`]), or spells a change it did not
/// read as a command ([`Walk::spells_unread_changes`]); a place that is no
/// directory on disk, which a `cd` cannot enter; and a repository git
/// cannot list.
fn in_a_checkout(root: &Path, walk: &Walk) -> bool {
    if walk.lost() || walk.spells_unread_changes() {
        return true;
    }
    let Ok(checkouts) = crate::track::checkouts(root) else {
        return true;
    };
    let inside = |path: &Path| checkouts.iter().any(|c| path.starts_with(c));
    let named: Vec<&String> = walk.line().map_or_else(Vec::new, |line| {
        let words = line.commands().iter().flat_map(Argv::words);
        words.chain(line.written()).collect()
    });
    // A name the line spells there that resolves into a checkout.
    let reaches = |dir: &Path| {
        named.iter().any(|name| {
            Path::new(name).is_relative()
                && fs::canonicalize(dir.join(name)).is_ok_and(|path| inside(&path))
        })
    };
    let placed = walk.placed(Places::Every).into_iter().map(|(_, dir)| dir);
    std::iter::once(walk.runs_in())
        .chain(placed)
        .any(|dir| match fs::canonicalize(dir) {
            Ok(dir) => !dir.is_dir() || inside(&dir) || reaches(&dir),
            Err(_) => true,
        })
}

/// Whether a command the rule fires on runs in a checkout whose branch
/// records a ticket, or one the harness cannot tell does not
/// ([`Walk::fired_in`]); a call with no line, or a line with no command the
/// rule fires on, is asked where it stands ([`Walk::runs_in`]).
fn ticketed(rule: &Rule, walk: &Walk) -> bool {
    scoped_in(rule, walk)
        .iter()
        .any(|dir| !matches!(crate::track::head_ticket(dir), Ok(None)))
}

/// Whether a command the rule fires on runs in the repository's main
/// checkout rather than a linked worktree ([`linked`]), read where
/// [`ticketed`] reads (#2751). A directory under no `.git` is in no
/// checkout at all, such as a scratch repository the line has yet to
/// make, and not in main's (#3619).
fn in_main(rule: &Rule, walk: &Walk) -> bool {
    scoped_in(rule, walk)
        .iter()
        .any(|dir| linked(dir) == Some(false))
}

/// Whether a command the rule fires on runs in the merge worktree, the
/// checkout on [`crate::queue::STAGING`], read where [`ticketed`] reads
/// (#3622).
fn on_staging(rule: &Rule, walk: &Walk) -> bool {
    scoped_in(rule, walk)
        .iter()
        .any(|dir| crate::track::head_branch(dir).as_deref() == Some(crate::queue::STAGING))
}

/// Where a `where`-scoped rule asks: each checkout a command it fires on
/// runs in ([`Walk::fired_in`]), or, for a call with no line or a line with no
/// such command, where the call stands ([`Walk::runs_in`]).
fn scoped_in(rule: &Rule, walk: &Walk) -> Vec<PathBuf> {
    let mut checkouts = walk.fired_in(Places::Every, |argv| fires(rule, argv));
    if checkouts.is_empty() {
        checkouts.push(walk.runs_in());
    }
    checkouts
}

/// Whether `dir` stands in a linked worktree: the nearest `.git` above it
/// is the file git writes there, where the main checkout's is a
/// directory. `None` for a directory under no `.git`, in no checkout.
fn linked(dir: &Path) -> Option<bool> {
    dir.ancestors()
        .map(|dir| dir.join(".git"))
        .find(|git| git.exists())
        .map(|git| git.is_file())
}

/// Whether one command of a shell line is one the rule's argv predicate,
/// `deny_argv` or `require_argv` under its `with`, fires on; a
/// `deny_pattern` fires on a command whose words it matches and its
/// `unless_pattern` does not, so a `where` rule asks where the hit runs
/// and not where its neighbours do (#2751); a rule judging by another
/// predicate fires on the line whole, and `writes` on a command that
/// changes a checkout ([`changes`]).
fn fires(rule: &Rule, argv: &Argv) -> bool {
    let p = &rule.predicate;
    if p.writes.is_some() {
        return changes(argv);
    }
    match (&p.deny_argv, &p.require_argv, &p.deny_pattern) {
        (Some(denied), _, _) => argv.denies(denied, &p.with, p.unless_argv.as_ref()),
        (None, Some(separator), _) => argv.unbounded(separator, &p.with),
        (None, None, Some(pattern)) => {
            // A separator among a command's words is one a quote held, and
            // no command position (#3635).
            let words = argv.words().join(" ").replace([';', '&', '|', '\n'], " ");
            let matches = |pattern: &str| Regex::new(pattern).map(|r| r.is_match(&words));
            matches(pattern).unwrap_or(true)
                && !p
                    .unless_pattern
                    .as_deref()
                    .is_some_and(|unless| matches(unless).unwrap_or(false))
        }
        (None, None, None) => true,
    }
}

/// The person's approval of what the call would leave: for an `Edit` or a
/// `Write`, the file at `path` as it stands to what the call makes of it;
/// for a `Bash` line, which cannot be read for what it writes, an
/// approval pending for every harness file the line can write, whose
/// write `permit --after` holds to the diff.
fn admitted(root: &Path, call: &Call, path: Option<&Path>, walk: &Walk) -> Option<Admission> {
    let approvals = call.approvals();
    if approvals.is_empty() {
        return None;
    }
    let Some(line) = walk.line() else {
        let path = path?;
        // A call at the hook displaces the one text at the path.
        let (before, after) = call.rewrite(root, Some(path))?;
        return approvals.admits(path, before.first().map(String::as_str), Some(&after));
    };
    let (checkout, written) = super::after::written(root, call, walk, line);
    let pending: Option<Vec<Admission>> = written
        .iter()
        .map(|path| {
            let before = fs::read_to_string(checkout.join(path)).ok();
            approvals.pending(Path::new(path), before.as_deref())
        })
        .collect();
    pending?.into_iter().next()
}

/// The command a hit stands in: the text from the shell separator before
/// it (`|`, `;`, `&` or a line end) to the one after it.
fn command_around(text: &str, hit: std::ops::Range<usize>) -> &str {
    let separator = |c: char| matches!(c, '|' | ';' | '&' | '\n');
    let start = text[..hit.start].rfind(separator).map_or(0, |at| at + 1);
    let end = text[hit.end..]
        .find(separator)
        .map_or(text.len(), |at| hit.end + at);
    &text[start..end]
}

fn is_rust_source(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "rs")
}

/// Whether `after` carries any hit of `regex` more often than any text of
/// `before` does: what a change adds, as a `deny_pattern` over a write
/// reads it (#510). The count and not the presence, so one more copy of a
/// line the file already carries is an addition and a line moved is not.
/// A merge displaces both parents (#1312, #2783): a hit stands as often as
/// the parent carrying it most, never as often as the two together, so a
/// copy of a line both inherit is still one the merge adds.
pub(super) fn adds(regex: &Regex, after: &str, before: &[String]) -> bool {
    let mut standing: HashMap<&str, usize> = HashMap::new();
    for text in before {
        let mut here: HashMap<&str, usize> = HashMap::new();
        for hit in regex.find_iter(text) {
            *here.entry(hit.as_str()).or_default() += 1;
        }
        for (hit, n) in here {
            let held = standing.entry(hit).or_default();
            *held = (*held).max(n);
        }
    }
    regex
        .find_iter(after)
        .any(|hit| match standing.get_mut(hit.as_str()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
}

/// Whether a write adds a use of a `deny_types` type or an invocation of
/// a `deny_macros` macro: more hits of one in the code `after` leaves than
/// in the code `before` held, as a `deny_pattern` over a write is read
/// ([`adds`], #510), so a use the file already carries is clippy's to
/// hold and not this call's. A use is what clippy's `disallowed-types` and
/// `disallowed-macros`, which the rule renders to, count: a path in code,
/// so the word in a comment, in rustdoc prose or inside a string literal
/// is none (#2026), as an unsafe site is not
/// ([`rust::without_comments_or_literals`]).
fn adds_a_denied_use(after: &str, before: &[String], types: &[String], macros: &[String]) -> bool {
    let after = rust::without_comments_or_literals(after);
    let before: Vec<String> = before
        .iter()
        .map(|text| rust::without_comments_or_literals(text))
        .collect();
    types
        .iter()
        .map(|ty| type_pattern(ty))
        .chain(macros.iter().map(|path| macro_pattern(path)))
        .any(|pattern| adds(&pattern, &after, &before))
}

/// A `deny_types` entry as a pattern: the full path, or its last segment
/// as a word; `a::b::*` is the prefix or `b::`.
fn type_pattern(ty: &str) -> Regex {
    let (whole, tail) = match ty.strip_suffix("::*") {
        Some(prefix) => (prefix, "::"),
        None => (ty, r"\b"),
    };
    let last = whole.rsplit("::").next().unwrap_or(whole);
    regex(&format!(
        r"\b{}\b|\b{}{tail}",
        regex_lite::escape(whole),
        regex_lite::escape(last)
    ))
}

/// A `deny_macros` entry's invocation as a pattern: its last segment, bare
/// or behind any path, then `!` and the opening delimiter, so
/// `thread_local! {` and `std::thread_local!(` are both the invocation of
/// `std::thread_local` and a mention of the word in prose is not.
fn macro_pattern(path: &str) -> Regex {
    let last = path.rsplit("::").next().unwrap_or(path);
    regex(&format!(r"\b{}\s*!\s*[(\[{{]", regex_lite::escape(last)))
}

/// A pattern permit writes itself; one that does not compile is a bug
/// here, not a call to let through.
pub(super) fn regex(pattern: &str) -> Regex {
    Regex::new(pattern)
        .unwrap_or_else(|err| unreachable!("permit's own pattern {pattern:?}: {err}"))
}

/// Whether the written text carries an unsafe site: what `rust::unsafe_sites`
/// finds, so the judge at the edit and the audit after it read one matcher.
fn has_unsafe(text: &str) -> bool {
    !rust::unsafe_sites(text).is_empty()
}

/// An `unsafe.homes` entry against a path: a crate name, a path pattern, or
/// a `crate::module` path under the crate's `src/`.
fn in_home(manifest: &Manifest, home: &str, path: &Path) -> bool {
    if home.contains('/') {
        return scope_matches(home, path);
    }
    let mut parts = home.split("::");
    let Some(krate) = parts.next().and_then(|name| manifest.crate_named(name)) else {
        return false;
    };
    let Some(owner) = manifest.crate_of(path) else {
        return false;
    };
    if owner.name != krate.name {
        return false;
    }
    let module: Vec<&str> = parts.collect();
    if module.is_empty() {
        return true;
    }
    let mut file = krate.path().join("src");
    for segment in &module {
        file.push(segment);
    }
    path == file.with_extension("rs") || path.starts_with(&file)
}

/// Whether the call leaves more `pub` items in the file than it found.
fn adds_a_pub_item(root: &Path, call: &Call, path: Option<&Path>) -> bool {
    let pub_item = regex(r"(?m)^\s*pub\s");
    let count = |text: &str| pub_item.find_iter(text).count();
    let before = match (&call.tool_input.old_string, path) {
        (Some(old), _) => count(old),
        (None, Some(path)) => fs::read_to_string(root.join(path)).map_or(0, |t| count(&t)),
        (None, None) => 0,
    };
    call.written().is_some_and(|text| count(text) > before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_match_files_at_or_under_them() {
        let m = |p: &str, f: &str| scope_matches(p, Path::new(f));
        assert!(m("ext/*", "ext/pcre/src/lib.rs"));
        assert!(!m("ext/*", "crates/php_ext/src/lib.rs"));
        assert!(m("**/*.rs", "xtask/src/permit/rules.rs"));
        assert!(!m("**/*.rs", "xtask/fixtures/x.json"));
        assert!(m(
            "crates/php_engine/**",
            "crates/php_engine/src/api/values.rs"
        ));
        assert!(m(".claude/**", ".claude/skills/h/hooks/hooks.json"));
        assert!(m("seams.toml", "seams.toml"));
        assert!(!m("seams.toml", "docs/seams.toml"));
        assert!(m("ext/*/src/ffi/", "ext/curl/src/ffi/bindings.rs"));
        assert!(!m("ext/*/src/ffi/", "ext/curl/src/lib.rs"));
    }

    /// A `deny = true` row with `adding` (#4363): every write into its
    /// scope is refused, and past the scope a write that adds a hit of the
    /// pattern to the file it leaves, while one that leaves each hit as it
    /// stood, or adds none, is no finding of the row's; a row without
    /// `adding` judges nothing past its scope. The pattern here is a
    /// stand-in, so this file carries no line the tracked row refuses.
    #[test]
    fn a_deny_row_with_adding_refuses_its_scope_and_an_added_hit_anywhere() {
        const ROW: &str = "[[rule]]\nid = \"x.pipe\"\ntool = [\"Edit\", \"Write\"]\n\
            scope = [\"ci/**\"]\ndeny = true\nadding = 'RUNS-ON-MAIN:[^\\n]*'\ntext = \"t\"\nremedy = \"r\"\n";
        let minimal = "[budget]\ntotal = 1\nnew = 1\ndocs = 1\nharness = 1\nlock_packages = 1\n\
            clean_build_release_s = 1\napi_items = 1\nbindings = 1\n";
        let tree = crate::scratch::Tree::new("permit-adding").unwrap();
        let root = tree.path();
        fs::write(root.join("doc.md"), "# doc\n\nRUNS-ON-MAIN: ls\n\nprose\n").unwrap();
        let call = |tool: &str, file: &str, fields: Value| {
            let mut input = json!({"file_path": root.join(file).to_str().unwrap()});
            input
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            Call::parse(&json!({"tool_name": tool, "tool_input": input}).to_string()).unwrap()
        };
        let denies = |manifest: &Manifest, call: &Call| {
            let judged = judge(manifest, root, call).unwrap();
            judged
                .findings
                .iter()
                .any(|f| f.rule == "x.pipe" && f.kind == Kind::Deny)
        };
        let manifest = Manifest::parse(&format!("{minimal}{ROW}")).unwrap();
        let fires = [
            call("Write", "ci/hook.sh", json!({"content": "echo\n"})),
            call(
                "Edit",
                "ci/hook.sh",
                json!({"old_string": "a", "new_string": "b"}),
            ),
            call(
                "Write",
                "new.md",
                json!({"content": "x\nRUNS-ON-MAIN: rm -rf /\n"}),
            ),
            call(
                "Edit",
                "doc.md",
                json!({"old_string": "prose", "new_string": "RUNS-ON-MAIN: ls"}),
            ),
            call(
                "Edit",
                "doc.md",
                json!({"old_string": ": ls", "new_string": ": curl x"}),
            ),
        ];
        for call in &fires {
            assert!(denies(&manifest, call), "{call:?}");
        }
        let passes = [
            call("Write", "new.md", json!({"content": "no marker\n"})),
            call(
                "Edit",
                "doc.md",
                json!({"old_string": "prose", "new_string": "more prose"}),
            ),
            call(
                "Write",
                "doc.md",
                json!({"content": "RUNS-ON-MAIN: ls\n\n# doc moved\n"}),
            ),
        ];
        for call in &passes {
            assert!(!denies(&manifest, call), "{call:?}");
        }
        let unqualified =
            Manifest::parse(&format!("{minimal}{}", ROW.replace("adding", "# adding"))).unwrap();
        assert!(denies(&unqualified, &fires[0]));
        assert!(
            !denies(&unqualified, &fires[2]),
            "no adding, no reach past the scope"
        );
    }

    #[test]
    fn types_are_found_by_full_path_last_segment_or_wildcard_prefix() {
        let types = ["std::sync::Mutex", "std::sync::atomic::*"];
        let mentions_a_type =
            |text: &str, types: &[&str]| types.iter().any(|ty| type_pattern(ty).is_match(text));
        assert!(mentions_a_type("use std::sync::Mutex;", &types));
        assert!(mentions_a_type("static X: Mutex<u8>", &types));
        assert!(mentions_a_type(
            "use std::sync::atomic::AtomicUsize;",
            &types
        ));
        assert!(mentions_a_type("atomic::Ordering::SeqCst", &types));
        assert!(!mentions_a_type(
            "let mutex_free = 1; // Mutexes are elsewhere",
            &types
        ));
    }

    /// A use is what clippy's `disallowed-types` and `disallowed-macros`
    /// count, a path in code: the word in a comment, in rustdoc prose or
    /// inside a string literal is none (#2026); and a write is judged by
    /// the uses it adds, so one the file already carries is not the call's.
    #[test]
    fn a_type_or_macro_named_in_prose_or_a_literal_is_no_use() {
        let types = ["std::sync::Once".to_owned(), "std::cell::Cell".to_owned()];
        let macros = ["std::thread_local".to_owned()];
        let uses = |text: &str| adds_a_denied_use(text, &[], &types, &macros);
        assert!(!uses(
            "/// Once per process, whoever asks.\npub fn init() {}"
        ));
        assert!(!uses("//! Once a byte reached the host, a Cell held it."));
        assert!(!uses(
            "/* thread_local! { } */ let s = \"std::sync::Once\";"
        ));
        assert!(!uses("let s = r#\"Once::new() and thread_local!(x)\"#;"));
        assert!(uses("use std::sync::Once;"));
        assert!(uses("/// Once.\nstatic X: Once = Once::new();"));
        assert!(uses("let c = std::cell::Cell::new(1); // Once"));
        assert!(uses("thread_local! { static K: u8 = 0; }"));
        let carried = ["use std::sync::Once;\n".to_owned()];
        let adds = |after: &str| adds_a_denied_use(after, &carried, &types, &macros);
        assert!(!adds("/// Once more.\nuse std::sync::Once;\nfn a() {}\n"));
        assert!(adds(
            "use std::sync::Once;\nstatic X: Once = Once::new();\n"
        ));
    }
}
