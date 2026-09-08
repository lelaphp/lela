//! `xtask permit`, the judge at the tool call (harness-design.md §4.4).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::manifest::Manifest;
use xtask::permit::{Call, Kind, judge, judged_at_the_tool_call, scratch_workspace};
use xtask::reading::{Status, Store};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn call(path: &Path) -> Call {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Call::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// One fixture per rule permit judges: one call it fires on, one neighbour
/// it lets through (harness-design.md §4.1, §7).
#[test]
fn every_judged_rule_fires_on_its_fixture_and_not_on_its_neighbour() {
    let manifest = Manifest::load(workspace()).unwrap();
    let judged: Vec<_> = manifest
        .rules
        .iter()
        .filter(|r| judged_at_the_tool_call(r))
        .collect();
    assert!(
        judged.len() >= 8,
        "{:?}",
        judged.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    for rule in judged {
        let dir = fixtures().join("rules").join(&rule.id);
        let kind = Kind::of(rule);
        let fires = dir.join(match kind {
            Kind::Deny => "deny.json",
            Kind::Note => "note.json",
        });
        let judgement = judge(&manifest, workspace(), &call(&fires)).unwrap();
        let finding = judgement
            .findings
            .iter()
            .find(|f| f.rule == rule.id)
            .unwrap_or_else(|| {
                panic!(
                    "{}: {} fired nothing: {judgement:?}",
                    rule.id,
                    fires.display()
                )
            });
        assert_eq!(finding.kind, kind, "{}", rule.id);
        let reason = finding.reason();
        assert!(reason.contains(&rule.id), "{reason}");
        assert!(reason.contains(&rule.text), "{reason}");
        for adr in rule.adr.iter() {
            assert!(reason.contains(&format!("ADR-{adr}")), "{reason}");
        }
        if kind == Kind::Deny {
            assert_eq!(
                judgement.denial().map(|f| f.rule.as_str()),
                Some(rule.id.as_str())
            );
        }

        let judgement = judge(&manifest, workspace(), &call(&dir.join("allow.json"))).unwrap();
        assert!(
            judgement.findings.iter().all(|f| f.rule != rule.id),
            "{}: the neighbour fired: {judgement:?}",
            rule.id
        );
    }
}

/// Every rule with a tool or a scope is either judged here, with its
/// fixture, or carries one of the four predicates that read the tree after
/// the write; a fifth kind cannot fall between the two.
#[test]
fn every_rule_with_a_tool_or_a_scope_is_judged_or_is_an_after_the_write_sensor() {
    let manifest = Manifest::load(workspace()).unwrap();
    for rule in manifest
        .rules
        .iter()
        .filter(|r| !r.tool.is_empty() || !r.scope.is_empty())
    {
        let p = &rule.predicate;
        let after_the_write = p.check.is_some()
            || p.deny_lints.is_some()
            || p.avoid_from.is_some()
            || p.generated.is_some();
        assert!(
            judged_at_the_tool_call(rule) != after_the_write,
            "{}: judged {}, after the write {}",
            rule.id,
            judged_at_the_tool_call(rule),
            after_the_write
        );
    }
}

#[test]
fn a_rule_without_a_fixture_fails() {
    let manifest = Manifest::load(workspace()).unwrap();
    for rule in manifest.rules.iter().filter(|r| judged_at_the_tool_call(r)) {
        let dir = fixtures().join("rules").join(&rule.id);
        assert!(dir.join("allow.json").is_file(), "{}", dir.display());
        assert!(
            dir.join("deny.json").is_file() || dir.join("note.json").is_file(),
            "{}",
            dir.display()
        );
    }
}

#[test]
fn the_harness_files_are_denied_by_their_absolute_path() {
    let manifest = Manifest::load(workspace()).unwrap();
    for file in [
        "seams.toml",
        "docs/divergence.toml",
        "oracle.toml",
        ".claude/x/hooks.json",
    ] {
        for tool in ["Edit", "Write"] {
            let text = serde_json::json!({
                "tool_name": tool,
                "tool_input": {
                    "file_path": workspace().join(file).to_str().unwrap(),
                    "old_string": "a", "new_string": "b", "content": "b"
                }
            })
            .to_string();
            let judgement = judge(&manifest, workspace(), &Call::parse(&text).unwrap()).unwrap();
            let denial = judgement
                .denial()
                .unwrap_or_else(|| panic!("{tool} {file} allowed"));
            assert_eq!(denial.rule, "harness.self");
            assert!(
                denial.reason().contains("harness.self"),
                "{}",
                denial.reason()
            );
        }
    }
}

#[test]
fn a_file_in_an_agent_worktree_is_judged_against_that_checkout() {
    let manifest = Manifest::load(workspace()).unwrap();
    // A worktree is a checkout: it has a `.git`. This one is gitignored.
    let worktree = workspace().join(format!(
        ".claude/worktrees/agent-permit-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: nowhere\n").unwrap();
    let text = serde_json::json!({
        "tool_name": "Write",
        "cwd": worktree.to_str().unwrap(),
        "tool_input": {
            "file_path": worktree.join("crates/php_ext/src/lib.rs").to_str().unwrap(),
            "content": "pub struct Facade;\n"
        }
    })
    .to_string();
    let call = Call::parse(&text).unwrap();
    let root = call.workspace(workspace());
    assert_eq!(root, worktree.canonicalize().unwrap_or(worktree.clone()));
    assert_eq!(
        call.path(&root).unwrap(),
        Path::new("crates/php_ext/src/lib.rs")
    );
    let judgement = judge(&manifest, &root, &call).unwrap();
    assert!(
        judgement.denial().is_none(),
        "a worktree is a checkout, not .claude/** of the main one: {judgement:#?}"
    );

    // A Bash call carries no file; its cwd places it.
    let shell = Call::parse(
        &serde_json::json!({"tool_name": "Bash", "cwd": worktree.to_str().unwrap(),
            "tool_input": {"command": "cargo test -p xtask"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(shell.workspace(workspace()), root);
    // Outside any worktree, the root stands.
    let plain = Call::parse(
        &serde_json::json!({"tool_name": "Bash", "cwd": workspace().to_str().unwrap(),
            "tool_input": {"command": "ls"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        plain.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    // A directory under .claude/worktrees without a `.git` is not a
    // checkout; its files are the main one's `.claude/**`.
    let stray = workspace().join(".claude/worktrees/notes/todo.md");
    let dir = Call::parse(
        &serde_json::json!({"tool_name": "Write",
            "tool_input": {"file_path": stray.to_str().unwrap(), "content": "x"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        dir.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    std::fs::remove_dir_all(&worktree).unwrap();
}

#[test]
fn an_edit_inside_a_computed_region_body_is_denied_and_one_beside_it_allowed() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("regions");
    let inside = judge(&manifest, workspace(), &call(&dir.join("edit-body.json"))).unwrap();
    let denial = inside.denial().expect("an edit into the body is denied");
    assert_eq!(denial.rule, "computed.region-body");
    assert!(
        denial.reason().contains("computed run"),
        "{}",
        denial.reason()
    );

    let beside = judge(&manifest, workspace(), &call(&dir.join("edit-prose.json"))).unwrap();
    assert!(beside.denial().is_none(), "{beside:?}");

    let fenced = judge(&manifest, workspace(), &call(&dir.join("edit-fenced.json"))).unwrap();
    assert!(
        fenced.denial().is_none(),
        "markers inside a fence are prose: {fenced:?}"
    );

    let rewrite = judge(&manifest, workspace(), &call(&dir.join("write-body.json"))).unwrap();
    assert_eq!(
        rewrite.denial().map(|f| f.rule.as_str()),
        Some("computed.region-body"),
        "a Write that changes a body is the same edit"
    );
    let same = judge(&manifest, workspace(), &call(&dir.join("write-same.json"))).unwrap();
    assert!(same.denial().is_none(), "{same:?}");
    let opener = judge(
        &manifest,
        workspace(),
        &call(&dir.join("write-opener.json")),
    )
    .unwrap();
    assert!(
        opener.denial().is_none(),
        "a changed opener is a template change: {opener:?}"
    );
}

fn permit(root: &Path, stdin: &str, args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .arg("permit")
        .args(args)
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

/// A scratch workspace that is a git repository with the tracked manifest
/// and pin, so a denial there is stamped and stored without touching this
/// workspace's store.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-permit-{name}-{}", std::process::id()));
    scratch_workspace(workspace(), &dir).unwrap();
    dir
}

/// The stdin shapes recorded on Claude Code 2.1.263 (data/pre-day-1.md §4)
/// produce the recorded stdout shapes.
#[test]
fn the_recorded_stdin_shapes_produce_the_recorded_stdout_shapes() {
    let root = scratch("shapes");
    let dir = fixtures().join("hooks");
    let stdin = std::fs::read_to_string(dir.join("pretooluse-allow.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.trim().is_empty(),
        "an allow with nothing to say prints nothing: {out}"
    );

    let stdin = std::fs::read_to_string(dir.join("pretooluse-deny.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
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
    let (code, _, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
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
    assert_eq!(reading.tree.len(), 40);
}

#[test]
fn a_note_rides_on_an_allow_as_additional_context_and_is_a_reading() {
    let root = scratch("note");
    let stdin = std::fs::read_to_string(fixtures().join("rules/api.lock-line/note.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
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

#[test]
fn a_call_that_cannot_be_read_is_denied_not_dropped() {
    let root = scratch("garbage");
    let (code, out, _) = permit(&root, "not json", &[]);
    assert_eq!(code, 0);
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(got["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn bench_measures_p99_against_the_bar_and_writes_a_reading() {
    let root = scratch("bench");
    let (code, out, err) = permit(&root, "", &["--bench", "--runs", "6"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("p99"), "{out}");
    assert!(out.contains("100 ms"), "the bar is named: {out}");
    let reading = Store::of_workspace(&root)
        .read("permit", "bench")
        .unwrap()
        .expect("the bench is a reading");
    assert_eq!(reading.numbers["runs"], 6.into());
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

fn sh(root: &Path, args: &[&str]) {
    let out = Command::new(args[0])
        .args(&args[1..])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
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
    let (code, out, err) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 0, "nothing staged is nothing denied: {out}{err}");

    let locked = "use std::sync::Mutex;\n\npub struct Cache(Mutex<Vec<u8>>);\n";
    write(&root, "ext/pcre/src/cache.rs", locked);
    sh(&root, &["git", "add", "ext/pcre/src/cache.rs"]);
    let (code, out, _) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 1, "a denied staged file refuses the commit: {out}");
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
    let (code, out, _) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 1, "the staged content still holds the lock: {out}");
    sh(&root, &["git", "add", "ext/pcre/src/cache.rs"]);
    let (code, out, err) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 0, "{out}{err}");

    // The harness's own file: a note, since the hook cannot tell a person
    // from an agent, and the commit goes through.
    let manifest = std::fs::read_to_string(root.join("seams.toml")).unwrap();
    write(
        &root,
        "seams.toml",
        &format!("{manifest}\n# a person's note\n"),
    );
    sh(&root, &["git", "add", "seams.toml"]);
    let (code, out, err) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("note seams.toml"), "{out}");
    assert!(out.contains("harness.self"), "{out}");
}

/// The hook's stdin carries `session_id`; a reading carries it, so the
/// `permits` section can count denials per session (harness-design.md
/// §4.6, §10 rule 6).
#[test]
fn a_denial_carries_the_session_of_the_call() {
    let root = scratch("session");
    let stdin = std::fs::read_to_string(fixtures().join("hooks/pretooluse-deny.json")).unwrap();
    let (code, _, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
    let reading = Store::of_workspace(&root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial was recorded");
    assert_eq!(reading.session, "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11");

    let (code, _, err) = permit(
        &root,
        "",
        &[
            "--json",
            r#"{"tool_name":"Edit","tool_input":{"file_path":"seams.toml","old_string":"a","new_string":"b"}}"#,
        ],
    );
    assert_eq!(code, 0, "{err}");
    let log = Store::of_workspace(&root).log().unwrap();
    assert_eq!(
        log.last().unwrap().session,
        "",
        "a call without a session is recorded without one"
    );
}
