//! `xtask permit`, the judge at the hook's call (harness-design.md §4.4).
#![cfg(tracked)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::divergence::Registry;
use xtask::manifest::{Manifest, OneOrMany, Rule, Where};
use xtask::oracle::Promise;
use xtask::permit::{Call, Finding, Kind, judge, judged_at_the_call};
use xtask::reading::{Reading, Status, Store};
use xtask::scratch::Tree;
use xtask::witness::{self, Accept, Ask};

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
    pub mod tracker;
}
use common::manifest::Tracked;
use common::repo::{Repo, scratch_workspace};
use common::run::{Ran, Run, xtask};
use common::tracker::{Tracker, issue, labels};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixtures() -> PathBuf {
    xtask::package_dir().join("fixtures")
}

fn call(path: &Path) -> Call {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Call::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// An `allow-excused*.json` neighbour, the call an `unless_argv` lets
/// through, while the tracked row carries none: a person has yet to paste
/// it, and the row's own test holds it under the row as proposed (#3068).
fn unpasted(rule: &Rule, name: &str) -> bool {
    name.starts_with("allow-excused") && rule.predicate.unless_argv.is_none()
}

/// The manifest with the `git.pathspec` row #33 proposes, as the tracked
/// file carries it or as the constructors build it.
fn with_the_pathspec_rule(mut manifest: Manifest) -> Manifest {
    if manifest.rule("git.pathspec").is_none() {
        let mut rule = Rule::named("git.pathspec", "");
        rule.scope = OneOrMany::None;
        rule.tool = OneOrMany::One("Bash".to_owned());
        rule.predicate.deny = None;
        rule.predicate.require_argv = Some("--".to_owned());
        rule.predicate.with = OneOrMany::Many(vec!["git add".to_owned(), "git commit".to_owned()]);
        "An agent stages and commits by pathspec, `git add -- <paths>` and `git commit -F <file> -- <paths>`: the index is shared with every session in the checkout."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    manifest
}

fn bash(line: &str) -> Call {
    Call::parse(
        &serde_json::json!({"tool_name": "Bash", "tool_input": {"command": line}}).to_string(),
    )
    .unwrap()
}

/// The `lanes.nightly-flags` row #3052 ruled and #3068 proposes, as a
/// person would paste it: `--jit` passes on `xtask phpt` where `--subset`
/// names one subset, and `--full`, `--jit` with no subset and `xtask
/// bench --jit` stay denied.
const PROPOSED_NIGHTLY_FLAGS: &str = r#"
tool = "Bash"
deny_argv = ["--full", "--jit"]
unless_argv = { "--jit" = "xtask phpt --subset" }   # grammar: unless_argv, a denied element let through where the phrase runs bounded by the option, as require_argv reads one: the tier's one-subset reading (#3052, #3068)
with = ["xtask phpt", "xtask bench"]   # xtask however it is run: the cargo alias, cargo run -p xtask, a path, a variable the line assigns (#1860)
text = "Nightly lanes run in CI only; run the per-crate subset, and the tier's `--jit` lane on one subset named by `--subset` (#3052)."
"#;

/// The `git.no-verify` row #1001 proposes: `with` names git's `commit` and
/// not the word, and the global option that points the hooks elsewhere is
/// denied beside `-n` and `--no-verify`.
const PROPOSED_NO_VERIFY: [&str; 4] = [
    "--no-verify",
    "-n",
    "-c core.hooksPath",
    "--config-env core.hooksPath",
];

/// A worktree carries the tracked manifest, so it is a workspace of its
/// own by the one rule (`Call::workspace`, #57); it also has a `.git`, so
/// it is the checkout its readings go to. This one is gitignored.
fn lay_worktree(worktree: &Path) {
    std::fs::create_dir_all(worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: nowhere\n").unwrap();
    std::fs::copy(workspace().join("seams.toml"), worktree.join("seams.toml")).unwrap();
}

/// A worktree laid down beside the checkout, where `worktree add` lays
/// one (revision 56), is a checkout of the repository outside the root:
/// a file in it is judged against that worktree's manifest, and is
/// neither outside every workspace nor read against the checkout's. Git's
/// own `.git` says which repository a checkout is of, so a clone laid
/// beside it with a manifest of its own is another repository's, and its
/// files stay outside the workspace as before.
#[test]
fn a_file_in_a_worktree_beside_the_checkout_is_judged_against_that_worktree() {
    let tree = Tree::new("permit-beside").unwrap();
    let root = tree.join("repo");
    scratch_workspace(workspace(), &root);
    let beside = tree.join("repo-worktrees/t-x/beside");
    Repo::at(&root).git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "t-x/beside",
        beside.to_str().unwrap(),
    ]);
    let marker = "edited by a person, says the worktree's manifest";
    Tracked::of(&beside)
        .with_rule("harness.self", |rule| {
            // The marker beside the rule's own words, whatever they are.
            let text = format!("{} Here: {marker}.", rule["text"].as_str().unwrap());
            rule.insert("text".to_owned(), text.into());
        })
        .write(&beside);

    let edit = |file: &Path| {
        Call::parse(
            &serde_json::json!({"tool_name": "Edit", "cwd": root.to_str().unwrap(),
                "tool_input": {"file_path": file.to_str().unwrap(),
                    "old_string": "a", "new_string": "b"}})
            .to_string(),
        )
        .unwrap()
    };
    let call = edit(&beside.join("oracle.toml"));
    let judged_in = call.workspace(&root);
    assert_eq!(judged_in, beside, "the worktree beside is the workspace");
    assert_eq!(call.path(&judged_in).unwrap(), Path::new("oracle.toml"));
    let judged = judge(&Manifest::load(&judged_in).unwrap(), &judged_in, &call).unwrap();
    let denial = judged
        .denial()
        .expect("the pin is a person's in the worktree too");
    assert_eq!(denial.rule, "harness.self");
    assert!(denial.reason().contains(marker), "{}", denial.reason());

    // A Bash call is placed by its cwd, there as under the root.
    let shell = Call::parse(
        &serde_json::json!({"tool_name": "Bash", "cwd": beside.to_str().unwrap(),
            "tool_input": {"command": "cargo test -p xtask"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(shell.workspace(&root), beside);
    // And a file places a call by itself, wherever the shell stands: an
    // edit in the checkout from a session in the worktree is the
    // checkout's, and one in the worktree asked of the worktree is the
    // worktree's own (#1047).
    let across = Call::parse(
        &serde_json::json!({"tool_name": "Edit", "cwd": beside.to_str().unwrap(),
            "tool_input": {"file_path": root.join("oracle.toml").to_str().unwrap(),
                "old_string": "a", "new_string": "b"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(across.workspace(&root), root.canonicalize().unwrap());
    let within = edit(&beside.join("oracle.toml"));
    assert_eq!(
        within.workspace(&beside),
        beside.canonicalize().unwrap(),
        "not the checkout the session stands in"
    );

    // A clone beside is another repository: its files are no workspace's.
    let clone = tree.join("repo-t-y");
    Repo::at(&tree).git(&[
        "clone",
        "-q",
        root.to_str().unwrap(),
        clone.to_str().unwrap(),
    ]);
    let call = edit(&clone.join("oracle.toml"));
    assert_eq!(call.workspace(&root), root);
    assert_eq!(call.path(&root), None);
}

/// A worktree beside a scratch checkout, on a branch of its own: the
/// place `worktree add` lays one (revision 56).
fn worktree_beside(tree: &Path, root: &Path, branch: &str) -> PathBuf {
    let beside = tree.join("repo-worktrees").join(branch);
    Repo::at(root).git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        branch,
        beside.to_str().unwrap(),
    ]);
    beside.canonicalize().unwrap()
}

/// The hook's stdin for one call, placed by `cwd` when it has no file.
fn hook_stdin(tool: &str, cwd: &Path, input: &serde_json::Value) -> String {
    serde_json::json!({"session_id": "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11",
        "tool_name": tool, "cwd": cwd.to_str().unwrap(), "tool_input": input})
    .to_string()
}

/// A worktree laid before a `seams.toml` change carries rules no build of
/// the checkout is compiled against, and each tree is judged by the rules
/// it carries: the checkout's build hands a call there to the worktree's
/// own, which judges it against its own tree, and returns its decision
/// (#1047). A rule only the worktree's manifest carries fires on a write
/// there and not on the same write in the checkout; a Bash call is placed
/// by its working directory the same way, and a row the worktree words
/// differently is denied in the worktree's words; the readings are the
/// worktree's build's, in its store. The walk in the worktree passes over
/// the checkout's build seeded into its `target/debug`, which refuses a
/// manifest it was not compiled against, and `cargo run` there builds the
/// harness under test against the worktree's manifest.
#[test]
fn a_call_in_a_worktree_whose_manifest_differs_is_judged_by_that_worktrees_build() {
    let tree = Tree::new("permit-hand-off").unwrap();
    let root = tree.join("repo");
    xtask::scratch::repository(workspace(), &root, &["."]).unwrap();
    let beside = worktree_beside(&tree, &root, "t-x/handed");
    let marker = "not available to agents, says the worktree's manifest";
    Tracked::of(&beside)
        .with_rule("git.no-verify", |rule| {
            // The marker beside the rule's own words, whatever they are.
            let text = format!("{} Here: {marker}.", rule["text"].as_str().unwrap());
            rule.insert("text".to_owned(), text.into());
        })
        .with_rule("worktree.only", |rule| {
            rule.extend(
                r#"
tool = ["Edit", "Write"]
scope = "only-here/**"
deny = true
text = "A file under only-here/ is refused by the worktree's manifest alone."
"#
                .parse::<toml::Table>()
                .unwrap(),
            );
        })
        .write(&beside);
    // The checkout's dependency build, seeded the way `worktree add` seeds
    // it; where the volume cannot clone, the build there starts cold.
    std::fs::create_dir_all(beside.join("target")).unwrap();
    Command::new("cp")
        .args(["-c", "-R"])
        .arg(workspace().join("target/debug"))
        .arg(beside.join("target/debug"))
        .output()
        .ok();

    let write = |file: &Path| {
        hook_stdin(
            "Write",
            &root,
            &serde_json::json!({"file_path": file.to_str().unwrap(), "content": "one\n"}),
        )
    };
    let ran = permit(&root, &write(&beside.join("only-here/one.txt")), &[]);
    assert_eq!(ran.code(), 0, "the worktree's build judged the call: {ran}");
    let out = ran.stdout;
    let err = ran.stderr;
    assert!(out.contains("worktree.only"), "{out}{err}");
    assert!(out.contains("\"deny\""), "{out}");
    let reading = Store::of_workspace(&beside)
        .read("permit", "worktree.only")
        .unwrap()
        .expect("the worktree's build recorded its denial in its own store");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        Store::of_workspace(&root)
            .read("permit", "permit.unjudged-call")
            .unwrap()
            .is_none()
            && Store::of_workspace(&beside)
                .read("permit", "permit.unjudged-call")
                .unwrap()
                .is_none(),
        "nothing went unjudged"
    );

    // The same write in the checkout is the checkout's to judge, by rules
    // that carry no such row.
    let out = permit(&root, &write(&root.join("only-here/one.txt")), &[]).ok();
    assert!(!out.contains("worktree.only"), "{out}");
    assert!(!out.contains("\"deny\""), "{out}");

    // A Bash call is placed by its working directory: in the worktree it
    // is denied in the worktree's words, in the checkout in the checkout's.
    let commit = |cwd: &Path| {
        hook_stdin(
            "Bash",
            cwd,
            &serde_json::json!({"command": "git commit --no-verify -m x"}),
        )
    };
    let ran = permit(&root, &commit(&beside), &[]);
    assert!(ran.stdout.contains(marker), "the worktree's rule: {ran}");
    ran.ok();
    let out = permit(&root, &commit(&root), &["--above-the-command-hook"]).ok();
    assert!(out.contains("git.no-verify"), "{out}");
    assert!(!out.contains(marker), "the checkout's rule: {out}");
}

/// A hand-off that does not run decides nothing, and nothing is allowed on
/// its silence (#1047): the worktree here has no build to hand the call
/// to, so the checkout's build exits without judging, as it did before
/// there was a hand-off, and the call reaches the end of the hook's walk
/// unjudged. Every candidate of that walk hands the call to the same
/// worktree, so it is the failure no rebuild repairs, in the worktree's
/// store, naming the worktree's manifest.
#[test]
fn a_hand_off_that_does_not_run_leaves_the_call_unjudged() {
    let tree = Tree::new("permit-hand-off-fails").unwrap();
    let root = tree.join("repo");
    scratch_workspace(workspace(), &root);
    let beside = worktree_beside(&tree, &root, "t-x/unbuilt");
    // A row a person has since settled.
    Tracked::of(&beside)
        .with_crate("php_heap", |row| row.budget += 1)
        .write(&beside);
    let stdin = hook_stdin(
        "Write",
        &root,
        &serde_json::json!({"file_path": beside.join("README.md").to_str().unwrap(),
            "content": "one\n"}),
    );
    let ran = permit(&root, &stdin, &[]);
    assert_ne!(ran.code(), 0, "the call was not judged: {ran}");
    let out = ran.stdout;
    let err = ran.stderr;
    assert!(out.trim().is_empty(), "and nothing was decided: {out}");
    assert!(
        err.contains(&beside.join("seams.toml").display().to_string()),
        "{err}"
    );
    let reading = Store::of_workspace(&beside)
        .read("permit", "permit.unjudged-call")
        .unwrap()
        .expect("the refusal was recorded");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        Store::of_workspace(&beside)
            .read("permit", "permit.stale-build")
            .unwrap()
            .is_none(),
        "no rebuild of the checkout judges it"
    );
}

/// The unsafe attribute written into a file of the `unsafe.homes` fixture.
fn edit_of(file: &Path) -> Call {
    Call::parse(
        &serde_json::json!({"tool_name": "Edit", "cwd": workspace().to_str().unwrap(),
            "tool_input": {"file_path": file.to_str().unwrap(), "old_string": "}\n",
                "new_string": "}\n\n#[unsafe(no_mangle)]\npub extern \"C\" fn go() {}\n"}})
        .to_string(),
    )
    .unwrap()
}

/// The `ext.no-locks` row #1781 ruled, in the fixture that carries it word
/// for word (#1888): permit denies the once type and the thread-local,
/// each a write into an extension, and lets the neighbour through, whose
/// prose names `thread_local` without invoking it. The thread-local is
/// denied by `deny_macros` alone: under the same row without it, the type
/// ban lets the write through. Once the tracked manifest carries the row,
/// `ext.no-locks`' own fixture walk holds the rule as it holds it today.
#[test]
fn the_ruled_no_locks_row_denies_a_once_type_and_a_thread_local_and_not_their_neighbour() {
    let root = fixtures().join("clippy/once-and-thread-local");
    let manifest = Manifest::load(&root).unwrap();
    let denied = |manifest: &Manifest, file: &str| {
        judge(manifest, &root, &call(&root.join(file)))
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
    };
    for file in ["once.json", "thread-local.json"] {
        assert_eq!(
            denied(&manifest, file).as_deref(),
            Some("ext.no-locks"),
            "{file}"
        );
    }
    assert_eq!(denied(&manifest, "allow.json"), None);

    let mut types_alone = manifest.clone();
    for rule in &mut types_alone.rules {
        rule.predicate.deny_macros = None;
    }
    assert_eq!(denied(&types_alone, "thread-local.json"), None);
    assert_eq!(
        denied(&types_alone, "once.json").as_deref(),
        Some("ext.no-locks")
    );
}

/// A fixture without a manifest of its own is judged by the workspace's,
/// as before #57: its directory is under the workspace and nothing
/// between them carries a `seams.toml`.
#[test]
fn a_fixture_without_a_manifest_is_judged_by_the_workspaces() {
    let file = fixtures().join("rules/ext.no-locks/deny.json");
    assert!(
        !file.parent().unwrap().join("seams.toml").exists(),
        "the fixture carries no manifest"
    );
    let call = edit_of(&file);
    assert_eq!(
        call.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    assert_eq!(
        call.path(workspace()).unwrap(),
        Path::new("xtask/fixtures/rules/ext.no-locks/deny.json")
    );
}

/// `permit --diff` and `done` apply the same resolution to a diff that
/// touches a fixture source (#57): the diff judge resolves each file's
/// workspace as the hook does, so a source in a home of the fixture's
/// manifest passes and one outside it is refused with the fixture's text.
#[test]
fn diff_judges_a_fixture_source_by_the_fixture_manifest() {
    let root = scratch("diff-fixture");
    let fixture = "xtask/fixtures/rules/unsafe.homes";
    let manifest = Tracked::of(&fixtures().join("rules/unsafe.homes"))
        .with_the_workspace_pin()
        .text();
    write(&root, &format!("{fixture}/seams.toml"), &manifest);
    write(
        &root,
        &format!("{fixture}/ext/pcre/Cargo.toml"),
        "[package]\nname = \"pcre\"\n",
    );
    let site = "pub fn read(p: *const u8) -> u8 {\n    unsafe { *p }\n}\n";
    write(&root, &format!("{fixture}/ext/pcre/src/ffi/mod.rs"), site);
    Repo::at(&root).git(&["add", "-A"]);
    let ran = permit(&root, "", &["--diff"]);
    assert_eq!(ran.code(), 0, "a home of the fixture's manifest: {ran}");

    write(
        &root,
        &format!("{fixture}/crates/php_unit/src/lib.rs"),
        site,
    );
    Repo::at(&root).git(&["add", "-A"]);
    let ran = permit(&root, "", &["--diff"]);
    assert_eq!(ran.code(), 1, "outside the fixture's homes: {ran}");
    let out = ran.stdout;
    assert!(
        out.contains(&format!("deny {fixture}/crates/php_unit/src/lib.rs"))
            && out.contains("(fixture)"),
        "the path is the checkout's and the text the fixture's: {out}"
    );
}

/// The label a rule does not judge (#341, harness-design.md ledger 52).
/// `ticket.close` denies `gh issue close` so that nothing closes a ticket
/// but the instrument that read its acceptance criteria; a chart sheet
/// carries none, so the row names `frontier:chart` and the close of a
/// filled sheet is not the act the rule is for. The label is the
/// tracker's fact, so permit asks the tracker, here a [`Tracker`] first
/// on `PATH` and never the network: the sheet's close is let through, the
/// same call on a ticket carrying no such label is denied as before, and
/// a number the tracker answers is no issue leaves the denial standing.
#[test]
fn a_close_is_denied_unless_the_tracker_says_the_issue_carries_the_exempt_label() {
    let root = scratch("exempt-label");
    let tracker = Tracker::empty("exempt-label");
    tracker
        .with_issue(&xtask::ticket::Issue {
            labels: labels(&["frontier:chart", "track:t-opus-5"]),
            ..issue(460)
        })
        .with_issue(&xtask::ticket::Issue {
            labels: labels(&["ready-for-agent"]),
            ..issue(28)
        })
        .none(9001);

    let judged = |command: &str| -> Option<String> {
        let stdin = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command, "description": "close" }
        })
        .to_string();
        let path = format!(
            "{}:{}",
            tracker.bin().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let ran = Run::at(&root)
            .args(["permit"])
            .env("PATH", path)
            .stdin(stdin)
            .go();
        assert_eq!(ran.code(), 0, "permit decides with its JSON: {ran}");
        let stdout = ran.stdout;
        let text = stdout.trim();
        if text.is_empty() {
            return None;
        }
        let got: serde_json::Value = serde_json::from_str(text).unwrap();
        Some(
            got["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    };

    assert_eq!(
        judged("gh issue close 460 --comment 'sheet 7 continues this'"),
        None,
        "the sheet carries the label the rule does not judge"
    );
    let ticket = judged("gh issue close 28 --comment 'done'").expect("a ticket is denied");
    assert!(ticket.contains("ticket.close"), "{ticket}");
    let unknown = judged("gh issue close 9001").expect("an issue the tracker will not answer for");
    assert!(unknown.contains("ticket.close"), "{unknown}");
}

/// `permit <args>` over `root` with `stdin` on its stdin, as a hook runs it.
fn permit(root: &Path, stdin: &str, args: &[&str]) -> Ran {
    Run::at(root).args(["permit"]).args(args).stdin(stdin).go()
}

/// A scratch workspace that is a git repository with the tracked manifest
/// and pin, so a denial there is stamped and stored without touching this
/// workspace's store.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("permit-{name}")).unwrap();
    scratch_workspace(workspace(), &dir);
    dir
}

/// The stdin shapes recorded on Claude Code 2.1.263 (data/pre-day-1.md §4)
/// produce the recorded stdout shapes.
#[test]
fn the_recorded_stdin_shapes_produce_the_recorded_stdout_shapes() {
    let root = scratch("shapes");
    let dir = fixtures().join("hooks");
    let stdin = std::fs::read_to_string(dir.join("pretooluse-allow.json")).unwrap();
    let out = permit(&root, &stdin, &[]).ok();
    assert!(
        out.trim().is_empty(),
        "an allow with nothing to say prints nothing: {out}"
    );

    let stdin = std::fs::read_to_string(dir.join("pretooluse-deny.json")).unwrap();
    let out = permit(&root, &stdin, &[]).ok();
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    let want: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("pretooluse-deny.stdout.json")).unwrap(),
    )
    .unwrap();
    let head = &got["hookSpecificOutput"];
    assert_eq!(
        head["hookEventName"],
        want["hookSpecificOutput"]["hookEventName"]
    );
    assert_eq!(
        head["permissionDecision"],
        want["hookSpecificOutput"]["permissionDecision"]
    );
    let reason = head["permissionDecisionReason"].as_str().unwrap();
    assert!(reason.contains("harness.self"), "{reason}");
    assert!(reason.contains("edited by a person"), "{reason}");
    assert_eq!(
        got.as_object().unwrap().len(),
        1,
        "the deny is the recorded shape and nothing beside it: {out}"
    );
    assert_eq!(head.as_object().unwrap().len(), 3, "{out}");
}

#[test]
fn a_denial_is_a_reading_with_instrument_permit_and_the_rule_id() {
    let root = scratch("reading");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    permit(&root, &stdin, &[]).ok();
    let reading = Store::of_workspace(&root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.instrument, "permit");
    assert_eq!(reading.rule, "harness.self");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.contains("seams.toml"),
        "{}",
        reading.message
    );
    assert!(reading.message.starts_with("Edit "), "{}", reading.message);
    assert_eq!(reading.key().len(), 40);
}

#[test]
fn a_note_rides_on_an_allow_as_additional_context_and_is_a_reading() {
    let root = scratch("note");
    let stdin = std::fs::read_to_string(fixtures().join("rules/api.lock-line/note.json")).unwrap();
    let out = permit(&root, &stdin, &[]).ok();
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    let head = &got["hookSpecificOutput"];
    assert_eq!(head["hookEventName"], "PreToolUse");
    assert!(
        head.get("permissionDecision").is_none(),
        "a note does not decide: {out}"
    );
    let context = head["additionalContext"].as_str().unwrap();
    assert!(context.contains("api.lock-line"), "{context}");
    let reading = Store::of_workspace(&root)
        .read("permit", "api.lock-line")
        .unwrap()
        .expect("the note was recorded");
    assert_eq!(reading.status, Status::Pass);
}

/// The function-hook adapter judges a call before the command hook does
/// (harness-design.md §4.4): its denial ends the call and is recorded
/// here, while an allow goes on to the command hook, which records the
/// notes, so a note is one line of the log and not two.
#[test]
fn above_the_command_hook_a_denial_is_recorded_and_a_note_is_left_to_it() {
    let root = scratch("above");
    let dir = fixtures();
    let stdin = std::fs::read_to_string(dir.join("rules/api.lock-line/note.json")).unwrap();
    let out = permit(&root, &stdin, &["--above-the-command-hook"]).ok();
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    let context = got["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("api.lock-line"), "{context}");
    let store = Store::of_workspace(&root);
    assert!(
        store.read("permit", "api.lock-line").unwrap().is_none(),
        "the note is the command hook's to record"
    );

    let stdin = std::fs::read_to_string(dir.join("hooks/pretooluse-deny.json")).unwrap();
    let out = permit(&root, &stdin, &["--above-the-command-hook"]).ok();
    assert!(out.contains("\"deny\""), "{out}");
    let reading = store
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial ends the call here and is recorded here");
    assert_eq!(reading.status, Status::Fail);
}

/// #2734 as it happened: an agent's merge stands in main's checkout, its
/// starter recorded, and its line aborting it there and starting a merge
/// in its own worktree is judged by both adapters, the function hook's
/// first. That admission dropped the record, as the abort's own, and
/// recorded the worktree's start, before the command hook beneath judged
/// the same call; that one found the merge standing with no record,
/// refused the abort, and every later abort of the starter's with it. The
/// operation is recorded by the command hook alone, the adapter that
/// judges a call last, so the abort is the starter's under both.
#[test]
fn an_abort_the_function_hook_admits_is_its_starter_s_under_the_command_hook_too() {
    let root = scratch("operation-adapters");
    let worktree = root.join("own");
    Repo::at(&root).git(&["worktree", "add", "-q", "-b", "own", "own"]);
    let line = |command: &str| {
        serde_json::json!({
            "session_id": "s1",
            "agent_id": "comp",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
        })
        .to_string()
    };
    let admitted = |stdin: &str, args: &[&str], what: &str| {
        let out = permit(&root, stdin, args).ok();
        assert!(!out.contains("\"deny\""), "{what} {args:?}: {out}");
    };
    let above: &[&str] = &["--above-the-command-hook"];
    let start = line("git merge --no-ff --no-commit topic");
    for adapter in [above, &[]] {
        admitted(&start, adapter, "the start");
    }
    std::fs::write(root.join(".git/MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();
    let abort = line(&format!(
        "C={}; git -C $C merge --abort; git -C $C status --short | head -3; \
         git -C $C log --oneline -1; W={}; git -C $W status --short | head; \
         git -C $W merge main 2>&1 | tail -3",
        root.display(),
        worktree.display()
    ));
    for adapter in [above, &[]] {
        admitted(&abort, adapter, "the starter's abort");
    }
}

/// The adapter hands the call to permit on stdin, which no argument-length
/// limit bounds (harness-design.md §13 D.17): a Write of several megabytes
/// is judged whole.
#[test]
fn a_large_write_on_stdin_is_judged_whole() {
    let root = scratch("large");
    let content = format!(
        "{}\nstatic X: std::sync::Mutex<u8> = todo!();\n",
        "// ".repeat(1 << 20)
    );
    assert!(content.len() > 3_000_000);
    let call = serde_json::json!({
        "session_id": "s", "cwd": root.path(), "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": "ext/pcre/src/cache.rs", "content": content},
    });
    let out = permit(&root, &call.to_string(), &[]).ok();
    let reason = serde_json::from_str::<serde_json::Value>(&out).unwrap()["hookSpecificOutput"]
        ["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        reason.contains("ext.no-locks"),
        "the lock at the end of the file was read: {reason}"
    );
}

#[test]
fn a_call_that_cannot_be_read_is_denied_not_dropped() {
    let root = scratch("garbage");
    let out = permit(&root, "not json", &[]).ok();
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(got["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn bench_measures_p99_against_the_bar_and_writes_a_reading() {
    let root = scratch("bench");
    let out = permit(&root, "", &["--bench", "--runs", "6"]).ok();
    assert!(out.contains("p99"), "{out}");
    assert!(out.contains("100 ms"), "the bar is named: {out}");
    // The judge's cost is a reading over the harness crate, whose sources
    // the judge is; the store's key is the manifest's scope.
    let reading = Store::of_workspace(&root)
        .read("permit", "xtask")
        .unwrap()
        .expect("the bench is a reading");
    assert_eq!(reading.numbers["runs"], 6.into());
    assert_eq!(reading.key().len(), 40, "stamped over the harness crate");
    assert!(
        reading.numbers.contains_key("p99_ms"),
        "{:?}",
        reading.numbers
    );
    assert!(
        Store::of_workspace(&root)
            .read("permit", "harness.self")
            .unwrap()
            .is_none(),
        "the bench's own denials do not land in the store it reports to"
    );
}

/// The bench is a measurement, which owes no exit (harness-design.md §4.4, #61):
/// with the bar missed the exit is 0, the reading records `fail` and the
/// numbers, and the verdict is printed for a person to read.
#[test]
fn bench_with_the_bar_missed_exits_zero_and_records_fail() {
    let root = scratch("bench-missed");
    let ran = permit(&root, "", &["--bench", "--runs", "3", "--bar", "0"]);
    assert_eq!(
        ran.code(),
        0,
        "a measurement's exit is not its status: {ran}"
    );
    let out = ran.stdout;
    assert!(out.contains("bar p99 ≤ 0 ms: fail"), "{out}");
    let reading = Store::of_workspace(&root)
        .read("permit", "xtask")
        .unwrap()
        .expect("the bench is a reading");
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.numbers["runs"], 3.into());
    assert_eq!(reading.numbers["bar_ms"], 0.0.into());
    assert!(
        reading.numbers["p99_ms"].as_f64() > 0.0,
        "{:?}",
        reading.numbers
    );
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, text).unwrap();
}

/// `permit --diff`, pre-commit's caller (harness-design.md §4.4, §6): the
/// staged files are judged as the `Write` of their staged content, a
/// denial refuses the commit with the rule id, and the harness's own
/// files, which a person commits, are a note and not a refusal.
#[test]
fn diff_judges_the_staged_files_and_refuses_a_denied_one_with_the_rule_id() {
    let root = scratch("diff");
    let ran = permit(&root, "", &["--diff"]);
    assert_eq!(ran.code(), 0, "nothing staged is nothing denied: {ran}");

    let locked = "use std::sync::Mutex;\n\npub struct Cache(Mutex<Vec<u8>>);\n";
    write(&root, "ext/pcre/src/cache.rs", locked);
    Repo::at(&root).git(&["add", "ext/pcre/src/cache.rs"]);
    let ran = permit(&root, "", &["--diff"]);
    assert_eq!(
        ran.code(),
        1,
        "a denied staged file refuses the commit: {ran}"
    );
    let out = ran.stdout;
    assert!(
        out.contains("ext.no-locks"),
        "the rule id is printed: {out}"
    );
    assert!(out.contains("ext/pcre/src/cache.rs"), "{out}");
    let reading = Store::of_workspace(&root)
        .read("permit", "ext.no-locks")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.starts_with("Write ext/pcre/src/cache.rs"),
        "{}",
        reading.message
    );

    // What is judged is the index, not the working tree.
    write(
        &root,
        "ext/pcre/src/cache.rs",
        "pub struct Cache(Vec<u8>);\n",
    );
    let ran = permit(&root, "", &["--diff"]);
    assert_eq!(
        ran.code(),
        1,
        "the staged content still holds the lock: {ran}"
    );
    Repo::at(&root).git(&["add", "ext/pcre/src/cache.rs"]);
    permit(&root, "", &["--diff"]).ok();

    // The harness's own file: a note, since the hook cannot tell a person
    // from an agent, and the commit goes through.
    // A person's note.
    Tracked::of(&root)
        .with_crate("php_heap", |row| row.budget += 1)
        .write(&root);
    Repo::at(&root).git(&["add", "seams.toml"]);
    let out = permit(&root, "", &["--diff"]).ok();
    assert!(out.contains("note seams.toml"), "{out}");
    assert!(out.contains("harness.self"), "{out}");
}

/// A Mach-O header, a byte that is no UTF-8 anywhere, and the name
/// `ext.no-private` denies in an extension's text: git reads it as binary
/// (`ls-files --eol`'s `-text`), so no rule reads what it holds.
const BINARY: &[u8] = b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01\xff php_ext::__private\n";

/// A staged binary file is judged by its path alone: `permit --diff`
/// read every staged file as UTF-8 and refused the whole diff on one,
/// "git wrote something that is not UTF-8", which kept t3468's stand-in
/// php builds out of its fixture (#3458). The name its bytes spell is no
/// denial, a denied text file beside it still refuses the commit, and a
/// binary at a harness path is that path's note.
#[test]
fn diff_judges_a_staged_binary_file_by_its_path_alone() {
    let root = scratch("diff-binary");
    let put = |path: &str| {
        Repo::at(&root)
            .write(path, BINARY)
            .git(&["add", "--", path])
    };
    put("ext/pcre/src/stand-in.so");
    let ran = permit(&root, "", &["--diff"]);
    assert_eq!(ran.code(), 0, "a binary is judged by its path: {ran}");
    assert!(!ran.stderr.contains("not UTF-8"), "{ran}");

    let locked = "use std::sync::Mutex;\n\npub struct Cache(Mutex<Vec<u8>>);\n";
    write(&root, "ext/pcre/src/cache.rs", locked);
    Repo::at(&root).git(&["add", "ext/pcre/src/cache.rs"]);
    let ran = permit(&root, "", &["--diff"]);
    assert_eq!(ran.code(), 1, "the text file beside it is judged: {ran}");
    assert!(ran.stdout.contains("deny ext/pcre/src/cache.rs"), "{ran}");
    assert!(!ran.stdout.contains("stand-in.so"), "{ran}");
    Repo::at(&root).git(&["rm", "--cached", "-q", "ext/pcre/src/cache.rs"]);

    put(".claude/hooks/stand-in");
    let out = permit(&root, "", &["--diff"]).ok();
    assert!(out.contains("note .claude/hooks/stand-in"), "{out}");
    assert!(out.contains("harness.self"), "{out}");
}

/// A binary the merge takes whole from the merged branch is that
/// branch's, as a text file is (#1312): a stand-in at a harness path the
/// branch committed is judged at the branch's own commits, so the merge
/// carrying it notes nothing.
#[test]
fn a_binary_the_merge_takes_whole_is_the_merged_branchs() {
    let root = scratch("merge-binary");
    let repo = Repo::at(&root);
    let stand_in = ".claude/hooks/stand-in";
    repo.git(&["commit", "-q", "--allow-empty", "-m", "the base"]);
    repo.git(&["checkout", "-q", "-b", "branch"]);
    repo.write(stand_in, BINARY).git(&["add", "--", stand_in]);
    repo.git(&["commit", "-q", "-m", "the stand-in"]);
    repo.git(&["checkout", "-q", "-"]);
    repo.git(&["commit", "-q", "--allow-empty", "-m", "main moves on"]);
    repo.git(&["merge", "-q", "--no-ff", "--no-commit", "branch"]);
    let out = permit(&root, "", &["--diff"]).ok();
    assert!(!out.contains("stand-in"), "{out}");
}

/// The hook's stdin carries `session_id`; a reading carries it, so the
/// `permits` section can count denials per session (harness-design.md
/// §4.6, §10 rule 6).
#[test]
fn a_denial_carries_the_session_of_the_call() {
    let root = scratch("session");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    permit(&root, &stdin, &[]).ok();
    let reading = Store::of_workspace(&root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");

    permit(
        &root,
        "",
        &[
            "--json",
            r#"{"tool_name":"Edit","tool_input":{"file_path":"seams.toml","old_string":"a","new_string":"b"}}"#,
        ],
    ).ok();
    let log = Store::of_workspace(&root).history("permit").unwrap();
    assert_eq!(
        log.readings.last().unwrap().session,
        "",
        "a call without a session is recorded without one"
    );
}

// ----------------------------------------------- the wiring at the tool call

/// The `PreToolUse` entries hooks.json declares, each with the tools its
/// matcher names.
fn pre_tool_use_entries() -> Vec<(Vec<String>, serde_json::Value)> {
    let text =
        std::fs::read_to_string(workspace().join("harness-plugins/hooks/hooks.json")).unwrap();
    let hooks: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entries = hooks["hooks"]["PreToolUse"]
        .as_array()
        .unwrap_or_else(|| panic!("no PreToolUse entry: {hooks}"))
        .clone();
    entries
        .into_iter()
        .map(|entry| {
            let tools = entry["matcher"]
                .as_str()
                .unwrap_or_else(|| panic!("an entry without a matcher: {entry}"))
                .split('|')
                .map(str::to_owned)
                .collect();
            (tools, entry)
        })
        .collect()
}

/// The one command hooks.json declares for one `tool_name` at `PreToolUse`: the
/// line the hook runs before every call of it. Two entries walk the same
/// candidates and differ in what they do when none judges, so the `tool_name`
/// picks the line (#16 items 2 and 5).
fn pre_tool_use_command(tool: &str) -> String {
    let commands: Vec<String> = pre_tool_use_entries()
        .into_iter()
        .filter(|(tools, _)| tools.iter().any(|t| t == tool))
        .flat_map(|(_, entry)| entry["hooks"].as_array().cloned().unwrap_or_default())
        .map(|hook| hook["command"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        commands.len(),
        1,
        "one command judges a {tool} call: {commands:?}"
    );
    commands.into_iter().next().unwrap()
}

/// `PreToolUse` hands the call to permit, over the three tools the design
/// names, from the workspace root (harness-design.md §4.4). Each of them is
/// named by exactly one entry, so no call is judged twice by the command
/// hook and none is left to no entry at all.
#[test]
fn pre_tool_use_judges_the_three_tools_through_permit() {
    for tool in JUDGED {
        let named: Vec<_> = pre_tool_use_entries()
            .into_iter()
            .filter(|(tools, _)| tools.iter().any(|t| t == tool))
            .collect();
        assert_eq!(named.len(), 1, "{tool} is named by one entry: {named:?}");
        let command = pre_tool_use_command(tool);
        assert!(command.contains("cd \"$CLAUDE_PROJECT_DIR\""), "{command}");
        assert!(command.contains("permit"), "{command}");
    }
}

/// The tools permit judges, the matchers' union (harness-design.md §4.4).
const JUDGED: [&str; 3] = ["Edit", "Write", "Bash"];

/// The candidate that closes the list rebuilds rather than trusting a stale
/// build, so the hook's budget has to cover a build and not only a judging:
/// a warm recompile of xtask measured 4.88s when the fallback was written.
/// A budget too small for it times the hook out, and a call that times out
/// is a call that passed unjudged, which is the window the fallback closed
/// (harness-design.md §4.4). Every entry, since each walks that candidate.
#[test]
fn the_hook_leaves_room_for_the_rebuild_its_last_candidate_may_do() {
    for (tools, entry) in pre_tool_use_entries() {
        let timeout = entry["hooks"][0]["timeout"]
            .as_i64()
            .unwrap_or_else(|| panic!("{tools:?} declares no timeout"));
        assert!(
            timeout >= 60,
            "the cargo candidate needs room to rebuild: {tools:?} {timeout}"
        );
    }
}

/// The two adapters do not resolve the built binaries in one order (#16
/// item 4): the command hook takes the release build first and the module
/// beside it the debug one, so one stale build no longer silences both
/// layers at once, which is how an hour of calls went unjudged. The
/// candidate that closes each list is the same rebuild, so a tree in which
/// every build is stale still ends in one place.
#[test]
fn the_two_adapters_do_not_walk_the_built_binaries_in_one_order() {
    let command = pre_tool_use_command("Edit");
    let walk: Vec<&str> = command
        .split_once("for x in ")
        .unwrap_or_else(|| panic!("no walk in {command}"))
        .1
        .split_once("; do")
        .unwrap_or_else(|| panic!("no walk in {command}"))
        .0
        .split_whitespace()
        .collect();
    let module =
        std::fs::read_to_string(workspace().join("harness-plugins/hooks/harness.ts")).unwrap();
    let listed = module
        .split_once("const BUILT = [")
        .expect("the module lists the builds it walks")
        .1
        .split_once(']')
        .expect("a list that closes")
        .0;
    let order: Vec<&str> = listed
        .split(',')
        .map(|path| path.trim().trim_matches('\''))
        .filter(|path| !path.is_empty())
        .collect();
    assert_eq!(walk.len(), 2, "the command hook walks two builds: {walk:?}");
    assert_eq!(order.len(), 2, "and the module the same two: {order:?}");
    let mut by_name = (walk.clone(), order.clone());
    by_name.0.sort_unstable();
    by_name.1.sort_unstable();
    assert_eq!(by_name.0, by_name.1, "the same builds");
    assert_ne!(walk, order, "in orders that do not fall together");
}

/// What a build does when the command runs it: one that stands judges, and
/// one older than the manifest or the flag it is handed exits without
/// judging, the way a build from before `[telemetry]` landed did.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum Build {
    Judges,
    DidNotJudge,
}

/// Lay a build down under the scratch workspace. One that judges prints its
/// name and the call it read, so what the command hands each candidate is
/// measured beside which candidate ran.
#[cfg(unix)]
fn build(root: &Path, path: &str, who: &str, how: Build) {
    use std::os::unix::fs::PermissionsExt;
    let text = match how {
        Build::Judges => format!("#!/bin/sh\nin=$(cat)\nprintf 'JUDGED={who} SAW=%s' \"$in\"\n"),
        Build::DidNotJudge => {
            format!("#!/bin/sh\ncat >/dev/null\necho '{who}: unknown field' >&2\nexit 1\n")
        }
    };
    write(root, path, &text);
    std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run the declared command as the hook does: the call on stdin, the
/// workspace in `CLAUDE_PROJECT_DIR`, and the scratch `bin` ahead of PATH so
/// the cargo the command closes with is the one laid down here.
#[cfg(unix)]
fn pre_tool_use(root: &Path, tool: &str, stdin: &str) -> (i32, String, String) {
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(pre_tool_use_command(tool))
        .current_dir(root)
        .env("CLAUDE_PROJECT_DIR", root)
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// permit's exit is 0 whatever it decides, so a non-zero exit is a build
/// that never judged: the command walks its candidates until one does, and
/// cargo closes the list, rather than the call passing on a stale build's
/// silence (harness-design.md §4.4). The two adapters resolve the binary in
/// the same order, so a build that stood for both left the call unjudged
/// beneath a hook that had already failed open above it. The walk is one
/// walk, whichever entry declares it, so both are held to it.
#[cfg(unix)]
#[test]
fn a_build_that_did_not_judge_hands_the_call_to_the_next_candidate() {
    let call = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let call = call.trim();
    for tool in JUDGED {
        the_walk_is_the_same_for(tool, call);
    }
}

#[cfg(unix)]
fn the_walk_is_the_same_for(tool: &str, call: &str) {
    // The release build stands: it judges, and no other candidate runs.
    let root = scratch(&format!("wiring-stands-{tool}"));
    build(&root, "target/release/xtask", "release", Build::Judges);
    build(&root, "target/debug/xtask", "debug", Build::Judges);
    build(&root, "bin/cargo", "cargo", Build::Judges);
    let (code, out, err) = pre_tool_use(&root, tool, call);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("JUDGED=release"), "{out}");
    assert!(!out.contains("JUDGED=debug"), "one build judges: {out}");
    assert!(!out.contains("JUDGED=cargo"), "one build judges: {out}");
    assert!(out.contains(call), "the call reaches the build that judges");

    // The release build is older than the manifest: the next one judges.
    let root = scratch(&format!("wiring-stale-{tool}"));
    build(&root, "target/release/xtask", "release", Build::DidNotJudge);
    build(&root, "target/debug/xtask", "debug", Build::Judges);
    build(&root, "bin/cargo", "cargo", Build::Judges);
    let (code, out, err) = pre_tool_use(&root, tool, call);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("JUDGED=debug"), "{out} / {err}");
    assert!(!out.contains("JUDGED=cargo"), "{out}");
    assert!(
        out.contains(call),
        "the call the first build read reaches the second"
    );

    // No build stands: cargo, which rebuilds rather than trusting one.
    let root = scratch(&format!("wiring-cargo-{tool}"));
    build(&root, "target/release/xtask", "release", Build::DidNotJudge);
    build(&root, "target/debug/xtask", "debug", Build::DidNotJudge);
    build(&root, "bin/cargo", "cargo", Build::Judges);
    let (code, out, err) = pre_tool_use(&root, tool, call);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("JUDGED=cargo"), "{out} / {err}");
    assert!(out.contains(call), "the call reaches cargo");
}

/// Where the two entries part (#16 items 2 and 5): with every candidate
/// refusing, an Edit or a Write is denied, since a write no judge saw does
/// not proceed, and a Bash call is handed on, since an agent repairs a
/// broken harness with Bash and denying all three deadlocks the repair. A
/// deny is exit 0 with the JSON (D.16); the hand-on is the non-zero exit
/// the host reports as a non-blocking error, and the call proceeds.
#[cfg(unix)]
#[test]
fn with_no_candidate_judging_a_write_is_denied_and_a_bash_call_is_handed_on() {
    let call = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let call = call.trim();
    for tool in JUDGED {
        let root = scratch(&format!("wiring-none-{tool}"));
        build(&root, "target/release/xtask", "release", Build::DidNotJudge);
        build(&root, "target/debug/xtask", "debug", Build::DidNotJudge);
        build(&root, "bin/cargo", "cargo", Build::DidNotJudge);
        let (code, out, err) = pre_tool_use(&root, tool, call);
        if tool == "Bash" {
            assert_ne!(code, 0, "a Bash call is handed on: {out}");
            assert!(out.trim().is_empty(), "with no decision of its own: {out}");
            continue;
        }
        assert_eq!(code, 0, "a deny is exit 0 with the JSON: {err}");
        let got: serde_json::Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("{tool}: {e}: {out} / {err}"));
        let head = &got["hookSpecificOutput"];
        assert_eq!(head["hookEventName"], "PreToolUse");
        assert_eq!(head["permissionDecision"], "deny");
        let reason = head["permissionDecisionReason"].as_str().expect(&out);
        assert!(reason.contains("did not judge"), "{reason}");
        assert!(
            reason.contains("cargo build -p xtask"),
            "the repair: {reason}"
        );
    }
}

// ------------------------------ the build against the manifest it judges by

/// The manifest an earlier build was compiled against (#16): it parses
/// under today's grammar and judges the same call differently, which an
/// exit code cannot see.
fn stale_manifest() -> String {
    std::fs::read_to_string(fixtures().join("stale/seams.toml")).unwrap()
}

/// A scratch workspace holding that manifest: a build meeting rules it was
/// not compiled against.
fn scratch_with_a_stale_manifest(name: &str) -> Tree {
    let root = scratch(name);
    std::fs::write(root.join("seams.toml"), stale_manifest()).unwrap();
    root
}

fn bash_call(command: &str) -> Call {
    Call::parse(
        &serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command, "description": "one call" }
        })
        .to_string(),
    )
    .unwrap()
}

/// permit's exit is 0 whatever it decides, so the refusal is the one thing
/// it says with an exit: a build that would judge by rules it was not
/// compiled against decides nothing, names both digests, and leaves the
/// call to the next candidate (#16, harness-design.md §4.4). The call this
/// one reads is denied by both manifests, and the refusal still stands: a
/// decision that happens to agree is not a decision this build may make.
#[test]
fn a_build_the_manifest_moved_under_refuses_to_judge_rather_than_judging_stale() {
    let root = scratch_with_a_stale_manifest("stale");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let ran = permit(&root, &stdin, &[]);
    assert_ne!(
        ran.code(),
        0,
        "the build that cannot judge exits non-zero: {ran}"
    );
    let out = ran.stdout;
    let err = ran.stderr;
    assert!(out.trim().is_empty(), "and decides nothing: {out}");
    assert!(err.contains("seams.toml"), "{err}");
    // The digest compiled in, sha1 as `build.rs` prints it: a 40-digit
    // hex word beside the manifest's, which the refusal names both of.
    assert!(
        err.split(|c: char| !c.is_ascii_hexdigit())
            .filter(|w| w.len() == 40)
            .count()
            >= 2,
        "both digests are named: {err}"
    );
}

/// The refusal is a reading, so the store says what the judge did with the
/// call where the facts already are; through the outage of #16 nothing
/// said it. A build apart from the tree hands the call to the candidate
/// that rebuilds, which judges it, so the reading is a skip that names
/// where the call went and not a failure (#118): the store of the main
/// checkout held 5687 such failures against 23 denials, and the one number
/// a person wants was under them.
#[test]
fn the_refusal_that_hands_the_call_on_is_a_skip_that_names_the_next_candidate() {
    let root = scratch_with_a_stale_manifest("stale-reading");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let ran = permit(&root, &stdin, &[]);
    assert_ne!(ran.code(), 0, "{ran}");
    let reading = Store::of_workspace(&root)
        .read("permit", "permit.stale-build")
        .unwrap()
        .expect("the refusal was recorded");
    assert_eq!(reading.instrument, "permit");
    assert_eq!(reading.rule, "permit.stale-build");
    assert_eq!(
        reading.status,
        Status::Skip,
        "a call that went on was judged: {}",
        reading.message
    );
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");
    assert!(
        reading.message.contains("next candidate"),
        "and the reading says where it went: {}",
        reading.message
    );
    assert!(
        reading.message.contains("seams.toml"),
        "{}",
        reading.message
    );
    assert_eq!(reading.key().len(), 40);
    assert!(
        Store::of_workspace(&root)
            .read("permit", "permit.unjudged-call")
            .unwrap()
            .is_none(),
        "nothing reached the end of the walk unjudged"
    );
}

/// The outage of #16 was the loud staleness: a manifest no build reads at
/// all. It exited without judging, which the fallback walks, and every
/// candidate walked to did the same, so the call proceeded unjudged and
/// `status --section permits` said the judge was up for the hour it was
/// down. That one stays a failure, since no rebuild repairs it (#118). The
/// manifest here is a fixture workspace's, the one place a build standing
/// over the checkout it was compiled against still meets rules it cannot
/// read (#57).
#[test]
fn a_manifest_no_candidate_can_read_is_a_failure_the_build_records() {
    let root = scratch("unjudged");
    let nested = root.join("xtask/fixtures/unreadable");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("seams.toml"),
        "[oracle]\npin = \"oracle.toml\"\n\n[a_table_of_a_later_revision]\ndays = 14\n",
    )
    .unwrap();
    let stdin = serde_json::json!({
        "session_id": "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11",
        "tool_name": "Write",
        "tool_input": {
            "file_path": "xtask/fixtures/unreadable/one.rs",
            "content": "fn one() {}\n"
        }
    })
    .to_string();
    let ran = permit(&root, &stdin, &[]);
    assert_ne!(
        ran.code(),
        0,
        "the build that cannot judge exits non-zero: {ran}"
    );
    let out = ran.stdout;
    let err = ran.stderr;
    assert!(out.trim().is_empty(), "and decides nothing: {out}");
    assert!(err.contains("seams.toml"), "{err}");
    let reading = Store::of_workspace(&root)
        .read("permit", "permit.unjudged-call")
        .unwrap()
        .expect("the refusal was recorded");
    assert_eq!(reading.rule, "permit.unjudged-call");
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");
    assert!(
        Store::of_workspace(&root)
            .read("permit", "permit.stale-build")
            .unwrap()
            .is_none(),
        "this build stands over the manifest it was compiled against"
    );
}

/// A denial is the other half of #118 and it does not move: a call a rule
/// refuses is a failing reading under that rule, with the call and the
/// rule's text on it, whatever the refusals beside it read as.
#[test]
fn a_denial_is_still_a_failing_reading_under_the_rule_that_denied_it() {
    let root = scratch("denial-reading");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let out = permit(&root, &stdin, &[]).ok();
    assert!(out.contains("harness.self"), "{out}");
    let reading = Store::of_workspace(&root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.rule, "harness.self");
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");
}

/// A build standing over the manifest it was compiled against judges as it
/// did: the sensor fires on staleness and on nothing else.
#[test]
fn a_build_over_the_manifest_it_was_compiled_against_judges_as_it_did() {
    let root = scratch("stale-control");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let out = permit(&root, &stdin, &[]).ok();
    assert!(out.contains("harness.self"), "{out}");
    for rule in ["permit.stale-build", "permit.unjudged-call"] {
        assert!(
            Store::of_workspace(&root)
                .read("permit", rule)
                .unwrap()
                .is_none(),
            "nothing was refused: {rule}"
        );
    }
}

/// The manifest with the two `ticket.close*` rows #28 proposes, as the
/// tracked file carries them or as the constructors build them.
fn with_the_ticket_close_rules(mut manifest: Manifest) -> Manifest {
    if manifest.rule("ticket.close").is_none() {
        let mut rule = Rule::named("ticket.close", "");
        rule.scope = OneOrMany::None;
        rule.tool = OneOrMany::One("Bash".to_owned());
        rule.predicate.deny = None;
        rule.predicate.deny_argv = Some(vec!["close".to_owned(), "reopen".to_owned()]);
        rule.predicate.with = OneOrMany::One("gh issue".to_owned());
        "A ticket closes through `xtask ticket close`, the instrument that read its acceptance criteria; `gh issue close` and `gh issue reopen` are not available to agents."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    if manifest.rule("ticket.close-api").is_none() {
        let mut rule = Rule::named("ticket.close-api", "");
        rule.scope = OneOrMany::None;
        rule.tool = OneOrMany::One("Bash".to_owned());
        rule.predicate.deny = None;
        rule.predicate.deny_pattern = Some(
            r#"gh\s+api\b[^|;&]*([\s'"]state=(closed|open)\b|"state"\s*:\s*"(closed|open)"|--input\b|(close|reopen)Issue\()"#.to_owned(),
        );
        "The same act through `gh api`: an issue's state is set by `xtask ticket close` and by nothing else."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    manifest
}

// ------------------------------------------------------- the `unless` sensor

/// The rule `unless` qualifies, and the crate its fixture writes into.
const EXIT_AUTHORED: &str = "witness.exit-authored";
const CRATE: &str = "ext/ctype";
const FILE: &str = "ext/ctype/src/lib.rs";

/// The `Write` that would put `text` at `path`.
fn write_call(path: &str, text: &str) -> Call {
    Call::parse(
        &serde_json::json!({
            "tool_name": "Write",
            "tool_input": {"file_path": path, "content": text},
        })
        .to_string(),
    )
    .unwrap()
}

/// A rustdoc block whose snippet exits, its `expect` fence pinning
/// `status`: `exit=3` as `--accept` captures it, `exit` as the opt-in
/// stands before it.
fn block(status: &str) -> String {
    format!(
        "/// ```php\n/// exit(3);\n/// ```\n/// ```expect {status}\n/// ```\npub fn native() {{}}\n"
    )
}

/// The crate's sources: its `Cargo.toml`, which is what says the crate
/// exists at all, and the rustdoc block under test.
fn lay_down(root: &Path, text: &str) {
    std::fs::create_dir_all(root.join(FILE).parent().unwrap()).unwrap();
    std::fs::write(
        root.join(CRATE).join("Cargo.toml"),
        "[package]\nname = \"php_ext_ctype\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(root.join(FILE), text).unwrap();
}

/// The reading `xtask witness` writes over the crate, stamped over its
/// sources as they stand now.
fn witness_reading(root: &Path) {
    let mut reading = Reading::stamped_over(
        "witness",
        CRATE,
        &xtask::key::Snapshot::disk(root).unwrap(),
        Path::new(CRATE),
    )
    .unwrap();
    reading.status = Status::Pass;
    reading.message = format!("{CRATE}: measured");
    Store::of_workspace(root).record(&[reading]).unwrap();
}

fn denial(root: &Path, manifest: &Manifest, call: &Call) -> Option<String> {
    judge(manifest, root, call)
        .unwrap()
        .denial()
        .map(xtask::permit::Finding::reason)
}

/// The `Edit` that would put `new` where `old` stands in `path`.
fn edit_call(path: &str, old: &str, new: &str) -> Call {
    Call::parse(
        &serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": path, "old_string": old, "new_string": new},
        })
        .to_string(),
    )
    .unwrap()
}

/// What a call at the hook is measured against is the file it would change
/// (#510): a write that adds an `exit=` is denied whatever the store holds,
/// since the authoring the row refuses happens here, and one that leaves
/// every pinned verdict where it stood adds none and is permitted with no
/// reading behind it. The count decides and not the words, so a second copy
/// of the very line the file carries is a line added. The evidence `unless`
/// reads is the diff judge's question, held by
/// [`what_accept_wrote_permit_diff_permits_over_the_same_tree`].
#[test]
fn a_write_that_adds_an_exit_is_denied_and_one_that_adds_none_is_permitted() {
    let root = scratch("unless");
    let manifest = Manifest::load(&root).unwrap();
    let captured = block("exit=3");
    lay_down(&root, &captured);
    let write = write_call(FILE, &captured);

    // The file's own content: nothing is authored, and no reading is
    // needed to say so.
    assert_eq!(
        denial(&root, &manifest, &write),
        None,
        "a write that leaves the pinned verdict where it stood"
    );
    witness_reading(&root);
    assert_eq!(denial(&root, &manifest, &write), None);

    // The call the rule is for: a status this file does not carry.
    let authored = block("exit=7");
    let denied = denial(&root, &manifest, &write_call(FILE, &authored))
        .expect("an authored status the file has not");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    // One more copy of the line the file already carries is one more all
    // the same, which a fresh reading over the crate does not excuse.
    let denied = denial(
        &root,
        &manifest,
        &write_call(FILE, &format!("{captured}{captured}")),
    )
    .expect("a second copy of the pinned line is a line added");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    // The shape #510 named: an edit beside a pinned verdict, in a file
    // that carries one, adding another. The status is built from the
    // number, as `block` builds it, so this file carries no fence line of
    // its own for the row to read.
    let status = 3;
    let fence = format!("/// ```expect exit={status}");
    let beside = edit_call(
        FILE,
        "pub fn native() {}",
        &format!("{fence}\npub fn native() {{}}"),
    );
    let denied = denial(&root, &manifest, &beside).expect("the edit brings a second verdict");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    // And an edit that moves the prose around the pinned verdict adds
    // nothing, which is the edit the row used to refuse.
    let around = edit_call(FILE, &fence, &format!("/// A note:\n{fence}"));
    assert_eq!(denial(&root, &manifest, &around), None);
}

/// A `deny_pattern` over a write is asked of the file the write leaves and
/// not of the fragment it types (#2413): an edit whose `new_string` holds
/// no hit of its own can still finish one, a digit changed after `exit=`,
/// and that is a status authored. A pattern over a block, which the
/// fragment of an edit inside the block never holds whole, rests on the
/// same reading.
#[test]
fn an_edit_that_finishes_a_hit_outside_its_own_text_is_denied() {
    let root = scratch("unless-fragment");
    let manifest = Manifest::load(&root).unwrap();
    lay_down(&root, &block("exit=3"));
    let finished = edit_call(FILE, "t=3\n", "t=4\n");
    let denied = denial(&root, &manifest, &finished).expect("exit=4 is authored");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");
    let prose = edit_call(FILE, "pub fn native", "pub fn other");
    assert_eq!(denial(&root, &manifest, &prose), None);
}

/// The evidence this cannot read is no evidence: a crate the manifest does
/// not name has no reading to stand on, so a write that adds the line to a
/// file there is denied however the tree looks.
#[test]
fn an_exit_in_a_file_no_crate_row_names_is_denied() {
    let root = scratch("unless-uncrated");
    let manifest = Manifest::load(&root).unwrap();
    let text = block("exit=3");
    let file = "vendor/other/src/lib.rs";
    std::fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
    std::fs::write(root.join(file), "pub fn native() {}\n").unwrap();
    let denied = denial(&root, &manifest, &write_call(file, &text)).expect("no crate, no reading");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle_is_here() -> bool {
    let pin = Manifest::load(workspace()).unwrap().pin.unwrap();
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
        .is_some()
}

/// A merge commit displaces both its parents (#1312): a verdict the merged
/// branch's `--accept` captured is the branch's, so `permit --diff` over
/// the staged merge permits it with no witness reading over the merged
/// tree, and a verdict the merge itself adds is still denied.
#[test]
fn a_verdict_the_merged_branch_carried_is_the_branchs_and_not_the_merges() {
    let root = scratch("merge-verdict");
    let repo = Repo::at(&root);
    let tail = "pub fn other() {}\n";
    lay_down(&root, &format!("{}{tail}", block("exit")));
    repo.git(&["add", "--", CRATE]);
    repo.git(&["commit", "-q", "-m", "the fence"]);
    repo.git(&["checkout", "-q", "-b", "branch"]);
    write(&root, FILE, &format!("{}{tail}", block("exit=3")));
    repo.git(&["commit", "-q", "-am", "the branch's accept"]);
    repo.git(&["checkout", "-q", "-"]);
    write(
        &root,
        FILE,
        &format!("{}{tail}pub fn mains() {{}}\n", block("exit")),
    );
    repo.git(&["commit", "-q", "-am", "main moves on"]);
    repo.git(&["merge", "-q", "--no-ff", "--no-commit", "branch"]);

    let out = xtask(&root, &["permit", "--diff"]).ok();
    assert!(!out.contains("deny "), "{out}");

    let merged = std::fs::read_to_string(root.join(FILE)).unwrap();
    let authored = block("exit=4").replace("native", "another");
    write(&root, FILE, &format!("{merged}{authored}"));
    repo.git(&["add", "--", FILE]);
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(out.contains(EXIT_AUTHORED), "{out}");
}

/// A merge displaces both parents whole (#2783): a capture fence the
/// merged side carries is that side's, though the fence's own lines, its
/// closing fence line among them, stand in the merge base too and a judge
/// of the lines the side brought read the block as broken; a fence taken
/// with the file whole is the side's as well; and a capture fence the
/// merge itself edits, a line neither parent carried, is still denied.
#[test]
fn a_capture_fence_the_merged_side_carries_is_that_sides_and_one_the_merge_edits_is_denied() {
    let root = scratch("merge-capture");
    let repo = Repo::at(&root);
    let other = "ext/ctype/src/other.rs";
    let capture = "/// ```php\n/// echo 1;\n/// ```\n/// ```expect capture\n/// 1\n/// ```\n";
    lay_down(&root, &format!("{}pub fn tail() {{}}\n", block("exit")));
    let shared = "/// ```php\n/// exit(3);\n/// ```\npub fn other() {}\n";
    write(&root, other, shared);
    repo.git(&["add", "--", CRATE]);
    repo.git(&["commit", "-q", "-m", "the base"]);
    repo.git(&["checkout", "-q", "-b", "main-side"]);
    write(
        &root,
        FILE,
        &format!(
            "{capture}pub fn captured() {{}}\n{}pub fn tail() {{}}\n",
            block("exit")
        ),
    );
    write(&root, other, &format!("{capture}{shared}"));
    repo.git(&[
        "commit",
        "-q",
        "-am",
        "main's capture, pinned by --accept --streams",
    ]);
    repo.git(&["checkout", "-q", "-"]);
    write(
        &root,
        FILE,
        &format!("{}pub fn tail() {{}}\npub fn mine() {{}}\n", block("exit")),
    );
    repo.git(&["commit", "-q", "-am", "the branch moves on"]);
    repo.git(&["merge", "-q", "--no-ff", "--no-commit", "main-side"]);

    let out = xtask(&root, &["permit", "--diff"]).ok();
    assert!(!out.contains("deny "), "{out}");

    let merged = std::fs::read_to_string(root.join(FILE)).unwrap();
    write(&root, FILE, &merged.replace("/// 1\n", "/// 2\n"));
    repo.git(&["add", "--", FILE]);
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(
        out.contains("witness.capture-authored") && out.contains(FILE),
        "{out}"
    );
}

/// A capture fence both parents inherit from the merge base stands once,
/// not once per parent (#2783): a merge that writes a second copy of it
/// adds one, and is denied.
#[test]
fn a_second_copy_of_a_fence_both_parents_inherit_is_the_merges() {
    let root = scratch("merge-capture-twice");
    let repo = Repo::at(&root);
    let capture = "/// ```php\n/// echo 1;\n/// ```\n/// ```expect capture\n/// 1\n/// ```\npub fn captured() {}\n";
    lay_down(&root, &format!("{capture}pub fn tail() {{}}\n"));
    repo.git(&["add", "--", CRATE]);
    repo.git(&["commit", "-q", "-m", "the base"]);
    repo.git(&["checkout", "-q", "-b", "other-side"]);
    write(
        &root,
        FILE,
        &format!("{capture}pub fn tail() {{}}\npub fn theirs() {{}}\n"),
    );
    repo.git(&["commit", "-q", "-am", "the other side"]);
    repo.git(&["checkout", "-q", "-"]);
    write(
        &root,
        FILE,
        &format!("pub fn mine() {{}}\n{capture}pub fn tail() {{}}\n"),
    );
    repo.git(&["commit", "-q", "-am", "this side"]);
    repo.git(&["merge", "-q", "--no-ff", "--no-commit", "other-side"]);
    xtask(&root, &["permit", "--diff"]).ok();

    let merged = std::fs::read_to_string(root.join(FILE)).unwrap();
    write(
        &root,
        FILE,
        &format!("{merged}{}", capture.replace("captured", "again")),
    );
    repo.git(&["add", "--", FILE]);
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(out.contains("witness.capture-authored"), "{out}");
}

/// The evidence travels with the commits (#1448): a verdict a branch's
/// `--accept` captured, and the reading `witness` took over the branch's
/// commit, stand on main after a merge that also moved the crate, so
/// `done` over the merge reports no `witness.exit-authored`; the same
/// reading as a line of the log, where `worktree retire` appends it,
/// serves as the scope's file does. A verdict typed by hand on another
/// branch, whose commit nothing measured, is still reported. The control:
/// a sensor that reads the scope's file alone reports the captured one,
/// whose stamp is the branch's tree and not the merge's.
#[test]
fn a_verdict_a_branch_captured_stands_on_main_after_the_merge_and_a_typed_one_does_not() {
    let root = scratch("merged-capture");
    let repo = Repo::at(&root);
    let head = || repo.git(&["rev-parse", "HEAD"]);
    let reported = |base: &str| {
        let ran = xtask(&root, &["done", "--base", base]);
        let said = ran.stdout + &ran.stderr;
        said.lines()
            .filter(|l| l.contains(EXIT_AUTHORED))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let tail = "\npub fn other() {}\n";
    lay_down(&root, &format!("{}{tail}", block("exit")));
    repo.git(&["add", "--", CRATE]);
    repo.git(&["commit", "-q", "-m", "the fence"]);
    let base = head();

    // The branch captures the verdict and `witness` measures its commit;
    // the reading reaches the store's log alone, as a retire leaves it.
    repo.git(&["checkout", "-q", "-b", "branch"]);
    write(&root, FILE, &format!("{}{tail}", block("exit=3")));
    repo.git(&["commit", "-q", "-am", "the branch's accept"]);
    witness_reading(&root);
    std::fs::remove_file(root.join("target/harness/witness/ext/ctype.toml")).unwrap();
    repo.git(&["checkout", "-q", "-"]);
    write(
        &root,
        FILE,
        &format!("{}{tail}\npub fn mains() {{}}\n", block("exit")),
    );
    repo.git(&["commit", "-q", "-am", "main moves the crate on"]);
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge the branch", "branch"]);
    let merged = std::fs::read_to_string(root.join(FILE)).unwrap();
    assert!(
        merged.contains("exit=3") && merged.contains("mains"),
        "{merged}"
    );
    assert_eq!(reported(&base), Vec::<String>::new());

    // A verdict typed by hand on a second branch, in a commit nothing
    // measured, merged the same way.
    repo.git(&["checkout", "-q", "-b", "typed"]);
    let typed = block("exit=4").replace("native", "another");
    write(&root, FILE, &format!("{merged}\n{typed}"));
    repo.git(&["commit", "-q", "-am", "a verdict typed by hand"]);
    repo.git(&["checkout", "-q", "-"]);
    repo.git(&[
        "merge",
        "-q",
        "--no-ff",
        "-m",
        "merge the typed one",
        "typed",
    ]);
    let lines = reported(&base);
    assert!(
        lines.iter().any(|l| l.contains(FILE)),
        "the typed verdict is reported: {lines:?}"
    );
}

/// The content of one of the rule's `Write` fixtures, the file whole.
fn fixture_text(name: &str) -> String {
    call(&fixtures().join("rules/witness.exit-authored").join(name))
        .tool_input
        .content
        .expect("a Write carries the whole file")
}

/// The 1-based line of the first `expect` fence that pins `status` in
/// `text`.
fn pinned_at(text: &str, status: &str) -> usize {
    text.lines()
        .position(|l| l.contains(&format!("```expect {status}")))
        .expect("the fixture pins the status")
        + 1
}

/// A verdict `witness --accept --item` wrote is captured as one a run over
/// the crate wrote: the narrowed run writes no reading of the crate, and
/// the record of the verdict it wrote ([`witness::record_accepted`]) is
/// what `permit --diff` reads it by. The control: without the record the
/// same diff is denied, as it was before the record existed, and the
/// status changed by hand on the recorded fence is denied with it.
#[test]
fn a_verdict_an_item_run_accepted_is_captured_and_one_typed_over_it_is_not() {
    let root = scratch("item-accepted");
    let repo = Repo::at(&root);
    let accepted = fixture_text("item-accepted.json");
    lay_down(&root, &accepted.replace("exit=255", "exit"));
    repo.git(&["add", "--", CRATE]);
    repo.git(&["commit", "-q", "-m", "the fence"]);
    write(&root, FILE, &accepted);
    repo.git(&["add", "--", FILE]);
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "no reading and no record: {ran}");
    assert!(ran.stdout.contains(EXIT_AUTHORED), "{ran}");

    let line = pinned_at(&accepted, "exit=255");
    witness::record_accepted(&root, CRATE, &[(Path::new(FILE), line)]).unwrap();
    let out = xtask(&root, &["permit", "--diff"]).ok();
    assert!(!out.contains("deny "), "{out}");

    write(&root, FILE, &accepted.replace("exit=255", "exit=254"));
    repo.git(&["add", "--", FILE]);
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "{ran}");
    assert!(ran.stdout.contains(EXIT_AUTHORED), "{ran}");
}

/// Each verdict is captured by whichever evidence covers it, not by one
/// kind of evidence covering every verdict of the file (#3441): the old
/// verdict stands in a commit `witness` measured, the new one in the
/// record `--accept --item` wrote, and neither kind covers both, so the
/// file is permitted only where the hits are read one by one, as
/// `Request::debug_info`'s three new fences beside the file's carried ones
/// were not on t3260 at eee22a4e0. The control, `carried-and-typed.json`:
/// the new verdict's status typed over by hand is a block no record names
/// and no commit carries, and is still denied.
#[test]
fn a_file_of_carried_verdicts_and_one_an_item_run_accepted_is_captured_and_a_typed_one_is_not() {
    let root = scratch("carried-and-accepted");
    let repo = Repo::at(&root);
    let both = fixture_text("carried-and-accepted.json");
    let carried = both.replace("exit=3", "exit");
    lay_down(&root, &carried.replace("exit=255", "exit"));
    repo.git(&["add", "--", CRATE]);
    repo.git(&["commit", "-q", "-m", "the fences"]);
    write(&root, FILE, &carried);
    repo.git(&["commit", "-q", "-am", "the crate's accept"]);
    witness_reading(&root);

    write(&root, FILE, &both);
    repo.git(&["add", "--", FILE]);
    let line = pinned_at(&both, "exit=3");
    witness::record_accepted(&root, CRATE, &[(Path::new(FILE), line)]).unwrap();
    let out = xtask(&root, &["permit", "--diff"]).ok();
    assert!(!out.contains("deny "), "{out}");

    write(&root, FILE, &fixture_text("carried-and-typed.json"));
    repo.git(&["add", "--", FILE]);
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "{ran}");
    assert!(ran.stdout.contains(EXIT_AUTHORED), "{ran}");
}

/// A captured verdict outlives an edit of the prose around it and the
/// merge that brings both to main: the branch's run over the crate
/// captured the status and was measured over the branch's commit, a
/// later commit edited only the rustdoc above the fence, and `done` over
/// the merge reads the verdict by its record, the block the record names
/// standing as `--accept` wrote it ([`witness::record_accepted`]). The
/// control: the commits alone carry the block with the prose it was
/// measured beside, which is no longer the file's, so without the record
/// the verdict is reported.
#[test]
fn a_verdict_captured_before_a_prose_edit_stands_on_main_after_the_merge() {
    let root = scratch("prose-after-accept");
    let repo = Repo::at(&root);
    let head = || repo.git(&["rev-parse", "HEAD"]);
    let reported = |base: &str| {
        let ran = xtask(&root, &["done", "--base", base]);
        let said = ran.stdout + &ran.stderr;
        said.lines()
            .filter(|l| l.contains(EXIT_AUTHORED))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let edited = fixture_text("prose-after-accept.json");
    let prose = "a fatal. The prose above the fence was edited after `--accept` wrote the\n/// status, and the verdict below it is as `--accept` wrote it.";
    assert!(edited.contains(prose), "the fixture's prose: {edited}");
    let captured = edited.replace(prose, "a fatal.");
    lay_down(&root, &captured.replace("exit=255", "exit"));
    repo.git(&["add", "--", CRATE]);
    repo.git(&["commit", "-q", "-m", "the fence"]);
    let base = head();

    repo.git(&["checkout", "-q", "-b", "branch"]);
    write(&root, FILE, &captured);
    repo.git(&["commit", "-q", "-am", "the branch's accept"]);
    witness_reading(&root);
    let line = pinned_at(&captured, "exit=255");
    write(&root, FILE, &edited);
    repo.git(&["commit", "-q", "-am", "the prose above the fence"]);
    repo.git(&["checkout", "-q", "-"]);
    write(
        &root,
        FILE,
        &format!(
            "{}pub fn mains() {{}}\n",
            captured.replace("exit=255", "exit")
        ),
    );
    repo.git(&["commit", "-q", "-am", "main moves the crate on"]);
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge the branch", "branch"]);
    let merged = std::fs::read_to_string(root.join(FILE)).unwrap();
    assert!(
        merged.contains(prose) && merged.contains("mains"),
        "{merged}"
    );
    let lines = reported(&base);
    assert!(
        lines.iter().any(|l| l.contains(FILE)),
        "the commits alone do not carry it: {lines:?}"
    );

    std::fs::write(root.join(FILE), &captured).unwrap();
    witness::record_accepted(&root, CRATE, &[(Path::new(FILE), line)]).unwrap();
    std::fs::write(root.join(FILE), &merged).unwrap();
    assert_eq!(reported(&base), Vec::<String>::new());
}

// ------------------------------------------ `where` and `unless_pattern`

/// A scratch checkout whose branch records `ticket` as `xtask worktree
/// add --ticket` writes it, or records none, as main's checkout.
fn checkout(name: &str, ticket: Option<u32>) -> Tree {
    let dir = scratch(name);
    if let Some(ticket) = ticket {
        let repo = Repo::at(&dir);
        let key = format!(
            "branch.{}.ticket",
            repo.git(&["symbolic-ref", "--short", "HEAD"])
        );
        repo.git(&["config", &key, &ticket.to_string()]);
    }
    dir
}

/// A scratch checkout with a merge in progress, as git leaves one stopped
/// before its commit: `MERGE_HEAD` in its git dir.
fn operating(name: &str) -> Tree {
    let dir = checkout(name, None);
    let git_dir = PathBuf::from(Repo::at(&dir).git(&["rev-parse", "--absolute-git-dir"]));
    std::fs::write(git_dir.join("MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();
    dir
}

/// One line that starts a second merge, cherry-pick, revert or rebase in
/// a checkout after the first is refused under `git.operation` though no
/// operation stands yet (#3261): the hook can record the first start's
/// base, `HEAD` as the line begins, and never the second's, so the second
/// would stand foreign to its own starter, its commit and its `--abort`
/// refused. The fixture is the line main's checkout ran; the control, one
/// start on a line, is admitted in the same quiet checkout, and so is a
/// second start in another checkout.
#[test]
fn a_line_that_starts_a_second_operation_in_one_checkout_is_refused() {
    let manifest = Manifest::load(workspace()).unwrap();
    let quiet = checkout("two-starts", None);
    let dir = fixtures().join("rules/git.operation");
    let judged = judge(
        &manifest,
        quiet.path(),
        &call(&dir.join("deny-two-starts.json")),
    )
    .unwrap();
    let denial = judged
        .denial()
        .unwrap_or_else(|| panic!("two starts on one line: {judged:?}"));
    assert_eq!(denial.rule, "git.operation", "{denial:?}");
    assert!(
        denial.reason().contains("one operation per line"),
        "{}",
        denial.reason()
    );

    for line in [
        "git merge --no-commit t3261w && git commit -q -F /var/empty/m.txt",
        "git merge --abort && git merge --no-commit t3370",
    ] {
        let judged = judge(&manifest, quiet.path(), &bash(line)).unwrap();
        assert!(
            judged.findings.iter().all(|f| f.rule != "git.operation"),
            "{line}: {judged:?}"
        );
    }
    let other = checkout("two-starts-other", None);
    let line = format!(
        "git merge --no-commit a && git -C {} merge --no-commit b",
        other.path().display()
    );
    let judged = judge(&manifest, quiet.path(), &bash(&line)).unwrap();
    assert!(
        judged.findings.iter().all(|f| f.rule != "git.operation"),
        "{line}: {judged:?}"
    );
}

/// A `Bash` rule denying `xtask phpt`, with its qualifiers set by `with`.
fn probe(with: impl FnOnce(&mut Rule)) -> Manifest {
    let mut manifest = Manifest::load(workspace()).unwrap();
    let mut rule = Rule::named("probe.phpt", "");
    rule.scope = OneOrMany::None;
    rule.tool = OneOrMany::One("Bash".to_owned());
    rule.predicate.deny = None;
    rule.predicate.deny_pattern = Some(r"xtask\s+phpt\b".to_owned());
    with(&mut rule);
    manifest.rules.push(rule);
    manifest
}

fn denied(manifest: &Manifest, root: &Path, line: &str) -> bool {
    let judged = judge(manifest, root, &bash(line)).unwrap();
    judged
        .findings
        .iter()
        .any(|f| f.rule == "probe.phpt" && f.kind == Kind::Deny)
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red (#3091).
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
#[path = "tracked/permit/mod.rs"]
mod tracked;
