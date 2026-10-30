//! `permit --after`, the `PostToolUse` sensor over `harness.self`'s files
//! (#625, #1516): `permit` takes their blobs before a `Bash` call, and
//! `--after` puts back any the call's line can write and denies the call
//! there: a shell redirection, `sed -i`, a program an interpreter runs from
//! a file that names the path. A file that moved while the call ran and
//! that its line cannot write, a person's edit or a binary that hides the
//! path, is kept and named (#1895). Every test runs in a scratch
//! repository holding copies of the harness's files; none writes this
//! checkout's.
#![cfg(tracked)]

use std::path::Path;
use std::process::Command;

use xtask::divergence::Registry;
use xtask::done;
use xtask::key::Snapshot;
use xtask::manifest::Manifest;
use xtask::reading::{Status, Store};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::Tracked;
use common::repo::{Repo, scratch_workspace};
use common::run::{Ran, Run};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture() -> std::path::PathBuf {
    xtask::package_dir().join("fixtures/rules/harness.self")
}

const SESSION: &str = "5d0c7a1e-2b4f-4e8a-9c61-0f3b8d2e7a45";

fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("permit-after-{name}")).unwrap();
    scratch_workspace(workspace(), &dir);
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
fn permit(root: &Path, input: &str, args: &[&str]) -> Ran {
    Run::at(root).args(["permit"]).args(args).stdin(input).go()
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
    permit(root, &stdin("PreToolUse", SESSION, id, root, command), &[]).ok();
    assert!(record(root, SESSION, id).is_file(), "the blobs were taken");
    meanwhile();
    shell(root, command);
    let ran = permit(
        root,
        &stdin("PostToolUse", SESSION, id, root, command),
        &["--after"],
    );
    assert_eq!(ran.code(), 0, "{ran}");
    let out = ran.stdout;
    let err = ran.stderr;
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
    assert_eq!(reading.key(), now.key(Path::new(path)).unwrap());
    let kept = reason
        .split("what the call wrote is blob ")
        .nth(1)
        .and_then(|rest| rest.get(..40))
        .unwrap_or_else(|| panic!("the denial names the kept blob: {reason}"));
    assert!(
        Repo::at(root)
            .git(&["cat-file", "-p", kept])
            .contains(written),
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

    permit(
        &root,
        &stdin("PreToolUse", SESSION, id, &root, command),
        &[],
    )
    .ok();
    shell(&root, command);
    assert_ne!(
        read(&root, "docs/divergence.toml"),
        was,
        "the program wrote"
    );
    let mut after = probe.clone();
    after["cwd"] = root.to_str().unwrap().into();
    let out = permit(&root, &after.to_string(), &["--after"]).ok();

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

/// A `Bash` call that exits non-zero gets `PostToolUseFailure` in place
/// of `PostToolUse` (#2762): its record is spent and its write to a
/// harness file put back all the same, and the answer names the event it
/// answers, with what was put back as context and no `decision`.
#[test]
fn a_call_that_exits_non_zero_is_settled_on_the_failure_event() {
    let root = scratch("failure");
    let was = read(&root, "oracle.toml");
    let (id, command) = (
        "toolu_failed",
        "echo '# then failed' >> oracle.toml; exit 1",
    );
    permit(
        &root,
        &stdin("PreToolUse", SESSION, id, &root, command),
        &[],
    )
    .ok();
    let ran = Command::new("sh")
        .args(["-c", command])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(!ran.success(), "the call fails");
    let out = permit(
        &root,
        &stdin("PostToolUseFailure", SESSION, id, &root, command),
        &["--after"],
    )
    .ok();
    assert!(!record(&root, SESSION, id).exists(), "the record is spent");
    assert_eq!(read(&root, "oracle.toml"), was, "put back");
    let hook: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(
        hook["hookSpecificOutput"]["hookEventName"], "PostToolUseFailure",
        "{out}"
    );
    assert!(hook.get("decision").is_none(), "{out}");
    let said = hook["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        said.contains("harness.self") && said.contains("oracle.toml"),
        "{said}"
    );
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

/// The scratch's manifest as a person leaves it once they have pasted a
/// row in: a ceiling raised, written while a call runs.
fn a_persons_rows(root: &Path) -> String {
    Tracked::of(root)
        .with_crate("php_heap", |row| row.budget += 1)
        .text()
}

/// #1895: a person pastes rows into the manifest while another session's
/// call runs. The call's line names no harness file, so the change is not
/// the call's to answer for: it stands, and the reading says it moved.
#[test]
fn a_persons_edit_made_while_the_call_runs_is_kept_and_named() {
    let root = scratch("person");
    let edited = a_persons_rows(&root);
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

/// `done`'s unmet conditions over the scratch repository's working tree,
/// measured since `HEAD`, with no divergence row to hold.
fn unmet(root: &Path) -> Vec<String> {
    done::judge(
        &Snapshot::disk(root).unwrap(),
        &Manifest::load(root).unwrap(),
        &Registry::default(),
        &Store::of_workspace(root),
        Some("HEAD"),
        &[],
    )
    .unwrap()
    .unmet
}

/// #1972: a change `--after` kept is one of `done`'s unmet conditions,
/// naming the file, the blob it was before and the call it moved under,
/// until a person commits it or restores it to `HEAD` (#1931). Once
/// settled its record is spent, so a later edit of the file is not read
/// as the change.
#[test]
fn a_kept_change_is_unmet_in_done_until_a_person_commits_or_restores_it() {
    // The pin and not the manifest: a build judges by the manifest it was
    // compiled against, and the first commit would leave it another.
    let root = scratch("kept-done");
    let pin = "oracle.toml";
    let keep = |id: &str, rows: &str| {
        let was = Repo::at(&root).git(&["rev-parse", &format!("HEAD:{pin}")]);
        let edited = format!("{}\n# {rows}\n", read(&root, pin));
        let (out, _) = through_the_hooks_while(&root, id, "git status --short", || {
            std::fs::write(root.join(pin), &edited).unwrap();
        });
        assert_kept(&root, &out, pin);
        let unmet = unmet(&root);
        assert_eq!(unmet.len(), 1, "{unmet:?}");
        for named in ["harness.self", pin, was.trim(), "git status --short"] {
            assert!(unmet[0].contains(named), "{named}: {}", unmet[0]);
        }
    };
    keep("toolu_kept_then_committed", "a person's regime");
    Repo::at(&root).git(&["commit", "-qam", "a person's regime"]);
    assert_eq!(unmet(&root), Vec::<String>::new(), "committed");
    keep("toolu_kept_then_restored", "a regime a person drops");
    Repo::at(&root).git(&["checkout", "HEAD", "--", pin]);
    assert_eq!(unmet(&root), Vec::<String>::new(), "restored");
    // An edit made outside any call is condition 6's note, not the spent
    // record's change.
    let edited = format!("{}\n# typed later\n", read(&root, pin));
    std::fs::write(root.join(pin), edited).unwrap();
    assert_eq!(unmet(&root), Vec::<String>::new(), "the record is spent");
}

/// #2995: a kept file the call left at `HEAD`'s blob, as a `git switch`
/// leaves one, is no record, so its note says `done` lists nothing.
#[test]
fn a_kept_change_back_at_head_says_done_lists_nothing() {
    let root = scratch("kept-at-head");
    let pin = "oracle.toml";
    let edited = format!("{}\n# a person's edit\n", read(&root, pin));
    std::fs::write(root.join(pin), edited).unwrap();
    let (out, _) =
        through_the_hooks_while(&root, "toolu_kept_at_head", "git status --short", || {
            Repo::at(&root).git(&["checkout", "HEAD", "--", pin]);
        });
    assert_kept(&root, &out, pin);
    assert!(out.contains("`done` lists nothing"), "{out}");
    assert!(!out.contains("lists it as unmet"), "{out}");
    assert_eq!(unmet(&root), Vec::<String>::new());
}

/// A call whose line names the manifest without writing it, a reader of
/// it and a note about it, is no writer of it either: the person's edit
/// made while it runs stands.
#[test]
fn a_persons_edit_outlives_a_call_that_reads_the_file_and_writes_about_it() {
    let root = scratch("mention");
    let edited = a_persons_rows(&root);
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
        xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")).display(),
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
    Repo::at(&root).git(&["add", ".claude"]);
    Repo::at(&root).git(&["commit", "-q", "-m", "the skill and the settings"]);
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
    let ran = permit(
        &root,
        &stdin("PostToolUse", SESSION, "toolu_never_seen", &root, command),
        &["--after"],
    );
    assert_eq!(ran.code(), 0, "{ran}");
    let out = ran.stdout;
    let err = ran.stderr;
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
        permit(
            &root,
            &stdin("PreToolUse", session, "toolu_same", &root, command),
            &[],
        )
        .ok();
    }
    shell(&root, command);
    let out = permit(
        &root,
        &stdin("PostToolUse", other, "toolu_same", &root, command),
        &["--after"],
    )
    .stdout;
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
    Repo::at(&root).git(&["add", ".claude/settings.json"]);
    Repo::at(&root).git(&["commit", "-q", "-m", "settings"]);
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

/// #2247: a `Bash` write the person approved in the session, the change
/// their `AskUserQuestion` answer carries as a diff (the `admit` case of
/// `xtask/fixtures/approval/`), stands; one that writes anything else is
/// put back as before. `--after` reads the transcript the hook names. The
/// same line with no approval is denied before it runs (#625).
#[test]
fn a_write_the_person_approved_stands_and_one_that_differs_is_put_back() {
    let approval = xtask::package_dir().join("fixtures/approval");
    let transcript = approval.join("admit").join(format!("{SESSION}.jsonl"));
    let approved = "# The divergence registry of the approval fixture (#2247).\n[[divergence]]\n\
                    id = \"fixture-1\"\nstatus = \"accepted\"\nreason = \"Ruled by a person in the session.\"\n";
    let root = scratch("unapproved");
    let line = stdin(
        "PreToolUse",
        SESSION,
        "toolu_unapproved",
        &root,
        "echo x >> docs/divergence.toml",
    );
    let out = permit(&root, &line, &[]).stdout;
    assert!(
        out.contains("\"deny\"") && out.contains("harness.self-shell"),
        "{out}"
    );
    for (id, written, stands) in [
        ("toolu_approved", approved.to_owned(), true),
        (
            "toolu_beyond",
            format!("{approved}# and one line more\n"),
            false,
        ),
    ] {
        let root = scratch(id);
        std::fs::copy(
            approval.join("docs/divergence.toml"),
            root.join("docs/divergence.toml"),
        )
        .unwrap();
        Repo::at(&root).git(&["commit", "-qam", "the fixture's registry"]);
        let was = read(&root, "docs/divergence.toml");
        let command = format!("cat > docs/divergence.toml <<'EOF'\n{written}EOF");
        let hook = |event: &str| {
            let mut call: serde_json::Value =
                serde_json::from_str(&stdin(event, SESSION, id, &root, &command)).unwrap();
            call["transcript_path"] = transcript.to_str().unwrap().into();
            call.to_string()
        };
        // Before the line runs, what it will write is unknown: the
        // approval's diff applies to the file as it stands, so
        // harness.self-shell's denial is lifted for both, and the write
        // is held to the diff after it.
        let before = permit(&root, &hook("PreToolUse"), &[]).ok();
        assert!(!before.contains("\"deny\""), "{before}");
        assert!(
            before.contains("toolu_01AskFixtureApproval0001"),
            "{before}"
        );
        // An approval pending for a line is no change yet, so no audit
        // entry is written before it runs (#2281).
        let approvals = root.join("target/harness/permit/approvals");
        let entries = || -> Vec<serde_json::Value> {
            std::fs::read_dir(&approvals).map_or_else(
                |_| Vec::new(),
                |d| {
                    d.map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
                        .map(|t| serde_json::from_str(&t).unwrap())
                        .collect()
                },
            )
        };
        assert!(entries().is_empty(), "{:?}", entries());
        shell(&root, &command);
        let out = permit(&root, &hook("PostToolUse"), &["--after"]).ok();
        if stands {
            assert_eq!(read(&root, "docs/divergence.toml"), written);
            let hook: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            assert!(hook.get("decision").is_none(), "{out}");
            let said = hook["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap();
            assert!(said.contains("toolu_01AskFixtureApproval0001"), "{said}");
            // An approved change is no kept one: it leaves no record for
            // `done` to hold (the fixture's registry is no row `done`
            // parses, so the record is read where `--after` keeps it).
            let kept = root.join("target/harness/permit/kept");
            assert!(
                std::fs::read_dir(&kept).map_or(true, |mut d| d.next().is_none()),
                "{}",
                kept.display()
            );
            let reading = Store::of_workspace(&root)
                .read("permit", "harness.self")
                .unwrap()
                .expect("the approved write is a permit reading");
            assert_eq!(reading.status, Status::Pass, "{}", reading.message);
            // What stands as approved is one entry of permit's audit
            // record, written after the line ran (#2281).
            let entries = entries();
            assert_eq!(entries.len(), 1, "{entries:?}");
            assert_eq!(entries[0]["file"], "docs/divergence.toml");
            assert_eq!(entries[0]["ask"], "toolu_01AskFixtureApproval0001");
            assert_eq!(entries[0]["tool"], "Bash");
            assert_eq!(entries[0]["call"], id);
            assert_eq!(
                entries[0]["after"],
                Repo::at(&root)
                    .git(&["hash-object", "docs/divergence.toml"])
                    .trim()
            );
        } else {
            assert_eq!(read(&root, "docs/divergence.toml"), was);
            assert_denied(&root, &out, "docs/divergence.toml", "one line more");
            assert!(entries().is_empty(), "{:?}", entries());
        }
    }
}

/// A linked worktree of the scratch repository at `root`, laid in a
/// temporary directory of its own as a ticket's is beside the checkout;
/// the tree that holds it, and its path.
fn linked_worktree(root: &Path) -> (Tree, std::path::PathBuf) {
    let base = Tree::new("permit-after-linked").unwrap();
    let linked = base.join("t1");
    Repo::at(root).git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "t1",
        linked.to_str().unwrap(),
    ]);
    let linked = linked.canonicalize().unwrap();
    (base, linked)
}

/// A merge stopped before its commit in `root`, staging `path` as
/// `content`: the marker git keeps, the file written and added.
fn merging_with(root: &Path, path: &str, content: &str) {
    let head = Repo::at(root).git(&["rev-parse", "HEAD"]);
    let git_dir = Repo::at(root).git(&["rev-parse", "--absolute-git-dir"]);
    std::fs::write(Path::new(git_dir.trim()).join("MERGE_HEAD"), head).unwrap();
    std::fs::write(root.join(path), content).unwrap();
    Repo::at(root).git(&["add", "--", path]);
}

/// #2597: a builder's call runs in its worktree, its shell starting in
/// the main checkout and moved there by a leading `cd`, while another
/// session's merge in the main checkout stages a person's approved
/// registry. `--after` holds only the checkout the call ran in, so the
/// main checkout's working tree is the merge's, byte for byte, whatever
/// the line's words name.
#[test]
fn a_call_in_a_worktree_leaves_the_main_checkout_s_merge_as_it_stands() {
    let root = scratch("linked-merge");
    let (_base, linked) = linked_worktree(&root);
    let merged = format!(
        "{}\n# a row the person approved\n",
        read(&root, "docs/divergence.toml")
    );
    let command = format!("cd {} && touch docs/divergence.toml", linked.display());
    let (out, _) = through_the_hooks_while(&root, "toolu_linked_merge", &command, || {
        merging_with(&root, "docs/divergence.toml", &merged);
    });
    assert_eq!(
        read(&root, "docs/divergence.toml"),
        merged,
        "the merge's file stands"
    );
    let status = Repo::at(&root).git(&["status", "--porcelain", "--", "docs/divergence.toml"]);
    assert_eq!(
        status, "M  docs/divergence.toml",
        "staged, and the working tree with it"
    );
    assert!(
        !out.contains("divergence.toml"),
        "the main checkout is not judged: {out}"
    );
}

/// #2597: a call in the checkout where a merge stands, whose line can
/// write the registry the merge staged, does not put it back: the
/// operation's commit carries it, and the change is kept and named.
#[test]
fn a_harness_file_a_merge_staged_is_not_put_back() {
    let root = scratch("merge-staged");
    let merged = format!(
        "{}\n# a row the person approved\n",
        read(&root, "docs/divergence.toml")
    );
    let (out, _) = through_the_hooks_while(
        &root,
        "toolu_merge_staged",
        "touch docs/divergence.toml",
        || {
            merging_with(&root, "docs/divergence.toml", &merged);
        },
    );
    assert_eq!(
        read(&root, "docs/divergence.toml"),
        merged,
        "the merge's file stands"
    );
    let status = Repo::at(&root).git(&["status", "--porcelain", "--", "docs/divergence.toml"]);
    assert_eq!(status, "M  docs/divergence.toml", "still staged");
    let hook: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert!(hook.get("decision").is_none(), "nothing is denied: {out}");
    let said = hook["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        said.contains("docs/divergence.toml") && said.contains("a merge"),
        "{said}"
    );
}

/// A commit on a ticket's branch in a linked worktree of `root` that
/// writes `path` as `content`, merged into `root` by a merge commit, as
/// the coordinator merges a ticket in the main checkout.
fn merge_committing(root: &Path, path: &str, content: &str) {
    let (_base, linked) = linked_worktree(root);
    std::fs::write(linked.join(path), content).unwrap();
    Repo::at(&linked).git(&["add", "--", path]);
    Repo::at(&linked).git(&["commit", "-qm", "a ticket's commit"]);
    Repo::at(root).git(&["merge", "-q", "--no-ff", "-m", "Merge t1", "t1"]);
}

/// #3220, the fixture: another session's merge commit lands a row in the
/// registry while a call whose line can write it runs in that checkout.
/// History carries the change, so the file stands at `HEAD`'s blob, the
/// call is not denied, and nothing is recorded: no reading, nothing
/// `done` lists.
#[test]
fn the_fixture_a_harness_file_a_merge_commit_lands_during_the_call_stands_at_head() {
    let root = scratch("landed");
    let probe: serde_json::Value =
        serde_json::from_str(&read(&fixture(), "after-landed.json")).unwrap();
    let id = probe["tool_use_id"].as_str().unwrap();
    let command = probe["tool_input"]["command"].as_str().unwrap();
    let path = "docs/divergence.toml";
    let merged = format!(
        "{}\n# a by-design row a ticket's steer landed\n",
        read(&root, path)
    );
    permit(
        &root,
        &stdin("PreToolUse", SESSION, id, &root, command),
        &[],
    )
    .ok();
    merge_committing(&root, path, &merged);
    shell(&root, command);
    let mut after = probe.clone();
    after["cwd"] = root.to_str().unwrap().into();
    let out = permit(&root, &after.to_string(), &["--after"]).ok();

    assert_eq!(read(&root, path), merged, "the merge's file stands");
    let status = Repo::at(&root).git(&["status", "--porcelain", "--", path]);
    assert_eq!(status, "", "the checkout stands at HEAD");
    let hook: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert!(hook.get("decision").is_none(), "nothing is denied: {out}");
    let said = hook["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        said.contains(path) && said.contains("history carries"),
        "{said}"
    );
    let reading = Store::of_workspace(&root)
        .read("permit", "harness.self")
        .unwrap();
    assert!(reading.is_none(), "nothing is recorded: {reading:?}");
    assert_eq!(unmet(&root), Vec::<String>::new());
}

/// #3220's other side: `HEAD` moves during the call for another file, and
/// the call's line writes the pin back to `HEAD`'s blob over a person's
/// edit. History never carried that change, so it is put back and denied.
#[test]
fn a_write_back_to_head_is_put_back_though_head_moved_for_another_file() {
    let root = scratch("restored");
    let pin = "oracle.toml";
    let edited = format!("{}\n# a person's edit\n", read(&root, pin));
    std::fs::write(root.join(pin), &edited).unwrap();
    let command = format!("git show HEAD:{pin} > {pin}");
    let (out, _) = through_the_hooks_while(&root, "toolu_restored", &command, || {
        merge_committing(&root, "another-ticket.txt", "another ticket's file\n");
    });
    assert_eq!(read(&root, pin), edited, "the person's edit is put back");
    assert_denied(&root, &out, pin, "");
}

/// #3220's third side: a call whose line writes the pin and commits it
/// moves `HEAD` itself, so the history is the call's own and credits it
/// with nothing: the write is put back and denied as before.
#[test]
fn a_write_the_call_commits_itself_is_put_back_and_denied() {
    let root = scratch("self-commit");
    let pin = "oracle.toml";
    let was = read(&root, pin);
    let command = format!("echo '# committed' >> {pin} && git commit -qm pin -- {pin}");
    let (out, _) = through_the_hooks(&root, "toolu_self_commit", &command);
    assert_eq!(read(&root, pin), was, "put back");
    assert_denied(&root, &out, pin, "# committed");
}

/// #2597: a call that writes a harness file in its own worktree, its
/// shell moved there by a leading `cd`, is put back and denied there, in
/// that checkout's store, and the main checkout is not touched.
#[test]
fn a_call_that_writes_its_own_worktree_s_pin_is_put_back_there() {
    let root = scratch("linked-write");
    let (_base, linked) = linked_worktree(&root);
    let was = read(&linked, "oracle.toml");
    let main = read(&root, "oracle.toml");
    let command = format!("cd {} && echo '# written' >> oracle.toml", linked.display());
    let (out, _) = through_the_hooks(&root, "toolu_linked_write", &command);
    assert_eq!(read(&linked, "oracle.toml"), was, "put back");
    assert_eq!(read(&root, "oracle.toml"), main);
    assert!(
        out.contains("block") && out.contains("oracle.toml"),
        "{out}"
    );
    let reading = Store::of_workspace(&linked)
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial is the worktree's reading");
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
}

/// #2597: a record an older build took holds the hook's own checkout
/// beside the one the call ran in; `--after` compares the call's alone,
/// so the main checkout's registry, moved since that record, stands.
#[test]
fn a_record_an_older_build_took_of_the_hook_s_checkout_is_not_compared() {
    let root = scratch("linked-older");
    let (_base, linked) = linked_worktree(&root);
    let command = format!("cd {} && touch docs/divergence.toml", linked.display());
    permit(
        &root,
        &stdin("PreToolUse", SESSION, "toolu_older", &root, &command),
        &[],
    )
    .ok();
    let file = record(&root, SESSION, "toolu_older");
    let mut taken: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let mut main = taken["checkouts"][0].clone();
    main["checkout"] = root.canonicalize().unwrap().to_str().unwrap().into();
    taken["checkouts"].as_array_mut().unwrap().insert(0, main);
    std::fs::write(&file, taken.to_string()).unwrap();
    let merged = format!(
        "{}\n# a row the person approved\n",
        read(&root, "docs/divergence.toml")
    );
    std::fs::write(root.join("docs/divergence.toml"), &merged).unwrap();
    shell(&root, &command);
    let out = permit(
        &root,
        &stdin("PostToolUse", SESSION, "toolu_older", &root, &command),
        &["--after"],
    )
    .ok();
    assert_eq!(read(&root, "docs/divergence.toml"), merged);
    assert!(!out.contains("divergence.toml"), "{out}");
}

/// #2607: a call that works in its worktree by a shape other than a plain
/// leading `cd`, while a person edits the main checkout's registry. The
/// line's words can name the main checkout's file where they are read
/// there, so held in the main checkout the person's edit would be put
/// back or named; held in the worktree the line works in, the main
/// checkout is neither compared nor touched.
fn a_call_in_its_worktree_leaves_the_main_checkout_alone(
    name: &str,
    shape: impl Fn(&str) -> String,
) {
    let root = scratch(name);
    let (_base, linked) = linked_worktree(&root);
    let edited = format!(
        "{}\n# a person's edit\n",
        read(&root, "docs/divergence.toml")
    );
    let command = shape(linked.to_str().unwrap());
    let (out, _) = through_the_hooks_while(&root, &format!("toolu_{name}"), &command, || {
        std::fs::write(root.join("docs/divergence.toml"), &edited).unwrap();
    });
    assert_eq!(
        read(&root, "docs/divergence.toml"),
        edited,
        "the person's edit stands: {out}"
    );
    assert!(
        !out.contains("divergence.toml"),
        "the main checkout is not judged: {out}"
    );
}

#[test]
fn a_call_by_git_c_into_its_worktree_leaves_the_main_checkout_alone() {
    a_call_in_its_worktree_leaves_the_main_checkout_alone("git-c", |wt| {
        format!("git -C {wt} checkout -- docs/divergence.toml")
    });
}

#[test]
fn a_call_by_a_cd_to_a_variable_it_assigned_leaves_the_main_checkout_alone() {
    a_call_in_its_worktree_leaves_the_main_checkout_alone("cd-variable", |wt| {
        format!("W={wt} && cd \"$W\" && git checkout -- docs/divergence.toml")
    });
}

#[test]
fn a_call_by_pushd_into_its_worktree_leaves_the_main_checkout_alone() {
    a_call_in_its_worktree_leaves_the_main_checkout_alone("pushd", |wt| {
        format!("pushd {wt} && git checkout -- docs/divergence.toml && popd")
    });
}

/// #2880, the coordinator's sequence through both hooks: a merge whose
/// sides each append a row to the registry stops on the conflict, git's
/// union of the three blobs is the proposal, `steer` writes it, and the
/// merge is staged and committed. No call is denied, steer's write is
/// steer's and stands, and the merge commits the union. The sequence runs
/// in the merge worktree, the linked one on `staging`, where the merge
/// queue makes every merge (#3622); main's checkout takes none.
#[test]
fn a_steer_call_that_lands_a_merges_union_stands_and_the_merge_commits_it() {
    let root = scratch("steer-union");
    let file = "docs/divergence.toml";
    let rows = xtask::package_dir().join("fixtures/steer/merge");
    let base = std::fs::read_to_string(rows.join("base.toml")).unwrap();
    let row = |side: &str| {
        let whole = std::fs::read_to_string(rows.join(side)).unwrap();
        whole.strip_prefix(base.as_str()).unwrap().to_owned()
    };
    // The registry both sides append to holds the fixture's base row, as
    // a person's registry holds rows; the scratch's holds none.
    std::fs::write(root.join(file), &base).unwrap();
    Repo::at(&root).commit("the base's row");
    let tracked = read(&root, file);
    let main = Repo::at(&root).git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    Repo::at(&root).git(&["checkout", "-q", "-b", "merged"]);
    std::fs::write(root.join(file), format!("{tracked}{}", row("merged.toml"))).unwrap();
    Repo::at(&root).git(&["commit", "-q", "-am", "the merged side's row"]);
    Repo::at(&root).git(&["checkout", "-q", main.trim()]);
    std::fs::write(root.join(file), format!("{tracked}{}", row("head.toml"))).unwrap();
    Repo::at(&root).git(&["commit", "-q", "-am", "HEAD's row"]);
    let union = format!("{tracked}{}{}", row("head.toml"), row("merged.toml"));
    let queue = Tree::new("permit-after-steer-union-queue").unwrap();
    let at = queue.join("_merge");
    Repo::at(&root).git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "staging",
        at.to_str().unwrap(),
        main.trim(),
    ]);

    let call = |id: &str, command: &str| {
        let out = permit(&at, &stdin("PreToolUse", SESSION, id, &at, command), &[]).ok();
        assert!(!out.contains("\"deny\""), "{command} is admitted: {out}");
        shell(&at, command);
        permit(
            &at,
            &stdin("PostToolUse", SESSION, id, &at, command),
            &["--after"],
        )
        .ok()
    };
    call("toolu_union_start", "git merge --no-commit merged || true");
    assert!(
        read(&at, file).contains("<<<<<<<"),
        "the merge stopped on the conflict"
    );
    call(
        "toolu_union_proposal",
        &format!(
            "{{ git show :2:{file}; git show :3:{file} | \
             tail -n +$(( $(git show :1:{file} | wc -l) + 1 )); }} > proposal.toml"
        ),
    );
    // HEAD's blob and what MERGE_HEAD appends past the base's lines; not
    // `git merge-file --union`, which shares the `[[divergence]]` line two
    // appended rows open with and so makes one row of two.
    assert_eq!(
        read(&at, "proposal.toml"),
        union,
        "the union is the two rows"
    );
    let steer = format!(
        "{} --root {} steer {file} --from proposal.toml",
        xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")).display(),
        at.display()
    );
    let steered = call("toolu_union_steer", &steer);
    assert_eq!(steered.trim(), "", "steer's write is steer's: {steered}");
    assert_eq!(read(&at, file), union, "steer's write stands");
    call("toolu_union_add", &format!("git add -- {file}"));
    call("toolu_union_commit", "git commit -q -m merged");
    assert_eq!(
        Repo::at(&at).git(&["show", &format!("HEAD:{file}")]),
        union.trim_end(),
        "the commit holds the union, its trailing newline aside"
    );
    assert_eq!(
        Repo::at(&at)
            .git(&["rev-list", "--parents", "-n1", "HEAD"])
            .split(' ')
            .count(),
        3,
        "the commit is the merge's"
    );
}

/// #2880: a merge's conflicted registry is guarded as any file is. A line
/// that writes the resolution itself, not through steer, has it put back
/// to the conflict's markers, and the call is denied there.
#[test]
fn a_conflicted_registry_written_beside_steer_is_put_back_to_its_markers() {
    let root = scratch("conflict-written");
    let file = "docs/divergence.toml";
    let tracked = read(&root, file);
    let main = Repo::at(&root).git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    Repo::at(&root).git(&["checkout", "-q", "-b", "merged"]);
    std::fs::write(root.join(file), format!("{tracked}\n# the merged side\n")).unwrap();
    Repo::at(&root).git(&["commit", "-q", "-am", "merged"]);
    Repo::at(&root).git(&["checkout", "-q", main.trim()]);
    std::fs::write(root.join(file), format!("{tracked}\n# HEAD's side\n")).unwrap();
    Repo::at(&root).git(&["commit", "-q", "-am", "head"]);
    shell(&root, "git merge -q merged || true");
    let conflicted = read(&root, file);
    assert!(
        conflicted.contains("<<<<<<<"),
        "the merge stopped on the conflict"
    );

    let command = format!("python3 -c \"open('{file}', 'w').write('# resolved\\n')\"");
    let (out, _) = through_the_hooks(&root, "toolu_conflict_written", &command);
    assert_eq!(read(&root, file), conflicted, "put back to the markers");
    assert_denied(&root, &out, file, "# resolved");
}
