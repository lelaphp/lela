//! The rules: one call against the manifest's `[[rule]]` rows in order,
//! after the region-body rule, and the judgement that comes out, with the
//! hook's JSON it renders to. The deep part of the judge: the type
//! mentions, the `unsafe` homes and the `pub` count live here, and a shell
//! line's argv in [`super::argv`].

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use regex_lite::Regex;
use serde_json::{Value, json};

use super::argv::Line;
use super::call::Call;
use super::label;
use super::region::region_body;
use super::unless;
use crate::manifest::{Manifest, Rule, scope_matches};
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
}

impl Finding {
    fn of(kind: Kind, rule: &Rule) -> Finding {
        Finding {
            kind,
            rule: rule.id.clone(),
            text: rule.text.clone(),
            adr: rule.adr.iter().map(str::to_owned).collect(),
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
    } else if let Some(denied) = &p.deny_argv {
        line.is_some_and(|l| l.commands().iter().any(|a| a.denies(denied, &p.with)))
    } else if let Some(separator) = &p.require_argv {
        line.is_some_and(|l| l.commands().iter().any(|a| a.unbounded(separator, &p.with)))
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
        // nothing.
        match line {
            Some(line) => unexcused(line.text()) && !excused(line.text()),
            // Text the call writes with no hit in it adds none whatever
            // the file holds, so the file is read only where there is a
            // hit to place.
            None => match call.written().filter(|text| regex.is_match(text)) {
                Some(_) => match call.rewrite(root, path) {
                    Some((before, after)) => {
                        adds(&regex, &after, &before) && unexcused(&after) && !excused(&after)
                    }
                    None => false,
                },
                None => false,
            },
        }
    } else if p.deny == Some(true) {
        true
    } else if let Some(types) = &p.deny_types {
        let macros = p.deny_macros.as_deref().unwrap_or_default();
        source
            && call
                .written()
                .is_some_and(|t| mentions_a_type(t, types) || invokes_a_macro(t, macros))
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
    // names: the call's, moved by a leading `cd` ([`runs_in`]).
    if fired && !rule.within.holds_in(&runs_in(root, line)) {
        return Ok(None);
    }
    Ok(fired.then(|| Finding::of(Kind::of(rule), rule)))
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

/// The directory a call runs in: its workspace, or the directory a shell
/// line's leading `cd` moves to, which is how a session whose shell stands
/// in one checkout runs a command in another.
fn runs_in(root: &Path, line: Option<&Line>) -> std::path::PathBuf {
    let first = line.and_then(|line| line.commands().first());
    match first.map(super::argv::Argv::words) {
        Some([cd, dir]) if cd == "cd" => root.join(dir),
        _ => root.to_path_buf(),
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

/// A `deny_types` entry in the text: the full path, or its last segment as
/// a word; `a::b::*` is the prefix or `b::`.
fn mentions_a_type(text: &str, types: &[String]) -> bool {
    types.iter().any(|ty| {
        let (whole, tail) = match ty.strip_suffix("::*") {
            Some(prefix) => (prefix, "::"),
            None => (ty.as_str(), r"\b"),
        };
        let last = whole.rsplit("::").next().unwrap_or(whole);
        let pattern = format!(
            r"\b{}\b|\b{}{tail}",
            regex_lite::escape(whole),
            regex_lite::escape(last)
        );
        regex(&pattern).is_match(text)
    })
}

/// A `deny_macros` entry invoked in the text: its last segment, bare or
/// behind any path, then `!` and the opening delimiter, so
/// `thread_local! {` and `std::thread_local!(` are both the invocation of
/// `std::thread_local` and a mention of the word in prose is not.
fn invokes_a_macro(text: &str, macros: &[String]) -> bool {
    macros.iter().any(|path| {
        let last = path.rsplit("::").next().unwrap_or(path);
        regex(&format!(r"\b{}\s*!\s*[(\[{{]", regex_lite::escape(last))).is_match(text)
    })
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
        let types = [
            "std::sync::Mutex".to_owned(),
            "std::sync::atomic::*".to_owned(),
        ];
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
}
