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
use super::argv::{Argv, Joint, Line, Move};
use super::call::Call;
use super::label;
use super::operation;
use super::region::region_body;
use super::unless;
use crate::manifest::{Manifest, Rule, Where, scope_matches};
use crate::rust;

/// The hook event the shipped adapter wires (§6).
const HOOK_EVENT: &str = "PreToolUse";

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
}

impl Finding {
    fn of(kind: Kind, rule: &Rule) -> Finding {
        Finding {
            kind,
            rule: rule.id.clone(),
            text: rule.text.clone(),
            adr: rule.adr.iter().map(str::to_owned).collect(),
            admission: None,
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

    /// The sentence the model reads: the text, then the rule id and ADR
    /// numbers in parentheses.
    pub fn reason(&self) -> String {
        let mut tail = self.rule.clone();
        for adr in &self.adr {
            tail.push_str(", ADR-");
            tail.push_str(adr);
        }
        format!("{} ({tail})", self.text)
    }
}

/// Every finding over one call, in evaluation order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub findings: Vec<Finding>,
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
            return Some(deny_output(&denial.reason()));
        }
        let notes: Vec<String> = self.notes().map(Finding::reason).collect();
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
/// before the call runs. A rule at the Stop (`tool = "Stop"`) describes
/// the reply that ends a turn, which `done --hook` reads, and is not one.
pub fn judged_at_the_call(rule: &Rule) -> bool {
    rule.predicate.describes_a_call() && !rule.at_the_stop()
}

/// Judge one call: the region-body rule, then the manifest's rules in
/// order.
pub fn judge(manifest: &Manifest, root: &Path, call: &Call) -> Result<Judgement> {
    let path = call.path(root);
    let line = call.command().map(Line::read);
    let mut findings = Vec::new();
    if let Some(path) = &path {
        findings.extend(region_body(root, path, call));
    }
    for rule in &manifest.rules {
        let finding = evaluate(manifest, root, rule, call, path.as_deref(), line.as_ref())?;
        findings.extend(finding);
    }
    Ok(Judgement { findings })
}

/// One rule over the call, with the file it touches and the shell line it
/// runs, each read once for every rule.
fn evaluate(
    manifest: &Manifest,
    root: &Path,
    rule: &Rule,
    call: &Call,
    path: Option<&Path>,
    line: Option<&Line>,
) -> Result<Option<Finding>> {
    if !rule.tool.is_empty() && !rule.tool.iter().any(|t| t == call.tool_name) {
        return Ok(None);
    }
    // A file the rule's exempt covers is not judged by it, whatever the
    // predicate; the grammar is the scope's (#23).
    if path.is_some_and(|path| rule.exempts(path)) {
        return Ok(None);
    }
    // A rule with a scope judges a file in it and nothing else.
    if !rule.scope.is_empty() {
        match path {
            Some(path) if rule.covers(path) => {}
            _ => return Ok(None),
        }
    }
    let p = &rule.predicate;
    let source = path.is_some_and(is_rust_source);
    let fired = if !p.describes_a_call() {
        return Ok(None);
    } else if p.deny_argv.is_some() || p.require_argv.is_some() {
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
        let unexcused = |subject: &str| {
            regex.find_iter(subject).any(|hit| {
                unless_pattern
                    .as_ref()
                    .is_none_or(|unless| !unless.is_match(command_around(subject, hit.range())))
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
        // its text less the heredoc bodies it carries as text: what a
        // command quotes is not what it runs (#1771, #1832). A write is a
        // change to a file, so the pattern is asked of what the change
        // adds and not of what the file already carries (#510): a write
        // that leaves every hit where it stood adds none and fires on
        // nothing. It is asked of the file the write leaves, never of the
        // text the call types alone, since an edit can finish a hit its
        // own text does not hold, and a pattern over a block is never
        // whole in an edit inside it (#2413).
        match line {
            Some(line) => unexcused(line.text()) && !excused(line.text()),
            None => call.rewrite(root, path).is_some_and(|(before, after)| {
                adds(&regex, &after, &before) && unexcused(&after) && !excused(&after)
            }),
        }
    } else if p.deny == Some(true) {
        true
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
    let mut foreign = None;
    if fired {
        match judges_where(root, rule, line, call) {
            Standing::Outside => return Ok(None),
            Standing::Judged => {}
            Standing::Foreign(operation) => foreign = Some(operation),
        }
    }
    // A denial of the agent's hand on a harness file is lifted from the
    // one change the person approved in the session ([`super::approval`]).
    if fired
        && LIFTED.contains(&rule.id.as_str())
        && let Some(admission) = admitted(root, call, path, line)
    {
        return Ok(Some(Finding::admitted(rule, admission)));
    }
    Ok(fired.then(|| {
        let mut finding = Finding::of(Kind::of(rule), rule);
        if let Some(foreign) = foreign {
            finding.text = format!("{} {foreign}", rule.text);
        }
        finding
    }))
}

/// Where a call stands for a rule scoped by `where`.
enum Standing {
    /// Outside the checkouts the rule names: it does not judge the call.
    Outside,
    /// Inside them, a row without `where` among them.
    Judged,
    /// Where an operation stands that the calling session did not start,
    /// which the denial names (#2476).
    Foreign(operation::Foreign),
}

/// Where the call stands for the rule's `where`: asked where a command it
/// fires on runs ([`fired_in`]), whose branch records a ticket, which is
/// the main checkout (#2751), or where another session's operation
/// stands.
fn judges_where(root: &Path, rule: &Rule, line: Option<&Line>, call: &Call) -> Standing {
    let judged = |inside: bool| {
        if inside {
            Standing::Judged
        } else {
            Standing::Outside
        }
    };
    match rule.within {
        Where::Anywhere => Standing::Judged,
        Where::Ticketed => judged(ticketed(root, rule, line)),
        Where::Operating => operation::foreign(root, line, call, |argv| fires(rule, argv))
            .map_or(Standing::Outside, Standing::Foreign),
        Where::Main => judged(in_main(root, rule, line)),
        Where::Checkout => judged(in_a_checkout(root, line)),
    }
}

/// Whether a shell line runs anything inside a checkout of `root`'s
/// repository ([`crate::track::checkouts`]): where it stands
/// ([`runs_in`]) or where [`placed`] puts any of its commands, each by its
/// canonical path, so a scratch repository's `docs/divergence.toml` is not
/// this repository's (#2887), and a relative path the line names that
/// resolves into one, through a link, is in one. It fails safe, reading
/// as inside one: a line that changes directory, or moves a program,
/// where it does not say ([`Walk::lost`]), or spells a change it did not
/// read as a command ([`spells_unread_changes`]); a place that is no
/// directory on disk, which a `cd` cannot enter; and a repository git
/// cannot list.
fn in_a_checkout(root: &Path, line: Option<&Line>) -> bool {
    let walk = Walk::of(root, line);
    if walk.lost || line.is_some_and(spells_unread_changes) {
        return true;
    }
    let Ok(checkouts) = crate::track::checkouts(root) else {
        return true;
    };
    let inside = |path: &Path| checkouts.iter().any(|c| path.starts_with(c));
    let named: Vec<&String> = line.map_or_else(Vec::new, |line| {
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
    std::iter::once(walk.runs_in(root))
        .chain(walk.placed.into_iter().map(|p| p.dir))
        .any(|dir| match fs::canonicalize(dir) {
            Ok(dir) => !dir.is_dir() || inside(&dir) || reaches(&dir),
            Err(_) => true,
        })
}

/// Whether the line spells a directory change it did not read as a
/// command, one a program it does not peel runs (`xargs sh -c 'cd …'`):
/// the one reading of a line's text where its commands cannot say.
fn spells_unread_changes(line: &Line) -> bool {
    let changes = |argv: &&Argv| matches!(argv.program(), "cd" | "pushd" | "popd");
    let read = line.commands().iter().filter(changes).count();
    let spelled = regex(r"(?:^|[^\w./-])(?:cd|pushd|popd)\b");
    spelled.find_iter(line.text()).count() > read
}

/// Whether a command the rule fires on runs in a checkout whose branch
/// records a ticket, or one the harness cannot tell does not
/// ([`fired_in`]); a call with no line, or a line with no command the
/// rule fires on, is asked where it stands ([`runs_in`]).
fn ticketed(root: &Path, rule: &Rule, line: Option<&Line>) -> bool {
    scoped_in(root, rule, line)
        .iter()
        .any(|dir| !matches!(crate::track::head_ticket(dir), Ok(None)))
}

/// Whether a command the rule fires on runs in the repository's main
/// checkout rather than a linked worktree ([`linked`]), read where
/// [`ticketed`] reads (#2751).
fn in_main(root: &Path, rule: &Rule, line: Option<&Line>) -> bool {
    scoped_in(root, rule, line).iter().any(|dir| !linked(dir))
}

/// Where a `where`-scoped rule asks: each checkout a command it fires on
/// runs in ([`fired_in`]), or, for a call with no line or a line with no
/// such command, where the call stands ([`runs_in`]).
fn scoped_in(root: &Path, rule: &Rule, line: Option<&Line>) -> Vec<PathBuf> {
    let mut checkouts = line.map_or_else(Vec::new, |line| {
        fired_in(root, line, Places::Every, |argv| fires(rule, argv))
    });
    if checkouts.is_empty() {
        checkouts.push(runs_in(root, line));
    }
    checkouts
}

/// Whether `dir` stands in a linked worktree: the nearest `.git` above it
/// is the file git writes there, where the main checkout's is a
/// directory. A directory under no `.git` is not one.
fn linked(dir: &Path) -> bool {
    dir.ancestors()
        .map(|dir| dir.join(".git"))
        .find(|git| git.exists())
        .is_some_and(|git| git.is_file())
}

/// Whether one command of a shell line is one the rule's argv predicate,
/// `deny_argv` or `require_argv` under its `with`, fires on; a
/// `deny_pattern` fires on a command whose words it matches and its
/// `unless_pattern` does not, so a `where` rule asks where the hit runs
/// and not where its neighbours do (#2751); a rule judging by another
/// predicate fires on the line whole.
fn fires(rule: &Rule, argv: &Argv) -> bool {
    let p = &rule.predicate;
    match (&p.deny_argv, &p.require_argv, &p.deny_pattern) {
        (Some(denied), _, _) => argv.denies(denied, &p.with, p.unless_argv.as_ref()),
        (None, Some(separator), _) => argv.unbounded(separator, &p.with),
        (None, None, Some(pattern)) => {
            let words = argv.words().join(" ");
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
fn admitted(
    root: &Path,
    call: &Call,
    path: Option<&Path>,
    line: Option<&Line>,
) -> Option<Admission> {
    let approvals = call.approvals();
    if approvals.is_empty() {
        return None;
    }
    let Some(line) = line else {
        let path = path?;
        // A call at the hook displaces the one text at the path.
        let (before, after) = call.rewrite(root, Some(path))?;
        return approvals.admits(path, before.first().map(String::as_str), Some(&after));
    };
    let (checkout, written) = super::after::written(root, call, line);
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

/// Where a shell line's shell stands once its leading directory changes
/// have run ([`Walk::stands`]), which is how a session whose shell stands
/// in one checkout runs a command in another.
pub(super) fn runs_in(root: &Path, line: Option<&Line>) -> PathBuf {
    Walk::of(root, line).runs_in(root)
}

/// The commands of `line` whose place is read, each with every place of
/// it `asked` names, one pair per place ([`Walk::placed`]). What
/// records whose operation stands in a checkout reads a command past a
/// later change where that change took the shell, never where the shell
/// started (#2720): an orchestrator's `cat $S/owner; cd <main>; git merge
/// --no-commit t1955` started a merge no record named (#2723).
pub(super) fn placed<'a>(root: &Path, line: &'a Line, asked: Places) -> Vec<(&'a Argv, PathBuf)> {
    let walk = Walk::of(root, Some(line));
    let kept = walk.placed.into_iter();
    let kept = kept.filter(|p| asked.keeps(p.argv, p.via));
    kept.map(|p| (p.argv, p.dir)).collect()
}

/// Which of a command's places a reader asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Places {
    /// Every place it runs or reaches.
    Every,
    /// The places git reads a repository in: where it runs, unless its
    /// words name a git dir ([`Move::GitDir`]), which git reads in its
    /// place, and every place it reaches besides. Under `GIT_DIR` the
    /// index and an operation's marker are the git dir's, and the
    /// repository where the command runs is not read at all (#3271).
    Repositories,
}

impl Places {
    /// Whether these places take one `argv` stands at `via`.
    fn keeps(self, argv: &Argv, via: Via) -> bool {
        let named = || argv.moves().iter().any(|m| matches!(m, Move::GitDir(_)));
        self == Places::Every || via == Via::Reaches || !named()
    }
}

/// How a command stands at a place the walk gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    /// It runs there, and reaches it by no word of its own.
    Runs,
    /// A word of its own reaches it ([`Move::Reaches`], [`Move::GitDir`]).
    Reaches,
}

/// Where each command of `line` that `fires` may run or reach, the places
/// `asked` names ([`Walk::placed`]), or, for one placed
/// nowhere, past a change the line
/// does not say where it goes, where the line stands moved by its own
/// words ([`moved`]): a rule scoped by `where` judges the commands it
/// fires on there, so `git -C /wt commit` is judged in `/wt`, `(cd /wt &&
/// ls); git add x` where the shell stands (#2764), and `cd /wt; git add
/// x` both in `/wt` and where it started where `/wt` is no directory it
/// can enter, the `cd` being one that may fail (#2995, #3257).
pub(super) fn fired_in(
    root: &Path,
    line: &Line,
    asked: Places,
    fires: impl Fn(&Argv) -> bool,
) -> Vec<PathBuf> {
    let walk = Walk::of(root, Some(line));
    let mut checkouts = Vec::new();
    for argv in line.commands().iter().filter(|argv| fires(argv)) {
        let placed = walk.places_of(argv).map(|(dir, via)| (dir.clone(), via));
        let mut at: Vec<(PathBuf, Via)> = placed.collect();
        if at.is_empty() {
            at = moved(&walk.runs_in(root), argv);
        }
        let kept = at.into_iter().filter(|(_, via)| asked.keeps(argv, *via));
        for (dir, _) in kept {
            add(&mut checkouts, dir);
        }
    }
    checkouts
}

/// Where the call acts: where the line stands ([`runs_in`]), or, where
/// every command past its leading directory changes is git and each runs
/// in one and the same other place, by its `-C` or a subshell's change,
/// that place, so `git -C /wt checkout -- seams.toml` and `git -C /wt add
/// x && git -C /wt commit` act in `/wt`. git's `-C` moves one command and
/// not the shell, so a line any of whose other commands runs where the
/// shell stands (`git -C /wt status && git checkout -- seams.toml`,
/// `touch x && git -C /wt status`), or may run in more than one place,
/// acts where the shell stands, the call being held in one checkout.
pub(super) fn acts_in(root: &Path, line: Option<&Line>) -> PathBuf {
    let walk = Walk::of(root, line);
    let dir = walk.runs_in(root);
    let mut named = walk.rest.iter().map(|argv| {
        let mut at = walk.places_of(argv).map(|(dir, _)| dir);
        let one = at.next().filter(|_| at.next().is_none());
        one.filter(|_| argv.program() == "git")
    });
    match named.next() {
        Some(Some(first)) if *first != dir && named.all(|other| other == Some(first)) => {
            first.clone()
        }
        _ => dir,
    }
}

/// The one reading of where a shell line's commands run, from which
/// [`runs_in`], [`placed`], [`fired_in`] and [`acts_in`] are read, so
/// `where = "ticketed"`, `where = "main"`, `where = "operating"`,
/// `where = "checkout"`, the checkout `permit --after` holds and the
/// approvals it reads move together (#2597, #2607, #2995).
///
/// Each `cd`, `pushd` and `popd` the line says the target of moves the
/// shell it runs in, from the call's `root`, `~/` being the home; a `cd`
/// or `pushd` to a variable the line assigned before it, `W=/wt && cd
/// "$W"`, goes where the value names ([`Argv::directory`]). A change may
/// not run, past `||` or past a command of its `&&` chain that may fail,
/// and may fail, into a place the shell cannot enter as the line is
/// judged ([`enters`]): a file, a directory without search permission or
/// none. The shell then stays where it stood: each shell keeps the places
/// it may stand where every command since the last separator other than
/// `&&` succeeded apart from those where one of them failed, and a command
/// after any other separator ([`Joint`]) runs at both, so where `/wt` is
/// no directory, `cd /wt && x; y` runs `x` in `/wt` and `y` in `/wt` and
/// where the line started, as `x || cd /wt; y` runs `y` wherever `/wt`
/// is. A change sure to run into a directory it can enter is sure to move
/// the shell, so `cd <another repository> && S=x; git commit` commits
/// there alone (#3257). What that leaves open, stated: a directory the
/// line itself makes, removes or makes unsearchable before its change, or
/// one the shell's sandbox refuses where permit's process may enter it,
/// is read as it stands when the line is read, before the call and again
/// at `permit --after`, so the two may place it apart. A change piped or
/// sent to the background runs in a shell of its own and moves nothing. A
/// change inside a subshell, a substitution or a child shell moves the
/// commands in it alone ([`Line::shells`], #2764), and never the line:
/// `(cd /other && git status); git add x` stands where it started, and
/// its `git add` runs there (#2787).
///
/// A change the line does not say where it goes is not followed, and the
/// line is lost there: a variable the line did not assign, `cd` with no
/// operand or `-`, `pushd` with an option, a stack place (`+1`) or none,
/// and `popd` with an option or no `pushd` before it; nothing past it in
/// that shell is placed. A program moved by its own words runs where they
/// say ([`Move`]), and one moved where they do not say loses the line.
#[derive(Default)]
struct Walk<'a> {
    /// Each command with each place it may run or reach, one per place;
    /// none for a change, nor for a command past a change its shell lost.
    placed: Vec<Placed<'a>>,
    /// Where the line's own shell may stand at its first command past
    /// its leading changes and its `export`s of assignments alone, which
    /// move nothing, a subshell's changes among them.
    stands: Vec<PathBuf>,
    /// The commands from that one on.
    rest: &'a [Argv],
    /// Whether the line makes a change, or moves a program, where it does
    /// not say.
    lost: bool,
}

/// One place the walk gives a command, and how it stands there, which
/// [`Places`] tells apart.
struct Placed<'a> {
    argv: &'a Argv,
    dir: PathBuf,
    via: Via,
}

/// One shell the walk is in: its id, the places it may stand where every
/// command of its `&&` chain succeeded, those where one of them failed,
/// its `pushd` stack, each entry the places it pushed, whether it lost,
/// whether every command of its chain so far is sure to have run and
/// succeeded, and the places, stack and loss it began its chain with,
/// which a chain sent to the background leaves it.
#[derive(Default)]
struct Shell {
    id: u32,
    sure: Vec<PathBuf>,
    failed: Vec<PathBuf>,
    pushed: Vec<Vec<PathBuf>>,
    lost: bool,
    certain: bool,
    chain: (Vec<PathBuf>, Vec<Vec<PathBuf>>, bool),
}

impl Shell {
    fn new(id: u32, sure: Vec<PathBuf>, lost: bool, certain: bool) -> Shell {
        let chain = (sure.clone(), Vec::new(), lost);
        Shell {
            id,
            sure,
            lost,
            certain,
            chain,
            ..Shell::default()
        }
    }

    /// Where a command joined by `before` runs: after `&&` or a pipe where
    /// the chain succeeded, and after any other separator wherever the
    /// shell may stand, which begins a chain, sure to run but past `||`.
    fn runs_at(&mut self, before: Joint) -> Vec<PathBuf> {
        if !matches!(before, Joint::And | Joint::Pipe) {
            for dir in std::mem::take(&mut self.failed) {
                add(&mut self.sure, dir);
            }
            self.chain = (self.sure.clone(), self.pushed.clone(), self.lost);
            self.certain = before != Joint::Or;
        }
        self.sure.clone()
    }

    /// The shell once `argv`, a `cd`, `pushd` or `popd`, ran at `from`:
    /// moved where it succeeded and standing where it failed, or lost
    /// where the line does not say where it goes.
    fn change(&mut self, from: Vec<PathBuf>, argv: &Argv) {
        let words = argv.words();
        let to = argv
            .directory()
            .filter(|to| !to.starts_with(['-', '+', '$']));
        let moved = |to: &str| Some(from.iter().map(|dir| at(dir, to)).collect());
        let moved = match (words.first().map(String::as_str), to) {
            (Some("cd"), Some(to)) => moved(to),
            (Some("pushd"), Some(to)) if words.len() == 2 => {
                self.pushed.push(from.clone());
                moved(to)
            }
            (Some("popd"), _) if words.len() == 1 => self.pushed.pop(),
            _ => None,
        };
        let Some(moved) = moved else {
            self.lost = true;
            return;
        };
        self.certain &= moved.iter().all(|dir| enters(dir));
        self.sure.clear();
        for dir in moved {
            add(&mut self.sure, dir);
        }
        if !self.certain {
            for dir in from {
                add(&mut self.failed, dir);
            }
        }
    }

    /// The shell as its chain began, the chain having run in a shell of
    /// its own in the background.
    fn background(&mut self) {
        (self.sure, self.pushed, self.lost) = self.chain.clone();
        self.failed.clear();
    }
}

impl<'a> Walk<'a> {
    fn of(root: &Path, line: Option<&'a Line>) -> Walk<'a> {
        let mut walk = Walk::default();
        walk.stands.push(root.to_path_buf());
        let Some(line) = line else {
            return walk;
        };
        let mut top = Shell::new(0, walk.stands.clone(), false, true);
        let (mut nested, mut leading): (Vec<Shell>, _) = (Vec::new(), true);
        let commands = line.commands();
        let each = commands.iter().zip(line.shells()).zip(line.joints());
        for (n, ((argv, ids), joints)) in each.enumerate() {
            let kept = nested.iter().zip(ids).take_while(|(s, id)| s.id == **id);
            let kept = kept.count();
            if kept < nested.len() {
                nested.truncate(kept);
                // A shell that closed may have failed, and a change past it
                // in its chain not run.
                nested.last_mut().unwrap_or(&mut top).certain = false;
            }
            let entering = ids.len() > nested.len();
            for id in ids.get(nested.len()..).unwrap_or_default() {
                let parent = nested.last_mut().unwrap_or(&mut top);
                let sure = parent.runs_at(joints.before);
                let (lost, certain) = (parent.lost, parent.certain);
                if leading && nested.is_empty() {
                    walk.stands.clone_from(&sure);
                }
                nested.push(Shell::new(*id, sure, lost, certain));
            }
            let shell = nested.last_mut().unwrap_or(&mut top);
            let at = shell.runs_at(joints.before);
            let changes = matches!(argv.program(), "cd" | "pushd" | "popd");
            // A pipe's side runs in a shell of its own, unless the command
            // opens one, a subshell or a substitution, which its own
            // changes move.
            let apart = (joints.before == Joint::Pipe && !entering) || joints.after == Joint::Pipe;
            let words = argv.words();
            let exports = words.first().is_some_and(|w| w == "export")
                && words.iter().skip(1).all(|w| w.contains('='));
            if changes && !apart && !shell.lost {
                shell.change(at.clone(), argv);
                walk.lost |= shell.lost;
            } else if !exports {
                // It may fail, and a change past it in its chain not run.
                shell.certain = false;
            }
            walk.lost |= argv.moves().contains(&Move::Unsaid);
            for (dir, via) in at.iter().flat_map(|dir| moved(dir, argv)) {
                if changes || shell.lost {
                    continue;
                }
                let same = |p: &&mut Placed| std::ptr::eq(p.argv, argv) && p.dir == dir;
                match walk.placed.iter_mut().find(same) {
                    // A place it runs at and reaches is one it reaches.
                    Some(placed) if via == Via::Reaches => placed.via = via,
                    Some(_) => {}
                    None => walk.placed.push(Placed { argv, dir, via }),
                }
            }
            let read_past = exports || (changes && (apart || !shell.lost));
            let background = joints.after == Joint::Background;
            if background {
                shell.background();
            }
            if leading && ids.is_empty() {
                walk.stands = if background { shell.sure.clone() } else { at };
            }
            if leading && !read_past {
                leading = false;
                walk.rest = commands.get(n..).unwrap_or_default();
            }
        }
        if leading {
            walk.stands = top.sure;
        }
        walk
    }

    /// Where the line stands: the one place its shell may stand at its
    /// first command past its leading changes, or `root` where it may
    /// stand in more than one.
    fn runs_in(&self, root: &Path) -> PathBuf {
        match self.stands.as_slice() {
            [one] => one.clone(),
            _ => root.to_path_buf(),
        }
    }

    /// Each place `argv`, one of the line's commands, may run or reach,
    /// and how it stands there.
    fn places_of(&self, argv: &'a Argv) -> impl Iterator<Item = (&PathBuf, Via)> {
        let placed = self
            .placed
            .iter()
            .filter(move |p| std::ptr::eq(p.argv, argv));
        placed.map(|p| (&p.dir, p.via))
    }
}

/// Whether a change into `dir` succeeds as the line is judged: `dir/.`
/// resolves to a directory only where `dir` is one the shell may search,
/// as `cd` needs it to be (#3257).
fn enters(dir: &Path) -> bool {
    dir.join(".").is_dir()
}

/// `one` added to `set` where it is not there already.
fn add<T: PartialEq>(set: &mut Vec<T>, one: T) {
    if !set.contains(&one) {
        set.push(one);
    }
}

/// Where a command its shell runs in `dir` runs, moved by each
/// [`Move::Into`] of its words, then each place it reaches besides
/// ([`Move::Reaches`], [`Move::GitDir`]), read from where the moves into
/// left the program, as git reads `GIT_DIR` once its `-C` has moved it;
/// each with how it stands there.
fn moved(dir: &Path, argv: &Argv) -> Vec<(PathBuf, Via)> {
    let mut runs = dir.to_path_buf();
    for one in argv.moves() {
        if let Move::Into(to) = one {
            runs = at(&runs, to);
        }
    }
    let reached = argv.moves().iter().filter_map(|one| match one {
        Move::Reaches(to) | Move::GitDir(to) => Some((at(&runs, to), Via::Reaches)),
        Move::Into(_) | Move::Unsaid => None,
    });
    let reached: Vec<(PathBuf, Via)> = reached.collect();
    std::iter::once((runs, Via::Runs)).chain(reached).collect()
}

/// `to` read as a path from `dir`, `~/` as the home.
pub(super) fn at(dir: &Path, to: &str) -> PathBuf {
    match (to.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => dir.join(to),
    }
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
fn regex(pattern: &str) -> Regex {
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

    /// #2787: a leading `cd` inside a subshell moves that subshell alone,
    /// so the line stands where it started, its `git add` runs and acts
    /// there and is judged there, and its `git status` is judged where the
    /// subshell's change took it alone; a line whose every git command
    /// runs in the subshell's checkout acts in it, and a change outside a
    /// subshell still moves the line.
    #[test]
    fn a_leading_change_inside_a_subshell_moves_that_subshell_alone() {
        let (root, other) = (Path::new("/shell"), Path::new("/other"));
        let line = Line::read("(cd /other && git status); git add x");
        assert_eq!(runs_in(root, Some(&line)), root);
        assert_eq!(acts_in(root, Some(&line)), root);
        let adds = |argv: &Argv| argv.words().get(1).is_some_and(|w| w == "add");
        assert_eq!(fired_in(root, &line, Places::Every, adds), [root]);
        let statuses = |argv: &Argv| argv.words().get(1).is_some_and(|w| w == "status");
        assert_eq!(fired_in(root, &line, Places::Every, statuses), [other]);
        let inside = Line::read("(cd /other && git add x && git commit -m y)");
        assert_eq!(runs_in(root, Some(&inside)), root);
        assert_eq!(acts_in(root, Some(&inside)), other);
        let outside = Line::read("cd /wt && (cd sub && ls); git add x");
        assert_eq!(runs_in(root, Some(&outside)), Path::new("/wt"));
        assert_eq!(acts_in(root, Some(&outside)), Path::new("/wt"));
    }

    /// #3271: git reads the repository its `GIT_DIR` or `--git-dir` names
    /// in place of the one where it runs, and every other reader still
    /// takes both; `GIT_WORK_TREE` and `--work-tree` change no index and
    /// stay places it reaches besides where it runs.
    #[test]
    fn git_reads_the_repository_its_git_dir_names_in_place_of_where_it_runs() {
        let (root, wt, git_dir) = (Path::new("/shell"), Path::new("/wt"), Path::new("/o/.git"));
        let read = |text: &str, asked: Places| {
            fired_in(root, &Line::read(text), asked, |argv| {
                argv.program() == "git"
            })
        };
        let repositories = |text: &str| read(text, Places::Repositories);
        assert_eq!(
            repositories("GIT_DIR=/o/.git git commit -F m -- x"),
            [git_dir]
        );
        assert_eq!(repositories("git --git-dir=/o/.git commit"), [git_dir]);
        assert_eq!(repositories("git --git-dir /o/.git commit"), [git_dir]);
        assert_eq!(
            repositories("export GIT_DIR=/o/.git; git commit"),
            [git_dir]
        );
        assert_eq!(repositories("GIT_DIR=.git git -C /o commit"), [git_dir]);
        assert_eq!(
            repositories("GIT_DIR=/o/.git GIT_WORK_TREE=/wt git add x"),
            [git_dir, wt]
        );
        assert_eq!(repositories("GIT_WORK_TREE=/wt git add x"), [root, wt]);
        assert_eq!(repositories("git --work-tree=/wt add x"), [root, wt]);
        assert_eq!(repositories("cd /a && GIT_DIR=/o/.git git x"), [git_dir]);
        // Where the git dir is where it runs, it is read there.
        assert_eq!(repositories("GIT_DIR=. git -C /o/.git log"), [git_dir]);
        // A variable the line did not assign says nowhere: it runs where
        // it runs, and the line is lost.
        assert_eq!(repositories("GIT_DIR=$X git commit"), [root]);
        let places = read("GIT_DIR=/o/.git git commit", Places::Every);
        assert_eq!(places, [root, git_dir]);
    }

    /// #2995: a change is followed only as far as the `&&` chain it
    /// stands in, since it may fail; one piped or sent to the background
    /// moves nothing; a program's own words move it.
    #[test]
    fn a_change_is_followed_as_far_as_its_and_chain_and_a_program_where_its_words_say() {
        let (root, wt) = (Path::new("/shell"), Path::new("/wt"));
        let fired = |text: &str, program: &str| {
            fired_in(root, &Line::read(text), Places::Every, |argv| {
                argv.program() == program
            })
        };
        assert_eq!(fired("cd /wt && x; y", "x"), [wt]);
        assert_eq!(fired("cd /wt && x; y", "y"), [wt, root]);
        assert_eq!(fired("cd /wt &&\n  y", "y"), [wt]);
        assert_eq!(fired("cd /wt && X=1; y", "y"), [wt, root]);
        assert_eq!(fired("cd /wt || true && y", "y"), [wt, root]);
        assert_eq!(fired("cd /wt | true; y", "y"), [root]);
        assert_eq!(fired("x | cd /wt && y", "y"), [root]);
        assert_eq!(fired("cd /wt & y", "y"), [root]);
        assert_eq!(fired("cd /wt; cd /a && y", "y"), [Path::new("/a")]);
        assert_eq!(fired("env -C /wt y", "y"), [wt]);
        // A lone `&` sends the whole `&&` list to the background; a pipe's
        // subshell is moved by its own change; `|&` is a pipe.
        assert_eq!(fired("cd /wt && x & y", "y"), [root]);
        assert_eq!(fired("x | (cd /wt; y)", "y"), [wt, root]);
        assert_eq!(fired("x |& cd /wt && y", "y"), [root]);
        assert_eq!(fired("env -iC /wt y; env -C/wt y", "y"), [wt]);
        let reached = [wt.to_path_buf(), wt.join(".git")];
        assert_eq!(fired("GIT_DIR=.git git -C /wt status", "git"), reached);
        assert_eq!(
            fired("cd /a && GIT_DIR=/wt git x", "git"),
            [Path::new("/a"), wt]
        );
        // #3257: a change sure to run into a directory it can enter is sure
        // to move the shell; one past `||` or past a command of its chain
        // that may fail may not run.
        let tmp = std::env::temp_dir();
        let t = tmp.display();
        assert_eq!(fired(&format!("cd / && cd {t}; y"), "y"), [tmp.as_path()]);
        assert_eq!(
            fired(&format!("x || cd {t}; y"), "y"),
            [tmp.as_path(), root]
        );
        assert_eq!(
            fired(&format!("x && cd {t}; y"), "y"),
            [tmp.as_path(), root]
        );
        assert_eq!(fired(&format!("x; (cd {t}; y)"), "y"), [tmp.as_path()]);
        assert_eq!(
            fired(&format!("(x) && cd {t}; y"), "y"),
            [tmp.as_path(), root]
        );
        let line = |text: &str| Line::read(text);
        assert_eq!(runs_in(root, Some(&line("cd /wt; git add x"))), root);
        assert_eq!(runs_in(root, Some(&line("cd /wt && x & git add y"))), root);
        assert_eq!(runs_in(root, Some(&line("cd /wt && x; git add x"))), wt);
        assert_eq!(acts_in(root, Some(&line("cd /a; git -C sub add x"))), root);
        assert_eq!(acts_in(root, Some(&line("env -C /wt git add x"))), wt);
        assert_eq!(acts_in(root, Some(&line("git -C /wt add x"))), wt);
        let walk = |text: &str| Walk::of(root, Some(&line(text))).lost;
        assert!(walk("cd $X; y") && walk("find . -execdir y {} +"));
        assert!(!walk("cd $X | true; y") && !walk("X=/wt && cd $X"));
    }

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
