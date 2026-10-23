//! The person's approval at `permit` (#2247): a `harness.self` write the
//! person approved through `AskUserQuestion` in the session is admitted,
//! and every answer the model could have written is not. The cases are
//! the directories under `xtask/fixtures/approval/`, each the hook's
//! stdin of one `Edit` of the fixture's registry and the session's
//! transcript: `admit` must be admitted, each `deny-*` a forged or
//! mismatched answer that must stay denied.
#![cfg(tracked)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use xtask::manifest::Manifest;
use xtask::permit::{Call, Kind, judge};
use xtask::scratch::Tree;

mod common {
    pub mod repo;
    pub mod run;
}
use common::repo::scratch_workspace;
use common::run::Run;

const SESSION: &str = "5d0c7a1e-2b4f-4e8a-9c61-0f3b8d2e7a45";
const ASK: &str = "toolu_01AskFixtureApproval0001";
const RULE: &str = "harness.self";

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture() -> PathBuf {
    xtask::package_dir().join("fixtures/approval")
}

/// One case's stdin, its file and its transcript at their absolute paths,
/// the file under `root`.
fn stdin(case: &Path, root: &Path) -> Value {
    let mut call: Value =
        serde_json::from_str(&std::fs::read_to_string(case.join("call.json")).unwrap()).unwrap();
    let transcript = call["transcript_path"]
        .as_str()
        .map_or_else(|| format!("{SESSION}.jsonl"), str::to_owned);
    call["transcript_path"] = case.join(transcript).to_str().unwrap().into();
    call["tool_input"]["file_path"] = root.join("docs/divergence.toml").to_str().unwrap().into();
    call
}

/// Every case, judged as the command hook judges, from the transcript.
#[test]
fn the_persons_answer_admits_the_change_and_no_forged_answer_does() {
    let root = fixture();
    let manifest = Manifest::load(&root).unwrap();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("call.json").is_file())
        .collect();
    cases.sort();
    let names: Vec<_> = cases
        .iter()
        .map(|c| c.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert!(names.len() >= 14, "{names:?}");
    for (case, name) in cases.iter().zip(&names) {
        let call = Call::parse(&stdin(case, &root).to_string()).unwrap();
        let judgement = judge(&manifest, &root, &call).unwrap();
        let ours = judgement.findings.iter().find(|f| f.rule == RULE);
        let ours = ours.unwrap_or_else(|| panic!("{name}: {RULE} said nothing: {judgement:?}"));
        if name == "admit" {
            assert_eq!(ours.kind, Kind::Note, "{name}: {judgement:?}");
            assert!(judgement.denial().is_none(), "{name}: {judgement:?}");
            assert!(ours.text.contains(ASK), "{}", ours.text);
            assert!(ours.text.contains("docs/divergence.toml"), "{}", ours.text);
        } else {
            assert_eq!(
                judgement.denial().map(|f| f.rule.as_str()),
                Some(RULE),
                "{name} was admitted: {judgement:?}"
            );
        }
    }
    // The cases the deny list was asked for, so a renamed one is missed.
    for forged in [
        "deny-assistant-text",
        "deny-bash-output",
        "deny-agent-file",
        "deny-other-session",
        "deny-other-transcript",
        "deny-other-file",
        "deny-other-diff",
        "deny-subagent",
        "deny-filled-by-the-model",
        "deny-away",
        "deny-mislabelled",
    ] {
        assert!(names.iter().any(|n| n == forged), "{forged}: {names:?}");
    }
}

/// A scratch repository whose registry is the fixture's, so the case's
/// diff applies to it and the readings land in its store.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("permit-approval-{name}")).unwrap();
    scratch_workspace(workspace(), &dir);
    std::fs::copy(
        fixture().join("docs/divergence.toml"),
        dir.join("docs/divergence.toml"),
    )
    .unwrap();
    dir
}

/// `xtask --root <root> permit <args>` over `input`; the hook's stdout.
fn permit(root: &Path, input: &Value, args: &[&str]) -> Value {
    let out = Run::at(root)
        .args(["permit"])
        .args(args)
        .stdin(input.to_string())
        .go()
        .ok();
    serde_json::from_str(&out).unwrap_or(Value::Null)
}

/// The id git gives `bytes` as a blob.
fn hash_object(bytes: &[u8]) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn denied(out: &Value) -> bool {
    out["hookSpecificOutput"]["permissionDecision"] == "deny"
}

/// The entries of permit's audit record in the store of `root`.
fn entries(root: &Path) -> Vec<Value> {
    let dir = root.join("target/harness/permit/approvals");
    let Ok(listing) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    listing
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap())
        .collect()
}

/// Each adapter reads its own evidence: the command hook the transcript,
/// the function hook the records it read from the engine, `asked`. The
/// change an approval admitted is one entry of the audit record `permit`
/// writes (#2281), the approval whole, though both adapters judge the
/// call; a call denied writes none, and the hook's stdout stays in
/// Claude Code's shape.
#[test]
fn each_adapter_reads_its_own_evidence_and_an_admitted_change_is_one_audit_entry() {
    let root = scratch("adapters");
    let above = ["--above-the-command-hook"];

    let transcript = stdin(&fixture().join("admit"), &root);
    let engine = stdin(&fixture().join("engine"), &root);
    assert!(denied(&permit(&root, &transcript, &above)), "no `asked`");
    assert!(denied(&permit(&root, &engine, &[])), "`asked` is not read");
    let forged = stdin(&fixture().join("deny-other-diff"), &root);
    assert!(denied(&permit(&root, &forged, &[])));
    assert_eq!(entries(&root), Vec::<Value>::new(), "a denial is no entry");

    for (input, args) in [(&transcript, &[][..]), (&engine, &above[..])] {
        let out = permit(&root, input, args);
        assert!(!denied(&out), "{out}");
        let context = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains(ASK), "{context}");
        assert!(out.get("admitted").is_none(), "{out}");
    }

    let entries = entries(&root);
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry["ask"], ASK, "{entry}");
    assert_eq!(entry["file"], "docs/divergence.toml");
    assert_eq!(entry["answer"], "Apply this diff");
    assert_eq!(entry["notes"], "Ceiling ruled on the ticket.");
    assert_eq!(entry["options"][1]["label"], "Leave it");
    assert!(
        entry["question"]
            .as_str()
            .unwrap()
            .contains("docs/divergence.toml")
    );
    let diff = entry["diff"].as_str().unwrap();
    assert!(diff.starts_with("--- a/docs/divergence.toml\n"), "{diff}");
    assert_eq!(entry["diff_hash"], hash_object(diff.as_bytes()), "{entry}");
    let before = std::fs::read_to_string(root.join("docs/divergence.toml")).unwrap();
    let edit = &transcript["tool_input"];
    let after = before.replacen(
        edit["old_string"].as_str().unwrap(),
        edit["new_string"].as_str().unwrap(),
        1,
    );
    assert_eq!(entry["before"], hash_object(before.as_bytes()));
    assert_eq!(entry["after"], hash_object(after.as_bytes()));
    assert_eq!(entry["session"], SESSION);
    assert_eq!(entry["tool"], "Edit");
    assert_eq!(entry["call"], "toolu_01EditFixture0001");
    assert!(entry["time"].as_str().unwrap().ends_with('Z'), "{entry}");
    let head = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        entry["head"],
        String::from_utf8(head.stdout).unwrap().trim()
    );
}

/// The first live use (2026-10-07): the person's answer to a question whose
/// preview ends at its last context line with no newline, as the dialog's
/// text is cut, admits the two-line Edit it carries. The records are the
/// session's own, under `xtask/fixtures/approval-live/`.
#[test]
fn a_live_answer_whose_preview_is_cut_at_its_last_line_admits_its_edit() {
    let root = xtask::package_dir().join("fixtures/approval-live");
    let manifest = Manifest::load(&root).unwrap();
    let mut call: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("call.json")).unwrap()).unwrap();
    let session = call["session_id"].as_str().unwrap().to_owned();
    call["transcript_path"] = root
        .join(format!("{session}.jsonl"))
        .to_str()
        .unwrap()
        .into();
    let call = Call::parse(&call.to_string()).unwrap();
    let judgement = judge(&manifest, &root, &call).unwrap();
    assert!(judgement.denial().is_none(), "{judgement:?}");
    let note = judgement.findings.iter().find(|f| f.rule == RULE).unwrap();
    assert_eq!(note.kind, Kind::Note);
    assert!(
        note.text.contains("toolu_01FixtureLiveQuestion001"),
        "{}",
        note.text
    );
}
