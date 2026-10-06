//! `permit --after`, the `PostToolUse` sensor over `harness.self`'s files
//! (#625, #1516): `permit` takes their blobs before a `Bash` call, and
//! `--after` puts back any the call's line can write and denies the call
//! there: a shell redirection, `sed -i`, a program an interpreter runs from
//! a file that names the path. A file that moved while the call ran and
//! that its line cannot write, a person's edit or a binary that hides the
//! path, is kept and named (#1895). Every test runs in a scratch
//! repository holding copies of the harness's files; none writes this
//! checkout's.

use std::path::Path;
use std::process::{Command, Stdio};

use xtask::reading::{Status, Store};
use xtask::scratch::Tree;

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/rules/harness.self")
}

const SESSION: &str = "5d0c7a1e-2b4f-4e8a-9c61-0f3b8d2e7a45";

fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("permit-after-{name}")).unwrap();
    scratch_workspace(workspace(), &dir).unwrap();
    dir
}

/// The hook's stdin for one `Bash` call in `cwd`, before it or after it.
fn stdin(event: &str, session: &str, id: &str, cwd: &Path, command: &str) -> String {
    serde_json::json!({"session_id": session, "tool_use_id": id,
        "hook_event_name": event, "tool_name": "Bash", "cwd": cwd.to_str().unwrap(),
        "tool_input": {"command": command}})
    .to_string()
}

/// `xtask --root <root> permit <args>` with `input` on stdin.
fn permit(root: &Path, input: &str, args: &[&str]) -> (i32, String, String) {
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
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// The call itself, as the `Bash` tool runs it, in `root`. A commit the
/// call makes takes its identity from the environment, so the command's
/// words stay the call's and a runner with no global git identity (CI)
/// commits as any other host does.
fn shell(root: &Path, command: &str) {
    let out = Command::new("sh")
        .args(["-c", command])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=x", "-c", "user.email=x@x"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn read(root: &Path, path: &str) -> String {
    std::fs::read_to_string(root.join(path)).unwrap()
}

fn record(root: &Path, session: &str, id: &str) -> std::path::PathBuf {
    root.join("target/harness/permit/before")
        .join(session)
        .join(format!("{id}.json"))
}

/// One call through both hooks: `permit` before it, the command, then
/// `permit --after`; what `--after` printed.
fn through_the_hooks(root: &Path, id: &str, command: &str) -> (String, String) {
    through_the_hooks_while(root, id, command, || {})
}

/// [`through_the_hooks`], with `meanwhile` run while the call runs: what a
/// person's editor or another session does between the two hooks.
fn through_the_hooks_while(
    root: &Path,
    id: &str,
    command: &str,
    meanwhile: impl FnOnce(),
) -> (String, String) {
    let (code, _, err) = permit(root, &stdin("PreToolUse", SESSION, id, root, command), &[]);
    assert_eq!(code, 0, "{err}");
    assert!(record(root, SESSION, id).is_file(), "the blobs were taken");
    meanwhile();
    shell(root, command);
    let (code, out, err) = permit(
        root,
        &stdin("PostToolUse", SESSION, id, root, command),
        &["--after"],
    );
    assert_eq!(code, 0, "{err}");
    assert!(!record(root, SESSION, id).exists(), "the record is spent");
    (out, err)
}

/// The denial the hook prints and the reading the store keeps for a write
/// to `path`, and the write itself kept as the blob the denial names.
fn assert_denied(root: &Path, out: &str, path: &str, written: &str) {
    let rule = xtask::manifest::Manifest::load(root).unwrap();
    let text = &rule
        .rules
        .iter()
        .find(|r| r.id == "harness.self")
        .unwrap()
        .text;
    let hook: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| {
        panic!("{e}: {out}");
    });
    assert_eq!(hook["decision"], "block", "{out}");
    let reason = hook["reason"].as_str().unwrap();
    assert!(reason.contains("harness.self"), "{reason}");
    assert!(reason.contains(text.as_str()), "{reason}");
    assert!(reason.contains(path), "{reason}");
    let reading = Store::of_workspace(root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial is a permit reading");
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.rule, "harness.self");
    assert_eq!(reading.session, SESSION);
    assert!(reading.message.contains(path), "{}", reading.message);
    // Stamped over the file as it was put back: a snapshot taken after
    // the put-back, not one from before the call ran (#2163).
    let now = xtask::key::Snapshot::disk(root).unwrap();
    assert_eq!(reading.tree(), now.key(Path::new(path)).unwrap());
    let kept = reason
        .split("what the call wrote is blob ")
        .nth(1)
        .and_then(|rest| rest.get(..40))
        .unwrap_or_else(|| panic!("the denial names the kept blob: {reason}"));
    assert!(
        git(root, &["cat-file", "-p", kept]).contains(written),
        "the call's write is kept"
    );
}

/// The fixture under `xtask/fixtures/rules/harness.self/` is the probe
/// that must fire: a python3 program that appends to the divergence
/// registry, whose path no word of the command names.
#[test]
fn the_fixture_a_python_program_writing_the_registry_is_put_back_and_denied() {
    let root = scratch("python");
    std::fs::copy(fixture().join("append.py"), root.join("append.py")).unwrap();
    let probe: serde_json::Value = serde_json::from_str(&read(&fixture(), "after.json")).unwrap();
    let id = probe["tool_use_id"].as_str().unwrap();
    let command = probe["tool_input"]["command"].as_str().unwrap();
    assert_eq!(probe["session_id"], SESSION);
    assert!(
        !command.contains("divergence"),
        "the path is not on the line"
    );
    let was = read(&root, "docs/divergence.toml");

    let (code, _, err) = permit(
        &root,
        &stdin("PreToolUse", SESSION, id, &root, command),
        &[],
    );
    assert_eq!(code, 0, "{err}");
    shell(&root, command);
    assert_ne!(
        read(&root, "docs/divergence.toml"),
        was,
        "the program wrote"
    );
    let mut after = probe.clone();
    after["cwd"] = root.to_str().unwrap().into();
    let (code, out, err) = permit(&root, &after.to_string(), &["--after"]);
    assert_eq!(code, 0, "{err}");

    assert_eq!(read(&root, "docs/divergence.toml"), was, "put back");
    assert_denied(&root, &out, "docs/divergence.toml", "never named");
}

#[test]
fn a_shell_redirection_into_the_pin_is_put_back_and_denied() {
    let root = scratch("redirection");
    let was = read(&root, "oracle.toml");
    let (out, _) = through_the_hooks(
        &root,
        "toolu_redirect",
        "echo '# redirected' >> oracle.toml",
    );
    assert_eq!(read(&root, "oracle.toml"), was);
    assert_denied(&root, &out, "oracle.toml", "# redirected");
}

/// The note `--after` prints and the reading it keeps for a file that
/// changed while the call ran and that the call's line cannot write
/// (#1895): kept as it stands, the call not denied, the path named.
fn assert_kept(root: &Path, out: &str, path: &str) {
    let hook: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| {
        panic!("{e}: {out}");
    });
    assert!(hook.get("decision").is_none(), "nothing is denied: {out}");
    let said = hook["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(said.contains(path), "{said}");
    assert!(said.contains("its line cannot write it"), "{said}");
    let reading = Store::of_workspace(root)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the change is a permit reading");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.rule, "harness.self");
    assert_eq!(reading.session, SESSION);
    assert!(reading.message.contains(path), "{}", reading.message);
    assert!(
        reading.message.contains("its line cannot write it"),
        "{}",
        reading.message
    );
}

/// #1895: a person pastes rows into the manifest while another session's
/// call runs. The call's line names no harness file, so the change is not
/// the call's to answer for: it stands, and the reading says it moved.
#[test]
fn a_persons_edit_made_while_the_call_runs_is_kept_and_named() {
    let root = scratch("person");
    let edited = format!("{}\n# a person's rows, pasted\n", read(&root, "seams.toml"));
    let (out, _) = through_the_hooks_while(&root, "toolu_person", "git status --short", || {
        std::fs::write(root.join("seams.toml"), &edited).unwrap();
    });
    assert_eq!(
        read(&root, "seams.toml"),
        edited,
        "the person's edit stands"
    );
    assert_kept(&root, &out, "seams.toml");
}

/// A call whose line names the manifest without writing it, a reader of
/// it and a note about it, is no writer of it either: the person's edit
/// made while it runs stands.
#[test]
fn a_persons_edit_outlives_a_call_that_reads_the_file_and_writes_about_it() {
    let root = scratch("mention");
    let edited = format!("{}\n# a person's rows, pasted\n", read(&root, "seams.toml"));
    let command = "grep -c rule seams.toml; cat > note.txt <<'EOF'\nseams.toml gains a row\nEOF\n\
                   git add -- note.txt && git commit -q -F note.txt -- note.txt";
    let (out, _) = through_the_hooks_while(&root, "toolu_mention", command, || {
        std::fs::write(root.join("seams.toml"), &edited).unwrap();
    });
    assert_eq!(
        read(&root, "seams.toml"),
        edited,
        "the person's edit stands"
    );
    assert_kept(&root, &out, "seams.toml");
}

/// A call whose line writes the file is still put back, a person's edit
/// beside it or not: `sed -i` names the pin among its operands.
#[test]
fn a_sed_in_place_on_the_pin_is_put_back_and_denied() {
    let root = scratch("sed");
    let was = read(&root, "oracle.toml");
    let (out, _) = through_the_hooks(
        &root,
        "toolu_sed",
        "sed -i.bak -e '1s/^/# by sed /' oracle.toml",
    );
    assert_eq!(read(&root, "oracle.toml"), was);
    assert_denied(&root, &out, "oracle.toml", "# by sed");
}

/// A built binary, its path held inside it where no word of the line and
/// no file the line names spells it: the sensor cannot tell the binary's
/// write from a person's, so it keeps the change and says so (#1895),
/// `done`'s condition 6 being what holds the manifest at the push.
#[test]
fn a_binary_whose_line_names_no_harness_file_is_kept_and_named() {
    let root = scratch("binary");
    let source = root.join("writer.rs");
    std::fs::write(
        &source,
        "fn main() { let path = [\"seams\", \"toml\"].join(\".\"); \
         let mut text = std::fs::read_to_string(&path).unwrap(); \
         text.push_str(\"\\n# written by a binary\\n\"); \
         std::fs::write(&path, text).unwrap(); }\n",
    )
    .unwrap();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let built = Command::new(rustc)
        .arg(&source)
        .arg("-o")
        .arg(root.join("writer"))
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let (out, _) = through_the_hooks(&root, "toolu_binary", "./writer");
    assert!(read(&root, "seams.toml").ends_with("# written by a binary\n"));
    assert_kept(&root, &out, "seams.toml");
}

/// The proposal that adds one regime to the scratch's pin.
fn proposal(root: &Path) -> String {
    let added = format!(
        "{}\n[regime.after-sensor]\nini = [\"opcache.enable=0\"]\n",
        read(root, "oracle.toml")
    );
    std::fs::write(root.join("proposal.toml"), &added).unwrap();
    added
}

fn steer_command(root: &Path) -> String {
    format!(
        "{} --root {} steer oracle.toml --from proposal.toml",
        env!("CARGO_BIN_EXE_xtask"),
        root.display()
    )
}

/// `steer` is the sanctioned writer: a call that runs it and adds a row
/// is not put back, and its write stands.
#[test]
fn a_steer_call_that_adds_a_row_stands() {
    let root = scratch("steer");
    let added = proposal(&root);
    let (out, err) = through_the_hooks(&root, "toolu_steer", &steer_command(&root));
    assert_eq!(out.trim(), "", "{err}");
    assert_eq!(read(&root, "oracle.toml"), added, "steer's write stands");
    assert!(
        Store::of_workspace(&root)
            .read("permit", "harness.self")
            .unwrap()
            .is_none(),
        "nothing was denied"
    );
}

/// A write beside steer's in the same call is not steer's: the file no
/// longer carries what steer's reading says it wrote, and it is put back
/// as it stood before the call.
#[test]
fn a_write_beside_steer_in_the_same_call_is_put_back() {
    let root = scratch("steer-beside");
    let was = read(&root, "oracle.toml");
    proposal(&root);
    let command = format!("{} && echo '# beside' >> oracle.toml", steer_command(&root));
    let (out, _) = through_the_hooks(&root, "toolu_beside", &command);
    assert_eq!(read(&root, "oracle.toml"), was);
    assert_denied(&root, &out, "oracle.toml", "# beside");
}

/// `.claude/skills/**` is an ordinary file (#626): a program writing a
/// skill and the settings beside it in one call leaves the skill as it
/// wrote it, and the settings, which the rule still scopes, are put back.
#[test]
fn a_write_the_rule_exempts_stands_and_the_settings_beside_it_are_put_back() {
    let root = scratch("exempt");
    let manifest = xtask::manifest::Manifest::load(&root).unwrap();
    let rule = manifest.rule("harness.self").unwrap();
    assert!(
        rule.exempts(Path::new(".claude/skills/build/SKILL.md")),
        "the tracked row exempts the skills"
    );
    std::fs::create_dir_all(root.join(".claude/skills/build")).unwrap();
    std::fs::write(root.join(".claude/skills/build/SKILL.md"), "# build\n").unwrap();
    std::fs::write(root.join(".claude/settings.json"), "{}\n").unwrap();
    git(&root, &["add", ".claude"]);
    git(&root, &["commit", "-q", "-m", "the skill and the settings"]);
    let settings = read(&root, ".claude/settings.json");
    let (out, _) = through_the_hooks(
        &root,
        "toolu_exempt",
        "python3 -c \"[open(p, 'a').write('# written\\n') for p in ('.claude/skills/build/SKILL.md', '.claude/settings.json')]\"",
    );
    assert_eq!(
        read(&root, ".claude/skills/build/SKILL.md"),
        "# build\n# written\n",
        "the skill's write stands"
    );
    assert_eq!(read(&root, ".claude/settings.json"), settings, "put back");
    assert_denied(&root, &out, ".claude/settings.json", "# written");
    assert!(!out.contains("SKILL.md"), "the skill is not named: {out}");
}

/// A call `permit` took nothing before is never put back blindly.
#[test]
fn a_call_with_no_record_is_left_as_it_stands() {
    let root = scratch("unpaired");
    let command = "echo '# unpaired' >> oracle.toml";
    shell(&root, command);
    let written = read(&root, "oracle.toml");
    let (code, out, err) = permit(
        &root,
        &stdin("PostToolUse", SESSION, "toolu_never_seen", &root, command),
        &["--after"],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), "");
    assert!(err.contains("nothing is put back"), "{err}");
    assert_eq!(read(&root, "oracle.toml"), written);
}

/// Two sessions in one checkout keep their own records, even under one
/// tool-use id: one session's `--after` spends its own record and leaves
/// the other's.
#[test]
fn concurrent_sessions_keep_their_own_records() {
    let root = scratch("sessions");
    let other = "9a8b7c6d-0000-4000-8000-000000000001";
    let command = "echo '# one' >> oracle.toml";
    for session in [SESSION, other] {
        let (code, _, err) = permit(
            &root,
            &stdin("PreToolUse", session, "toolu_same", &root, command),
            &[],
        );
        assert_eq!(code, 0, "{err}");
    }
    shell(&root, command);
    let (_, out, _) = permit(
        &root,
        &stdin("PostToolUse", other, "toolu_same", &root, command),
        &["--after"],
    );
    assert!(out.contains("block"), "{out}");
    assert!(!record(&root, other, "toolu_same").exists());
    assert!(
        record(&root, SESSION, "toolu_same").is_file(),
        "the other session's record stands"
    );
}

/// The base is the file as it stood before the call, not as it was
/// committed: a person's edit not yet staged is what the call is put back
/// to. And an untracked file under `.claude/` is Claude Code's own state,
/// written between the hooks on a person's act, and is not touched.
#[test]
fn a_file_is_put_back_to_its_unstaged_edit_and_an_untracked_one_is_left() {
    let root = scratch("unstaged");
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    std::fs::write(root.join(".claude/settings.json"), "{}\n").unwrap();
    git(&root, &["add", ".claude/settings.json"]);
    git(&root, &["commit", "-q", "-m", "settings"]);
    let edited = format!(
        "{}\n# a person's edit, not staged\n",
        read(&root, "oracle.toml")
    );
    std::fs::write(root.join("oracle.toml"), &edited).unwrap();

    let command = "echo '# call' >> oracle.toml; echo '{\"hooks\":{}}' > .claude/settings.json; \
                   echo held > .claude/scheduled_tasks.lock";
    let (out, _) = through_the_hooks(&root, "toolu_unstaged", command);
    assert_eq!(
        read(&root, "oracle.toml"),
        edited,
        "the person's edit is the base"
    );
    assert_eq!(read(&root, ".claude/settings.json"), "{}\n");
    assert_eq!(read(&root, ".claude/scheduled_tasks.lock"), "held\n");
    assert!(out.contains(".claude/settings.json"), "{out}");
    assert!(!out.contains("scheduled_tasks"), "{out}");
}
