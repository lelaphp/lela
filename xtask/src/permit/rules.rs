//! The rules: one call against the manifest's `[[rule]]` rows in order,
//! after the region-body rule, and the judgement that comes out, with the
//! hook's JSON it renders to. The deep part of the judge: the type
//! mentions, the `unsafe` homes and the `pub` count live here, and a shell
//! line's argv in [`super::argv`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regex_lite::Regex;
use serde_json::{Value, json};

use super::approval::{Admission, LIFTED};
use super::argv::{Argv, Line};
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
    // names: where a command it fires on runs ([`fired_in`]), whose
    // branch records a ticket, or where an operation stands that the
    // calling session did not start, which the denial names (#2476).
    let mut foreign = None;
    if fired {
        match rule.within {
            Where::Anywhere => {}
            Where::Ticketed => {
                if !ticketed(root, rule, line) {
                    return Ok(None);
                }
            }
            Where::Operating => {
                foreign = operation::foreign(root, line, call, |argv| fires(rule, argv));
                if foreign.is_none() {
                    return Ok(None);
                }
            }
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

/// Whether a command the rule fires on runs in a checkout whose branch
/// records a ticket, or one the harness cannot tell does not
/// ([`fired_in`]); a call with no line, or a line with no command the
/// rule fires on, is asked where it stands ([`runs_in`]).
fn ticketed(root: &Path, rule: &Rule, line: Option<&Line>) -> bool {
    let mut checkouts = line.map_or_else(Vec::new, |line| {
        fired_in(root, line, |argv| fires(rule, argv))
    });
    if checkouts.is_empty() {
        checkouts.push(runs_in(root, line));
    }
    checkouts
        .iter()
        .any(|dir| !matches!(crate::track::head_ticket(dir), Ok(None)))
}

/// Whether one command of a shell line is one the rule's argv predicate,
/// `deny_argv` or `require_argv` under its `with`, fires on; a rule
/// judging by another predicate fires on the line whole.
fn fires(rule: &Rule, argv: &Argv) -> bool {
    let p = &rule.predicate;
    match (&p.deny_argv, &p.require_argv) {
        (Some(denied), _) => argv.denies(denied, &p.with),
        (None, Some(separator)) => argv.unbounded(separator, &p.with),
        (None, None) => true,
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
        let (before, after) = call.rewrite(root, Some(path))?;
        return approvals.admits(path, Some(&before), Some(&after));
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
/// have run ([`standing`]), which is how a session whose shell stands in
/// one checkout runs a command in another.
pub(super) fn runs_in(root: &Path, line: Option<&Line>) -> PathBuf {
    standing(root, line).0
}

/// The commands of `line` whose place is read, each with where it runs:
/// those past the leading directory changes, at where the line stands
/// ([`runs_in`]) moved by each later `cd`, `pushd` and `popd` as
/// [`standing`] reads one, and, for git, by each `-C` among the options
/// before its subcommand ([`moved`]); none past a change the line does not
/// say where it goes. What records whose operation stands in a checkout
/// reads a command past a later change where that change took the shell,
/// never where the shell started (#2720): an orchestrator's `cat $S/owner; cd <main>;
/// git merge --no-commit t1955` started a merge no record named (#2723).
/// A change inside a subshell, a substitution or a child shell moves the
/// commands in it alone, and the one after it is read where the shell
/// stood before it ([`Line::shells`], #2764).
pub(super) fn placed<'a>(root: &Path, line: &'a Line) -> Vec<(&'a Argv, PathBuf)> {
    let (mut dir, mut pushed) = (root.to_path_buf(), Vec::new());
    let mut entered: Vec<(u32, PathBuf, Vec<PathBuf>)> = Vec::new();
    let mut placed = Vec::new();
    for (argv, shells) in line.commands().iter().zip(line.shells()) {
        let kept = entered
            .iter()
            .zip(shells)
            .take_while(|((id, ..), s)| id == *s)
            .count();
        while entered.len() > kept {
            (_, dir, pushed) = entered.pop().unwrap_or_default();
        }
        let left = shells.get(kept..).unwrap_or_default();
        entered.extend(left.iter().map(|id| (*id, dir.clone(), pushed.clone())));
        if change(&mut dir, &mut pushed, argv) {
            continue;
        }
        if matches!(argv.program(), "cd" | "pushd" | "popd") {
            break;
        }
        placed.push((argv, moved(dir.clone(), argv)));
    }
    placed
}

/// Where each command of `line` that `fires` runs, read as [`placed`]
/// reads it and, past a later directory change, where the line stands
/// all the same, the side that judges: a rule scoped by `where` judges
/// the commands it fires on there, so `git -C /wt commit` is judged in
/// `/wt`.
pub(super) fn fired_in(root: &Path, line: &Line, fires: impl Fn(&Argv) -> bool) -> Vec<PathBuf> {
    let dir = runs_in(root, Some(line));
    line.commands()
        .iter()
        .filter(|argv| fires(argv))
        .map(|argv| moved(dir.clone(), argv))
        .collect()
}

/// Where the call acts: where the line stands ([`runs_in`]), or, where
/// every command past its leading directory changes is git and each
/// names one directory by its `-C`, that directory ([`moved`]), so `git
/// -C /wt checkout -- seams.toml` and `git -C /wt add x && git -C /wt
/// commit` act in `/wt`. git's `-C` moves one command and not the shell,
/// so a line any of whose other commands runs where the shell stands
/// (`git -C /wt status && git checkout -- seams.toml`, `touch x && git
/// -C /wt status`) acts where the shell stands, the call being held in
/// one checkout.
pub(super) fn acts_in(root: &Path, line: Option<&Line>) -> PathBuf {
    let (dir, rest) = standing(root, line);
    let mut named = rest
        .iter()
        .map(|argv| (argv.program() == "git").then(|| moved(dir.clone(), argv)));
    match named.next() {
        Some(Some(first)) if first != dir && named.all(|other| other.as_ref() == Some(&first)) => {
            first
        }
        _ => dir,
    }
}

/// The one reading of where a shell line stands, from which [`runs_in`],
/// [`placed`], [`fired_in`] and [`acts_in`] are read, so `where =
/// "ticketed"`, `where = "operating"`, the checkout `permit --after` holds
/// and the approvals it reads move together (#2597, #2607): the call's
/// `root`, moved by each `cd` and `pushd` before the first command that
/// does anything else and back by each `popd` to where its `pushd` left;
/// and the commands past those changes.
///
/// An `export` of assignments alone moves nothing and is read past, as a
/// bare assignment is, and `~/` is the home. A `cd` or `pushd` to a
/// variable the line assigned before it, `W=/wt && cd "$W"` or `export
/// W=/wt; cd $W/sub`, goes where the value names ([`Argv::directories`]).
/// A change the line does not say where it goes is not followed, and the
/// line stands where it stood before it, `root` for the first: a
/// variable the line did not assign (one the environment holds), `cd`
/// with no operand or `-`, `pushd` with an option, a stack place (`+1`)
/// or none, and `popd` with an option or no `pushd` before it.
fn standing<'a>(root: &Path, line: Option<&'a Line>) -> (PathBuf, &'a [Argv]) {
    let commands = line.map_or(&[][..], Line::commands);
    let mut dir = root.to_path_buf();
    let mut pushed = Vec::new();
    for (at_command, argv) in commands.iter().enumerate() {
        let words = argv.words();
        let exports = words.first().is_some_and(|w| w == "export")
            && words.iter().skip(1).all(|w| w.contains('='));
        if !exports && !change(&mut dir, &mut pushed, argv) {
            return (dir, commands.get(at_command..).unwrap_or_default());
        }
    }
    (dir, &[])
}

/// `dir` moved by `argv` where it is a `cd`, `pushd` or `popd` the line
/// says where it goes ([`standing`]), `pushed` the `pushd` stack; whether
/// it was one.
fn change(dir: &mut PathBuf, pushed: &mut Vec<PathBuf>, argv: &Argv) -> bool {
    let words = argv.words();
    let to = argv
        .directories()
        .next()
        .filter(|to| !to.starts_with(['-', '+', '$']));
    match (words.first().map(String::as_str), to) {
        (Some("cd"), Some(to)) => *dir = at(dir, to),
        (Some("pushd"), Some(to)) if words.len() == 2 => {
            pushed.push(dir.clone());
            *dir = at(dir, to);
        }
        (Some("popd"), _) if words.len() == 1 => match pushed.pop() {
            Some(back) => *dir = back,
            None => return false,
        },
        _ => return false,
    }
    true
}

/// `dir` moved, for a git command, by each `-C` among its options before
/// its subcommand ([`Argv::directories`]); any other command runs there.
fn moved(dir: PathBuf, argv: &Argv) -> PathBuf {
    if argv.program() != "git" {
        return dir;
    }
    argv.directories().fold(dir, |dir, to| at(&dir, to))
}

/// `to` read as a directory from `dir`, `~/` as the home.
fn at(dir: &Path, to: &str) -> PathBuf {
    match (to.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => dir.join(to),
    }
}

fn is_rust_source(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "rs")
}

/// Whether `after` carries any hit of `regex` more often than `before`
/// does: what a change adds, as a `deny_pattern` over a write reads it
/// (#510). The count and not the presence, so one more copy of a line the
/// file already carries is an addition and a line moved is not.
pub(super) fn adds(regex: &Regex, after: &str, before: &str) -> bool {
    let mut standing: Vec<&str> = regex.find_iter(before).map(|hit| hit.as_str()).collect();
    regex.find_iter(after).any(
        |hit| match standing.iter().position(|held| *held == hit.as_str()) {
            Some(at) => {
                standing.swap_remove(at);
                false
            }
            None => true,
        },
    )
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
fn adds_a_denied_use(after: &str, before: &str, types: &[String], macros: &[String]) -> bool {
    let after = rust::without_comments_or_literals(after);
    let before = rust::without_comments_or_literals(before);
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
        let uses = |text: &str| adds_a_denied_use(text, "", &types, &macros);
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
        let carried = "use std::sync::Once;\n";
        let adds = |after: &str| adds_a_denied_use(after, carried, &types, &macros);
        assert!(!adds("/// Once more.\nuse std::sync::Once;\nfn a() {}\n"));
        assert!(adds(
            "use std::sync::Once;\nstatic X: Once = Once::new();\n"
        ));
    }
}
