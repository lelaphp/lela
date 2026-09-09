//! `xtask permit`, the judge at the tool call (harness-design.md §4.4). It
//! reads one intended tool call as the `PreToolUse` hook receives it, evaluates
//! the region-body check and then every manifest rule that describes a write
//! or a command, in the manifest's order, and prints the hook's JSON: a deny
//! with the rule's text and ADR, an allow whose `additionalContext` carries
//! the notes, or nothing. Exit 0 whatever it decides, since the decision is
//! in the JSON; the one non-zero exit is the refusal to decide at all, a
//! build meeting a manifest it was not compiled against (`built`, #16),
//! which is what each adapter walks its next candidate on. Every denial,
//! every note and every refusal is a reading with `instrument = "permit"`.
//!
//! What permit judges: `deny`, `deny_argv`, `deny_pattern`, `deny_types`,
//! `allow` and `on`. The other four predicates (`check`, `deny_lints`,
//! `avoid_from`, `generated`) are sensors that read the tree after the write
//! and have no meaning for one call; their twins run on save and per commit
//! (§6). Bash arguments are split conservatively on unquoted whitespace and
//! `;`, `&&`, `|`; pre-commit remains the sensor of record for what a shell
//! can hide.

use std::fs;
use std::io::Read;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use regex_lite::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::built;
use crate::git::{self, git};
use crate::manifest::{self, Manifest, Rule, scope_matches};
use crate::reading::{Reading, Status, Store};
use crate::rust;
use crate::worktree::WORKTREES;

/// The instrument name every reading of permit carries.
pub const INSTRUMENT: &str = "permit";
/// The hook event the shipped adapter wires (§6).
const HOOK_EVENT: &str = "PreToolUse";
/// The bar of §4.4: p99 latency at the tool call, in milliseconds.
const BAR_MS: f64 = 100.0;
/// The region-body check is computed's ADR 0005, not a manifest rule; this
/// is the id its readings and denials carry.
pub const REGION_RULE: &str = "computed.region-body";
/// A build refusing to judge by a manifest it was not compiled against is
/// permit's own, not a manifest rule either; this is the id its readings
/// carry (#16).
pub const STALE_RULE: &str = "permit.stale-build";
const REGION_TEXT: &str = "The body of a computed region is rendered by `computed run`, not edited; change the command or its inputs, or the prose around the region (computed ADR 0005).";

/// One intended tool call, as the hook receives it on stdin. `cwd` places
/// a call without a file in a worktree; `session_id` is what a reading
/// carries so denials count per session (§10 rule 6); the other keys of
/// the recorded shape (`permission_mode`, …) are carried by the JSON and
/// not read.
#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: ToolInput,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub session_id: String,
}

/// The fields of `tool_input` a rule can read: an `Edit`'s, a `Write`'s or
/// a `Bash` call's.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolInput {
    pub file_path: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
    #[serde(default)]
    pub replace_all: bool,
    pub content: Option<String>,
    pub command: Option<String>,
}

impl Call {
    /// The call from the hook's JSON.
    pub fn parse(text: &str) -> Result<Call> {
        serde_json::from_str(text).context("reading the tool call")
    }

    /// The text the call would put in the file: an `Edit`'s `new_string` or
    /// a `Write`'s `content`.
    fn written(&self) -> Option<&str> {
        self.tool_input
            .new_string
            .as_deref()
            .or(self.tool_input.content.as_deref())
    }

    fn command(&self) -> Option<&str> {
        self.tool_input.command.as_deref()
    }

    /// The file the call touches, relative to the workspace root; `None`
    /// for a call without one or for a file outside the workspace, which
    /// no rule of the manifest describes.
    pub fn path(&self, root: &Path) -> Option<PathBuf> {
        let given = Path::new(self.tool_input.file_path.as_deref()?);
        let relative = if given.is_absolute() {
            let root = canonical(root);
            canonical(given).strip_prefix(&root).ok()?.to_path_buf()
        } else {
            given.to_path_buf()
        };
        let clean: PathBuf = relative
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        Some(clean)
    }

    /// The checkout the call belongs to: a worktree under
    /// [`WORKTREES`] when the file, or failing that the call's `cwd`, lies
    /// in one, else `root`. An agent's worktree is a checkout of its own
    /// (it has a `.git`), and a path in it is judged against that checkout,
    /// not as `.claude/**` of the main one. A track's worktree is named by
    /// its branch, whose name is a path, so the checkout is the first
    /// directory on the way down that has a `.git`.
    pub fn workspace(&self, root: &Path) -> PathBuf {
        let root = canonical(root);
        let worktrees = root.join(WORKTREES);
        let places = [self.tool_input.file_path.as_deref(), self.cwd.as_deref()];
        places
            .into_iter()
            .flatten()
            .map(|p| canonical(Path::new(p)))
            .find_map(|p| {
                let rest = p.strip_prefix(&worktrees).ok()?;
                let mut checkout = worktrees.clone();
                rest.components().find_map(|component| {
                    checkout.push(component);
                    checkout.join(".git").exists().then(|| checkout.clone())
                })
            })
            .unwrap_or(root)
    }

    /// What a reading names: the path, or the command's first line.
    fn subject(&self, root: &Path) -> String {
        match (self.path(root), self.command()) {
            (Some(path), _) => path.display().to_string(),
            (None, Some(command)) => {
                let line = command.lines().next().unwrap_or_default();
                let mut head: String = line.chars().take(80).collect();
                if head.len() < line.len() {
                    head.push('…');
                }
                head
            }
            (None, None) => String::new(),
        }
    }
}

/// The path with its deepest existing ancestor resolved, so that a file
/// under `/tmp` and one under `/private/tmp` compare equal on macOS and a
/// file that does not exist yet still has a place.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// A rule's verdict on one call: a deny stops the tool, a note rides on
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
    pub fn hook_output(&self) -> Option<Value> {
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
fn deny_output(reason: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": HOOK_EVENT,
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
}

/// Whether a rule describes a write or a command, so permit can judge it
/// before the tool runs.
pub fn judged_at_the_tool_call(rule: &Rule) -> bool {
    rule.predicate.describes_a_call()
}

/// Judge one call: the region-body check, then the manifest's rules in
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

/// One computed region of a Markdown file: its opener line and the byte
/// range of the body between opener and closer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Region<'a> {
    opener: &'a str,
    body: Range<usize>,
}

/// The regions of a file. Markers inside a fenced code block are prose.
fn regions(text: &str) -> Vec<Region<'_>> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    let mut open: Option<(&str, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let end = offset + line.len();
        if let Some(mark) = fence {
            if trimmed.starts_with(mark) {
                fence = None;
            }
        } else if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = Some(&trimmed[..3]);
        } else if trimmed.starts_with("<!-- computed") && trimmed.ends_with("-->") {
            open = Some((trimmed, end));
        } else if trimmed.starts_with("<!-- /computed")
            && let Some((opener, start)) = open.take()
        {
            out.push(Region {
                opener,
                body: start..offset,
            });
        }
        offset = end;
    }
    out
}

/// The region-body check: an `Edit` whose `old_string` lies in a body, or a
/// `Write` that leaves a body other than it found, is denied. A `Write`'s
/// regions pair with the file's by opener, or by position when an opener
/// changed; a region the `Write` removes is a template change, not a body
/// edit.
fn region_body(root: &Path, path: &Path, call: &Call) -> Option<Finding> {
    let existing = fs::read_to_string(root.join(path)).ok()?;
    let bodies = regions(&existing);
    if bodies.is_empty() {
        return None;
    }
    let edited = match call.tool_name.as_str() {
        "Edit" => {
            let old = call.tool_input.old_string.as_deref().unwrap_or_default();
            let mut sites = existing.match_indices(old).map(|(i, _)| i..i + old.len());
            let overlaps = |site: Range<usize>| {
                bodies
                    .iter()
                    .any(|r| site.start < r.body.end && site.end > r.body.start)
            };
            if call.tool_input.replace_all {
                sites.any(overlaps)
            } else {
                sites.next().is_some_and(overlaps)
            }
        }
        "Write" => {
            let content = call.tool_input.content.as_deref().unwrap_or_default();
            let after = regions(content);
            bodies.iter().enumerate().any(|(i, before)| {
                after
                    .iter()
                    .find(|r| r.opener == before.opener)
                    .or_else(|| after.get(i))
                    .is_some_and(|r| content[r.body.clone()] != existing[before.body.clone()])
            })
        }
        _ => false,
    };
    edited.then(|| Finding {
        kind: Kind::Deny,
        rule: REGION_RULE.to_owned(),
        text: REGION_TEXT.to_owned(),
        adr: Vec::new(),
    })
}

/// Which adapter judges the call: the command hook records every
/// finding; the function hook above it records a denial, which ends the
/// call there, and leaves a note to the command hook beneath, which sees
/// the same call and records it once (harness-design.md §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    CommandHook,
    AboveTheCommandHook,
}

impl Adapter {
    fn records(self, kind: Kind) -> bool {
        match self {
            Adapter::CommandHook => true,
            Adapter::AboveTheCommandHook => kind == Kind::Deny,
        }
    }
}

/// Record one reading per finding the adapter records: scope and rule are
/// the rule id, a denial fails and a note passes, the stamp is over the
/// file the call touches. The outcome is dropped: the decision is in the
/// hook's JSON and the exit is 0 whatever it was (harness-design.md §4.4).
fn record(root: &Path, call: &Call, judgement: &Judgement, adapter: Adapter) -> Result<()> {
    let touched = call.path(root).unwrap_or_else(|| PathBuf::from("."));
    let mut readings: Vec<Reading> = Vec::new();
    for finding in judgement
        .findings
        .iter()
        .filter(|f| adapter.records(f.kind))
    {
        // One call is one moment and one tree: the first finding's stamp
        // is every finding's, and git is spawned once per call.
        let mut reading = match readings.first() {
            Some(first) => {
                let mut reading = first.clone();
                reading.scope.clone_from(&finding.rule);
                reading
            }
            None => Reading::stamped_over(INSTRUMENT, &finding.rule, root, &touched)?,
        };
        reading.rule.clone_from(&finding.rule);
        reading.status = match finding.kind {
            Kind::Deny => Status::Fail,
            Kind::Note => Status::Pass,
        };
        reading.message = format!(
            "{} {}: {}",
            call.tool_name,
            call.subject(root),
            finding.text
        );
        reading.adr = finding.adr.first().cloned().unwrap_or_default();
        reading.session.clone_from(&call.session_id);
        readings.push(reading);
    }
    Store::of_workspace(root).record(&readings)?;
    Ok(())
}

/// The command: judge the call on stdin (or in `--json`), print the hook's
/// JSON, record the findings the adapter records. A call this cannot read
/// is a deny that says so; a manifest it cannot judge by is the refusal,
/// the one error returned, since that is the signal each adapter walks its
/// next candidate on (§4.4, #16). The manifest is read here and not by the
/// caller for that reason: a build that cannot read it must record the
/// refusal before it exits.
pub fn run(root: &Path, json: Option<&str>, adapter: Adapter) -> Result<()> {
    let text = json.map_or_else(
        || {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .context("reading the tool call from stdin")
                .map(|_| String::from_utf8_lossy(&bytes).into_owned())
        },
        |json| Ok(json.to_owned()),
    );
    let (root, call) = match text.and_then(|text| Call::parse(&text)) {
        Ok(call) => (call.workspace(root), call),
        Err(err) => {
            println!(
                "{}",
                deny_output(&format!("permit could not judge this call: {err:#}"))
            );
            return Ok(());
        }
    };
    // What this build judges by: the manifest in front of it, the sources
    // it was built from, and the manifest readable as rules. Any of the
    // three failing is a refusal to decide, and a refusal decides nothing
    // whichever way the decision would have fallen.
    let manifest = built::judges(&root, &crate::built_root()).and_then(|()| Manifest::load(&root));
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(err) => {
            record_the_refusal(&root, &call, &err);
            return Err(err);
        }
    };
    match judge(&manifest, &root, &call) {
        Ok(judgement) => {
            if let Some(output) = judgement.hook_output() {
                println!("{output}");
            }
            if let Err(err) = record(&root, &call, &judgement, adapter) {
                eprintln!("xtask permit: the decision stands but was not recorded: {err:#}");
            }
        }
        Err(err) => {
            println!(
                "{}",
                deny_output(&format!("permit could not judge this call: {err:#}"))
            );
        }
    }
    Ok(())
}

/// The refusal as a reading, so the store says the judge was down for this
/// call and this session: through the outage of #16 a build that could not
/// start recorded nothing, and `status --section permits` said the judge
/// was up. Best effort, since the refusal stands whether or not the store
/// takes it.
fn record_the_refusal(root: &Path, call: &Call, err: &anyhow::Error) {
    let recorded = Reading::stamped_over(INSTRUMENT, STALE_RULE, root, Path::new(manifest::FILE))
        .and_then(|mut reading| {
            STALE_RULE.clone_into(&mut reading.rule);
            reading.status = Status::Fail;
            reading.message = format!(
                "{} {}: this call was not judged. {err:#}",
                call.tool_name,
                call.subject(root)
            );
            reading.session.clone_from(&call.session_id);
            Store::of_workspace(root).record(&[reading])
        });
    // The refusal is the exit already; the outcome of recording it is not.
    if let Err(err) = recorded {
        eprintln!("xtask permit: the refusal to judge was not recorded: {err:#}");
    }
}

/// What permit found over a set of files, each judged as the `Write` of
/// its content: `done`'s condition 5 and pre-commit's `--diff` read one
/// verdict (harness-design.md §4.4, §4.5, §6). A denial under a rule the
/// manifest marks `deny = true`, the harness's own files, stands apart: a
/// person commits those, and the hook cannot tell a person from an agent,
/// so it refuses nothing and is reported as a note.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Judged {
    /// Each file's judgement, in the order given.
    pub files: Vec<(String, Judgement)>,
    /// The ids of the rules the manifest marks `deny = true`.
    a_persons: Vec<String>,
}

impl Judged {
    /// The denials that refuse the diff: every `Deny` finding outside a
    /// person's rules.
    pub fn denials(&self) -> impl Iterator<Item = (&str, &Finding)> {
        self.findings(Kind::Deny)
            .filter(|(_, f)| !self.a_persons.contains(&f.rule))
    }

    /// The harness's own files in the diff: a `Deny` under a person's rule.
    pub fn a_persons(&self) -> impl Iterator<Item = (&str, &Finding)> {
        self.findings(Kind::Deny)
            .filter(|(_, f)| self.a_persons.contains(&f.rule))
    }

    /// The notes that ride on the diff.
    pub fn notes(&self) -> impl Iterator<Item = (&str, &Finding)> {
        self.findings(Kind::Note)
    }

    fn findings(&self, kind: Kind) -> impl Iterator<Item = (&str, &Finding)> {
        self.files.iter().flat_map(move |(path, judgement)| {
            judgement
                .findings
                .iter()
                .filter(move |f| f.kind == kind)
                .map(move |f| (path.as_str(), f))
        })
    }

    /// The lines pre-commit prints: one per finding, the rule id in each.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (path, finding) in self.denials() {
            out.push_str("deny ");
            out.push_str(path);
            out.push_str(": ");
            out.push_str(&finding.reason());
            out.push('\n');
        }
        for (path, finding) in self.a_persons().chain(self.notes()) {
            out.push_str("note ");
            out.push_str(path);
            out.push_str(": ");
            out.push_str(&finding.reason());
            out.push('\n');
        }
        out
    }

    /// Every finding as a reading, as the hook records one call's.
    pub fn record(&self, root: &Path) -> Result<()> {
        for (path, judgement) in &self.files {
            record(root, &write_of(path, None), judgement, Adapter::CommandHook)?;
        }
        Ok(())
    }
}

/// The `Write` that would put `content` at `path`.
fn write_of(path: &str, content: Option<&str>) -> Call {
    Call {
        tool_name: "Write".to_owned(),
        tool_input: ToolInput {
            file_path: Some(path.to_owned()),
            content: content.map(str::to_owned),
            ..ToolInput::default()
        },
        cwd: None,
        session_id: String::new(),
    }
}

/// Judge `files`, each a path and the content that would be committed
/// there.
pub fn judge_files(manifest: &Manifest, root: &Path, files: &[(String, String)]) -> Result<Judged> {
    let a_persons = manifest
        .rules
        .iter()
        .filter(|r| r.predicate.deny == Some(true))
        .map(|r| r.id.clone())
        .collect();
    let mut judged = Vec::new();
    for (path, content) in files {
        let judgement = judge(manifest, root, &write_of(path, Some(content)))?;
        judged.push((path.clone(), judgement));
    }
    Ok(Judged {
        files: judged,
        a_persons,
    })
}

/// The staged files with their staged content: what a commit would take.
/// A deleted file has nothing to judge.
pub fn staged_files(root: &Path) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    for change in git::staged(root)?.into_iter().filter(|c| !c.deleted) {
        if let Some(content) = git::staged_content(root, &change.path)? {
            files.push((change.path, content));
        }
    }
    Ok(files)
}

/// The changed files with their working-tree content, as `done` judges
/// them; a deleted or vanished file has nothing to judge.
pub fn working_files(root: &Path, changed: &[git::Change]) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    for change in changed.iter().filter(|c| !c.deleted) {
        let file = root.join(&change.path);
        if !file.is_file() {
            continue;
        }
        let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        files.push((
            change.path.clone(),
            String::from_utf8_lossy(&bytes).into_owned(),
        ));
    }
    Ok(files)
}

/// `permit --diff`: judge the staged diff, print the findings, fail while
/// anything is denied, so the commit is refused with the rule id.
pub fn run_diff(root: &Path, manifest: &Manifest) -> Result<()> {
    let files = staged_files(root)?;
    let diff = judge_files(manifest, root, &files)?;
    diff.record(root)?;
    print!("{}", diff.render());
    let denied = diff.denials().count();
    if denied == 0 {
        Ok(())
    } else {
        bail!(
            "permit denies {denied} of {} file{} in the diff",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        )
    }
}

/// What `--bench` measured: the quantiles over every call, and over each
/// shape, since a denial pays for its reading and an allow does not.
#[derive(Debug, Clone, PartialEq)]
struct Bench {
    runs: usize,
    all: Quantiles,
    shapes: Vec<(String, Quantiles)>,
}

/// Three points of one distribution of call times, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Quantiles {
    p50: f64,
    p99: f64,
    max: f64,
}

impl Quantiles {
    fn of(times: &mut [f64]) -> Quantiles {
        times.sort_by(f64::total_cmp);
        Quantiles {
            p50: quantile(times, 50),
            p99: quantile(times, 99),
            max: quantile(times, 100),
        }
    }
}

impl Bench {
    /// The bar is on every call the hook makes, denials included.
    fn meets_the_bar(&self) -> bool {
        self.all.p99 <= BAR_MS
    }
}

/// The nearest-rank quantile of sorted samples: the smallest sample that
/// at least `percent` of them do not exceed.
fn quantile(sorted: &[f64], percent: usize) -> f64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// The shapes `--bench` cycles: an allowed Edit that reads a file for the
/// region check, an allowed Bash call, and a denied Edit that writes a
/// reading.
fn bench_shapes() -> [(&'static str, Value); 3] {
    [
        (
            "allow-edit",
            json!({"tool_name": "Edit", "tool_input": {"file_path": "README.md", "old_string": "#", "new_string": "##"}}),
        ),
        (
            "allow-bash",
            json!({"tool_name": "Bash", "tool_input": {"command": "cargo test -p xtask"}}),
        ),
        (
            "deny-edit",
            json!({"tool_name": "Edit", "tool_input": {"file_path": "seams.toml", "old_string": "a", "new_string": "b"}}),
        ),
    ]
}

/// Time `runs` calls of this binary as the hook would make them, over a
/// scratch copy of the workspace's manifest under `target/`, so the denials
/// the bench provokes are stamped and stored there and not here.
fn bench(root: &Path, runs: usize) -> Result<Bench> {
    if runs == 0 {
        bail!("--runs must be at least 1");
    }
    let scratch = root.join("target").join("harness-bench");
    scratch_workspace(root, &scratch)?;
    let exe = std::env::current_exe().context("locating this binary")?;
    let shapes = bench_shapes();
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); shapes.len()];
    for i in 0..runs {
        let which = i % shapes.len();
        let stdin = shapes[which].1.to_string();
        let start = Instant::now();
        let mut child = Command::new(&exe)
            .arg("--root")
            .arg(&scratch)
            .arg("permit")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning xtask permit")?;
        if let Some(mut pipe) = child.stdin.take() {
            std::io::Write::write_all(&mut pipe, stdin.as_bytes()).context("feeding the call")?;
        }
        let out = child
            .wait_with_output()
            .context("waiting for xtask permit")?;
        times[which].push(start.elapsed().as_secs_f64() * 1000.0);
        if !out.status.success() {
            bail!(
                "xtask permit exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let mut all: Vec<f64> = times.iter().flatten().copied().collect();
    Ok(Bench {
        runs,
        all: Quantiles::of(&mut all),
        shapes: shapes
            .iter()
            .zip(&mut times)
            .filter(|(_, t)| !t.is_empty())
            .map(|((name, _), t)| ((*name).to_owned(), Quantiles::of(t)))
            .collect(),
    })
}

/// A git repository at `dir`, rebuilt from scratch, holding the
/// workspace's manifest, its pin, the divergence registry and a README:
/// where `--bench` and the tests provoke denials, so the readings land in
/// its store and not the workspace's.
pub fn scratch_workspace(root: &Path, dir: &Path) -> Result<()> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir.join("docs")).with_context(|| format!("creating {}", dir.display()))?;
    let manifest = Manifest::load(root)?;
    for file in [
        crate::manifest::FILE,
        manifest.oracle.pin.as_str(),
        "docs/divergence.toml",
        "README.md",
    ] {
        fs::copy(root.join(file), dir.join(file)).with_context(|| format!("copying {file}"))?;
    }
    git(dir, &["init", "-q"], None)?;
    git(dir, &["add", "."], None)?;
    git(
        dir,
        &[
            "-c",
            "user.name=xtask",
            "-c",
            "user.email=xtask@scratch",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
        None,
    )?;
    Ok(())
}

/// `permit --bench`: measure, print one line per shape and the verdict,
/// record the reading, over the harness crate whose sources the judge is.
pub fn run_bench(root: &Path, manifest: &Manifest, store: &Store, runs: usize) -> Result<()> {
    let measured = bench(root, runs)?;
    let verdict = if measured.meets_the_bar() {
        "pass"
    } else {
        "fail"
    };
    let line = |name: &str, q: &Quantiles| {
        format!(
            "{name}: p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
            q.p50, q.p99, q.max
        )
    };
    println!(
        "permit --bench: {} runs; {}; bar p99 ≤ {BAR_MS:.0} ms: {verdict}",
        measured.runs,
        line("all", &measured.all)
    );
    for (name, q) in &measured.shapes {
        println!("  {}", line(name, q));
    }
    let mut reading = Reading::for_scope(INSTRUMENT, crate::done::HARNESS, manifest, root)?;
    reading.status = if measured.meets_the_bar() {
        Status::Pass
    } else {
        Status::Fail
    };
    reading
        .numbers
        .insert("runs".into(), i64::try_from(measured.runs)?.into());
    reading.numbers.insert("bar_ms".into(), BAR_MS.into());
    reading
        .numbers
        .insert("p50_ms".into(), measured.all.p50.into());
    reading
        .numbers
        .insert("p99_ms".into(), measured.all.p99.into());
    reading
        .numbers
        .insert("max_ms".into(), measured.all.max.into());
    for (name, q) in &measured.shapes {
        let key = name.replace('-', "_");
        reading
            .numbers
            .insert(format!("{key}_p50_ms"), q.p50.into());
        reading
            .numbers
            .insert(format!("{key}_p99_ms"), q.p99.into());
    }
    reading.message = format!(
        "permit at the tool call: p99 {:.1} ms against the bar of {BAR_MS:.0} ms (harness-design.md §4.4)",
        measured.all.p99
    );
    store.record(&[reading])?.exit()
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
        assert!(m("**/*.rs", "xtask/src/permit.rs"));
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
    fn regions_are_bodies_between_markers_outside_fences() {
        let text = "a\n<!-- computed tree src=. name=x -->\nbody\n<!-- /computed -->\n```\n<!-- computed exec cmd=y -->\nprose\n<!-- /computed -->\n```\n<!-- computed exec cmd=z volatile -->\n<!-- /computed -->\n";
        let found = regions(text);
        assert_eq!(found.len(), 2);
        assert_eq!(&text[found[0].body.clone()], "body\n");
        assert_eq!(&text[found[1].body.clone()], "");
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
