//! The rules: one call against the manifest's `[[rule]]` rows in order,
//! after the region-body rule, and the judgement that comes out, with the
//! hook's JSON it renders to. The deep part of the judge: the conservative
//! shell split, the flag clusters, the type mentions, the `unsafe` homes
//! and the `pub` count all live here and nowhere else.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use regex_lite::Regex;
use serde_json::{Value, json};

use super::call::Call;
use super::region::region_body;
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
/// before the call runs.
pub fn judged_at_the_call(rule: &Rule) -> bool {
    rule.predicate.describes_a_call()
}

/// Judge one call: the region-body rule, then the manifest's rules in
/// order.
pub fn judge(manifest: &Manifest, root: &Path, call: &Call) -> Result<Judgement> {
    let path = call.path(root);
    let mut findings = Vec::new();
    if let Some(path) = &path {
        findings.extend(region_body(root, path, call));
    }
    for rule in &manifest.rules {
        findings.extend(evaluate(manifest, root, rule, call, path.as_deref())?);
    }
    Ok(Judgement { findings })
}

fn evaluate(
    manifest: &Manifest,
    root: &Path,
    rule: &Rule,
    call: &Call,
    path: Option<&Path>,
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
    } else if let Some(argv) = &p.deny_argv {
        call.command()
            .is_some_and(|c| argv_denied(c, argv, p.with.as_deref()))
    } else if let Some(pattern) = &p.deny_pattern {
        let subject = call.command().or_else(|| call.written());
        let regex = Regex::new(pattern)
            .with_context(|| format!("rule \"{}\": deny_pattern is not a regex", rule.id))?;
        subject.is_some_and(|s| regex.is_match(s))
    } else if p.deny == Some(true) {
        true
    } else if let Some(types) = &p.deny_types {
        source && call.written().is_some_and(|t| mentions_a_type(t, types))
    } else if let Some(homes) = &p.allow {
        source
            && call.written().is_some_and(has_unsafe)
            && !path.is_some_and(|path| homes.iter().any(|h| in_home(manifest, h, path)))
    } else if let Some(on) = &p.on {
        on == "pub" && source && adds_a_pub_item(root, call, path)
    } else {
        return Ok(None);
    };
    Ok(fired.then(|| Finding::of(Kind::of(rule), rule)))
}

fn is_rust_source(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "rs")
}

/// The tokens of each simple command in a shell line, split on unquoted
/// whitespace and on `;`, `&&`, `||`, `|`, `&` and newlines, with quotes
/// stripped. Conservative: what a shell can hide is pre-commit's to find.
fn commands(line: &str) -> Vec<Vec<String>> {
    let mut commands = vec![Vec::new()];
    let mut token = String::new();
    let mut quoted = false;
    let mut quote = '\0';
    let mut escaped = false;
    let end_token = |token: &mut String, commands: &mut Vec<Vec<String>>, quoted: &mut bool| {
        if let Some(command) = commands.last_mut().filter(|_| !token.is_empty() || *quoted) {
            command.push(std::mem::take(token));
        }
        *quoted = false;
    };
    for c in line.chars() {
        if escaped {
            token.push(c);
            escaped = false;
        } else if quote != '\0' {
            if c == quote {
                quote = '\0';
            } else if c == '\\' && quote == '"' {
                escaped = true;
            } else {
                token.push(c);
            }
        } else {
            match c {
                '\\' => escaped = true,
                '\'' | '"' => {
                    quote = c;
                    quoted = true;
                }
                ';' | '|' | '&' | '\n' => {
                    end_token(&mut token, &mut commands, &mut quoted);
                    if commands.last().is_some_and(|c| !c.is_empty()) {
                        commands.push(Vec::new());
                    }
                }
                c if c.is_whitespace() => end_token(&mut token, &mut commands, &mut quoted),
                c => token.push(c),
            }
        }
    }
    end_token(&mut token, &mut commands, &mut quoted);
    commands.retain(|c| !c.is_empty());
    commands
}

/// `deny_argv` with `with`: a command whose tokens include the `with` word
/// and one of the denied flags, as a token or as a letter in a cluster of
/// short flags (`-anm` carries `-n`).
fn argv_denied(line: &str, denied: &[String], with: Option<&str>) -> bool {
    commands(line).iter().any(|tokens| {
        with.is_none_or(|w| tokens.iter().any(|t| t == w))
            && tokens
                .iter()
                .any(|t| denied.iter().any(|d| flag_matches(d, t)))
    })
}

fn flag_matches(denied: &str, token: &str) -> bool {
    if token == denied {
        return true;
    }
    let mut letters = denied.chars();
    if letters.next() == Some('-')
        && let (Some(letter), None) = (letters.next(), letters.next())
    {
        let cluster = token.strip_prefix('-').unwrap_or_default();
        return cluster.len() > 1
            && !cluster.starts_with('-')
            && cluster.contains(letter)
            && cluster.chars().all(char::is_alphabetic);
    }
    false
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
    fn a_shell_line_splits_on_separators_and_keeps_quoted_words_whole() {
        let split = commands("git add -A && git commit -m \"fix -n handling\"; echo 'a b' | cat");
        assert_eq!(
            split,
            vec![
                vec!["git", "add", "-A"],
                vec!["git", "commit", "-m", "fix -n handling"],
                vec!["echo", "a b"],
                vec!["cat"],
            ]
        );
        assert_eq!(
            commands("git commit -m ''"),
            vec![vec!["git", "commit", "-m", ""]]
        );
    }

    #[test]
    fn a_denied_flag_is_a_token_or_a_letter_in_a_cluster_beside_the_with_word() {
        let denied = ["--no-verify".to_owned(), "-n".to_owned()];
        assert!(argv_denied("git commit -n -m x", &denied, Some("commit")));
        assert!(argv_denied("git commit -anm x", &denied, Some("commit")));
        assert!(argv_denied(
            "git commit --no-verify",
            &denied,
            Some("commit")
        ));
        assert!(!argv_denied("git log -n 3", &denied, Some("commit")));
        // A quoted flag is still the flag; conservative is the design's word.
        assert!(argv_denied("git commit -m '-n'", &denied, Some("commit")));
        assert!(!argv_denied(
            "git commit -m 'no -n here'",
            &denied,
            Some("commit")
        ));
        assert!(!argv_denied(
            "git commit --no-verify-x",
            &denied,
            Some("commit")
        ));
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
