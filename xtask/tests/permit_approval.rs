//! The person's approval at `permit` (#2247): a `harness.self` write the
//! person approved through `AskUserQuestion` in the session is admitted,
//! and every answer the model could have written is not. The cases are
//! the directories under `xtask/fixtures/approval/`, each the hook's
//! stdin of one `Edit` of the fixture's registry and the session's
//! transcript: `admit` must be admitted, each `deny-*` a forged or
//! mismatched answer that must stay denied.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use xtask::manifest::Manifest;
use xtask::permit::{Call, Kind, judge};
use xtask::scratch::Tree;

mod common;
use common::scratch_workspace;

const SESSION: &str = "5d0c7a1e-2b4f-4e8a-9c61-0f3b8d2e7a45";
const ASK: &str = "toolu_01AskFixtureApproval0001";
const RULE: &str = "harness.self";

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/approval")
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
    ] {
        assert!(names.iter().any(|n| n == forged), "{forged}: {names:?}");
    }
}

/// A scratch repository whose registry is the fixture's, so the case's
/// diff applies to it and the readings land in its store.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("permit-approval-{name}")).unwrap();
    scratch_workspace(workspace(), &dir).unwrap();
    std::fs::copy(
        fixture().join("docs/divergence.toml"),
        dir.join("docs/divergence.toml"),
    )
    .unwrap();
    dir
}

/// `xtask --root <root> permit <args>` over `input`; the hook's stdout.
fn permit(root: &Path, input: &Value, args: &[&str]) -> Value {
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
    std::io::Write::write_all(
        &mut child.stdin.take().unwrap(),
        input.to_string().as_bytes(),
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
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

/// Each adapter reads its own evidence: the command hook the transcript,
/// the function hook the records it read from the engine, `asked`; the
/// function hook alone hands back what admitted the call, for its audit
/// log, and the command hook's stdout stays in Claude Code's shape.
#[test]
fn each_adapter_reads_its_own_evidence_and_the_function_hook_is_told_what_admitted_it() {
    let root = scratch("adapters");
    let above = ["--above-the-command-hook"];

    let transcript = stdin(&fixture().join("admit"), &root);
    let out = permit(&root, &transcript, &[]);
    assert!(!denied(&out), "{out}");
    let context = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains(ASK), "{context}");
    assert!(out.get("admitted").is_none(), "{out}");
    assert!(denied(&permit(&root, &transcript, &above)), "no `asked`");

    let engine = stdin(&fixture().join("engine"), &root);
    assert!(denied(&permit(&root, &engine, &[])), "`asked` is not read");
    let out = permit(&root, &engine, &above);
    assert!(!denied(&out), "{out}");
    let admitted = &out["admitted"][0];
    assert_eq!(admitted["ask"], ASK, "{out}");
    assert_eq!(admitted["file"], "docs/divergence.toml");
    assert_eq!(admitted["answer"], "Apply this diff");
    assert_eq!(admitted["notes"], "Ceiling ruled on the ticket.");
    let diff = admitted["diff"].as_str().unwrap();
    assert!(diff.starts_with("--- a/docs/divergence.toml\n"), "{diff}");
    assert_eq!(admitted["diff_hash"], hash_object(diff.as_bytes()), "{out}");
    let before = std::fs::read(root.join("docs/divergence.toml")).unwrap();
    assert_eq!(admitted["before"], hash_object(&before));
    assert_eq!(admitted["options"][1]["label"], "Leave it");
}
