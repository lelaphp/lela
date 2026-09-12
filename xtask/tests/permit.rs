//! `xtask permit`, the judge at the hook's call (harness-design.md §4.4).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::divergence::Registry;
use xtask::manifest::{Manifest, OneOrMany, Rule};
use xtask::oracle::Promise;
use xtask::permit::{Call, Kind, judge, judged_at_the_call};
use xtask::reading::{Reading, Status, Store};
use xtask::witness::{self, Ask};

mod common;
use common::scratch_workspace;

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
        .filter(|r| judged_at_the_call(r))
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

/// Every rule with a `tool` or a `scope` is either judged here, with its
/// fixture, or carries one of the four predicates that read the tree after
/// the write, or is read at the Stop alone (`tool = "Stop"`, #32), which
/// `permit` never judges and no after-the-write sensor reads; a fourth
/// kind cannot fall between the three.
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
        if rule.tool.iter().any(|t| t == "Stop") {
            assert!(
                !judged_at_the_call(rule) && !after_the_write,
                "{}: a Stop rule is judged at the call {}, after the write {}",
                rule.id,
                judged_at_the_call(rule),
                after_the_write
            );
            continue;
        }
        assert!(
            judged_at_the_call(rule) != after_the_write,
            "{}: judged {}, after the write {}",
            rule.id,
            judged_at_the_call(rule),
            after_the_write
        );
    }
}

/// The fixture of the `xtask.lock-line` rule #55 proposes, judged under
/// the rule as its row would read, before a person commits it: the note
/// fires on a new `pub` item under `xtask/src` and not on its neighbour.
/// Once the tracked manifest carries the row, the walk above holds it and
/// this adds nothing.
#[test]
fn the_proposed_xtask_lock_line_rule_notes_its_fixture_and_not_its_neighbour() {
    let mut manifest = Manifest::load(workspace()).unwrap();
    if manifest.rule("xtask.lock-line").is_none() {
        let mut rule = Rule::named("xtask.lock-line", "xtask/src/**");
        rule.predicate.deny = None;
        rule.predicate.on = Some("pub".to_owned());
        manifest.rules.push(rule);
    }
    let dir = fixtures().join("rules/xtask.lock-line");
    let judged = judge(&manifest, workspace(), &call(&dir.join("note.json"))).unwrap();
    let finding = judged
        .findings
        .iter()
        .find(|f| f.rule == "xtask.lock-line")
        .unwrap_or_else(|| panic!("the note did not fire: {judged:?}"));
    assert_eq!(finding.kind, Kind::Note);
    assert!(
        judged.denial().is_none(),
        "a note is not a denial: {judged:?}"
    );
    let judged = judge(&manifest, workspace(), &call(&dir.join("allow.json"))).unwrap();
    assert!(
        judged.findings.iter().all(|f| f.rule != "xtask.lock-line"),
        "the neighbour fired: {judged:?}"
    );
}

/// The rule #33 proposes, `git.pathspec`, built through the constructors
/// as its row would read, so the predicate is proven before a person
/// commits the row: `require_argv = "--"` with `with = ["git add", "git
/// commit"]` denies a `git add` over the whole tree and a `git commit`
/// that names no path, with the rule's id and text in the denial, and
/// lets the pathspec forms and the hooks' own `git add -- <file>` through.
/// Once the tracked manifest carries the row, the walk above holds it and
/// this adds nothing.
#[test]
fn the_proposed_git_pathspec_rule_denies_its_fixture_and_not_its_neighbour() {
    let manifest = with_the_pathspec_rule(Manifest::load(workspace()).unwrap());
    let rule = manifest.rule("git.pathspec").unwrap();
    let dir = fixtures().join("rules/git.pathspec");
    let judged = judge(&manifest, workspace(), &call(&dir.join("deny.json"))).unwrap();
    let denial = judged
        .denial()
        .unwrap_or_else(|| panic!("the denial did not fire: {judged:?}"));
    assert_eq!(denial.rule, "git.pathspec");
    assert!(
        denial.reason().contains("git.pathspec"),
        "{}",
        denial.reason()
    );
    assert!(denial.reason().contains(&rule.text), "{}", denial.reason());
    let judged = judge(&manifest, workspace(), &call(&dir.join("allow.json"))).unwrap();
    assert!(
        judged.findings.iter().all(|f| f.rule != "git.pathspec"),
        "the neighbour fired: {judged:?}"
    );
    for line in [
        "git add .",
        "git add -u",
        "git commit --amend --no-edit",
        "git add -- .",
        "git -C /Users/x/dax-php commit -F /tmp/msg",
    ] {
        let judged = judge(&manifest, workspace(), &bash(line)).unwrap();
        assert_eq!(
            judged.denial().map(|f| f.rule.as_str()),
            Some("git.pathspec"),
            "{line}: {judged:?}"
        );
    }
    for line in [
        "git add -- AGENTS.md",
        "git commit --amend -F /tmp/msg -- xtask/src",
        "git add -A -- xtask/fixtures/rules/git.pathspec",
        "git worktree add -b t/x .claude/worktrees/t/x main",
        "cargo add serde",
    ] {
        let judged = judge(&manifest, workspace(), &bash(line)).unwrap();
        assert!(judged.denial().is_none(), "{line}: {judged:?}");
    }
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

#[test]
fn a_rule_without_a_fixture_fails() {
    let manifest = Manifest::load(workspace()).unwrap();
    for rule in manifest.rules.iter().filter(|r| judged_at_the_call(r)) {
        let dir = fixtures().join("rules").join(&rule.id);
        assert!(dir.join("allow.json").is_file(), "{}", dir.display());
        assert!(
            dir.join("deny.json").is_file() || dir.join("note.json").is_file(),
            "{}",
            dir.display()
        );
    }
}

/// The `exempt` key (#23): a rule `permit` judges does not judge a call
/// whose file `exempt` covers, under the path grammar `scope` uses. The
/// fixture is the `witness.exit-authored` edit under the fence grammar's
/// own parser, `xtask/src/witness/fence.rs`, which the tracked row denies
/// until a person carries the exempt onto it; the row is built here
/// through the constructors so the predicate is proven without it, and
/// once the manifest carries it the first half holds it as tracked.
#[test]
fn a_call_the_rules_exempt_covers_is_not_judged_by_that_rule() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/witness.exit-authored");
    let exempt = call(&dir.join("exempt.json"));
    let rule = manifest.rule("witness.exit-authored").unwrap();
    let judged = judge(&manifest, workspace(), &exempt).unwrap();
    if rule.exempts(Path::new("xtask/src/witness/fence.rs")) {
        assert!(
            judged.findings.iter().all(|f| f.rule != rule.id),
            "the tracked row exempts the parser: {judged:?}"
        );
    } else {
        assert_eq!(
            judged.denial().map(|f| f.rule.as_str()),
            Some(rule.id.as_str()),
            "the negative control: denied until the row lands: {judged:?}"
        );
    }

    let mut manifest = manifest;
    let rule = manifest
        .rules
        .iter_mut()
        .find(|r| r.id == "witness.exit-authored")
        .unwrap();
    if rule.exempt.is_empty() {
        rule.exempt = vec![
            "xtask/src/witness/**".to_owned(),
            "xtask/tests/witness.rs".to_owned(),
        ];
    }
    let judged = judge(&manifest, workspace(), &exempt).unwrap();
    assert!(
        judged
            .findings
            .iter()
            .all(|f| f.rule != "witness.exit-authored"),
        "the parser's own fence line is exempt: {judged:?}"
    );
    let judged = judge(&manifest, workspace(), &call(&dir.join("deny.json"))).unwrap();
    assert_eq!(
        judged.denial().map(|f| f.rule.as_str()),
        Some("witness.exit-authored"),
        "an extension stays denied: {judged:?}"
    );
}

/// Every predicate `permit` judges by a file reads `exempt` the same way:
/// the rule over `**` fires on a write at `crates/x.rs` and not on the
/// same write at `xtask/tests/x.rs`, which the exempt covers. `deny_argv`
/// judges a `Bash` call, which has no file, and an exempt names files, so
/// it is the one judged predicate an exempt cannot reach.
#[test]
fn every_judged_predicate_honours_exempt_under_the_scope_grammar() {
    let manifest = Manifest::load(workspace()).unwrap();
    let written = "pub fn read(p: *const u8) -> u8 {\n    unsafe { *p }\n}\nstatic M: std::sync::Mutex<u8> = todo!();\n";
    let probe = |shape: fn(&mut Rule)| {
        let mut rule = Rule::named("exempt.probe", "**");
        rule.exempt = vec!["xtask/tests/**".to_owned()];
        rule.predicate.deny = None;
        shape(&mut rule);
        rule
    };
    let rules = [
        probe(|r| r.predicate.deny = Some(true)),
        probe(|r| r.predicate.deny_pattern = Some("Mutex".to_owned())),
        probe(|r| r.predicate.deny_types = Some(vec!["std::sync::Mutex".to_owned()])),
        probe(|r| r.predicate.allow = Some(vec!["crates/php_heap/**".to_owned()])),
        probe(|r| r.predicate.on = Some("pub".to_owned())),
    ];
    for rule in rules {
        let name = rule.predicate.named().join(",");
        let mut manifest = manifest.clone();
        manifest.rules = vec![rule];
        let write = |path: &str| {
            let text = serde_json::json!({
                "tool_name": "Write",
                "tool_input": { "file_path": path, "content": written }
            })
            .to_string();
            judge(&manifest, workspace(), &Call::parse(&text).unwrap()).unwrap()
        };
        let fired = write("crates/x.rs");
        assert!(
            fired.findings.iter().any(|f| f.rule == "exempt.probe"),
            "{name}: the rule fires outside the exempt: {fired:?}"
        );
        let spared = write("xtask/tests/x.rs");
        assert!(
            spared.findings.is_empty(),
            "{name}: the rule fires inside the exempt: {spared:?}"
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

/// A worktree carries the tracked manifest, so it is a workspace of its
/// own by the one rule (`Call::workspace`, #57); it also has a `.git`, so
/// it is the checkout its readings go to. This one is gitignored.
fn lay_worktree(worktree: &Path) {
    std::fs::create_dir_all(worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: nowhere\n").unwrap();
    std::fs::copy(workspace().join("seams.toml"), worktree.join("seams.toml")).unwrap();
}

#[test]
fn a_file_in_an_agent_worktree_is_judged_against_that_checkout() {
    let manifest = Manifest::load(workspace()).unwrap();
    let worktree = workspace().join(format!(
        ".claude/worktrees/agent-permit-{}",
        std::process::id()
    ));
    lay_worktree(&worktree);
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
    // A directory under .claude/worktrees without a manifest is not a
    // workspace; its files are the main one's `.claude/**`.
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

/// A track's worktree is named by its branch, and a branch's name is a
/// path (`t-x/witness`), so the checkout is the first directory under
/// `.claude/worktrees` on the way down that has a `.git`, not the first
/// component alone (harness-design.md §5, #41).
#[test]
fn a_worktree_named_by_a_branch_with_a_slash_is_a_checkout_of_its_own() {
    let track = workspace().join(format!(".claude/worktrees/t-permit-{}", std::process::id()));
    let worktree = track.join("witness");
    lay_worktree(&worktree);
    let call = Call::parse(
        &serde_json::json!({"tool_name": "Edit", "cwd": worktree.to_str().unwrap(),
            "tool_input": {"file_path": worktree.join("seams.toml").to_str().unwrap(),
                "old_string": "a", "new_string": "b"}})
        .to_string(),
    )
    .unwrap();
    let root = call.workspace(workspace());
    assert_eq!(root, worktree.canonicalize().unwrap());
    assert_eq!(call.path(&root).unwrap(), Path::new("seams.toml"));
    std::fs::remove_dir_all(&track).unwrap();
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

/// A fixture that carries a `seams.toml` is a workspace of its own, and a
/// call into it is judged by that manifest and not the workspace's
/// (#57): under the `unsafe.homes` fixture's rule, `ext/pcre/src/ffi/` is
/// a home and `php_unit` is not, which is the fixture's own text and the
/// workspace's rule says neither. The main checkout's own files resolve
/// to the main checkout.
#[test]
fn a_fixture_with_a_manifest_of_its_own_is_judged_by_that_manifest() {
    let dir = fixtures().join("rules/unsafe.homes");
    let inside = edit_of(&dir.join("ext/pcre/src/ffi/mod.rs"));
    let root = inside.workspace(workspace());
    assert_eq!(
        root,
        dir.canonicalize().unwrap(),
        "the fixture is the workspace"
    );
    assert_eq!(
        inside.path(&root).unwrap(),
        Path::new("ext/pcre/src/ffi/mod.rs"),
        "and the path is relative to it"
    );
    let manifest = Manifest::load(&root).unwrap();
    let judged = judge(&manifest, &root, &inside).unwrap();
    assert!(
        judged.denial().is_none(),
        "a home of the fixture's manifest: {judged:#?}"
    );
    let outside = edit_of(&dir.join("crates/php_unit/src/lib.rs"));
    assert_eq!(outside.workspace(workspace()), root);
    let denial = judge(&manifest, &root, &outside).unwrap();
    let denial = denial.denial().expect("outside the fixture's homes");
    assert_eq!(denial.rule, "unsafe.homes");
    assert!(
        denial.text.contains("(fixture)"),
        "the fixture's text, not the workspace's: {}",
        denial.text
    );
    // Under the workspace's manifest no fixture source is in a home, which
    // is the freeze #57 lifts.
    let workspaces = Manifest::load(workspace()).unwrap();
    assert!(
        judge(&workspaces, workspace(), &inside)
            .unwrap()
            .denial()
            .is_some(),
        "the workspace's rule would deny the home"
    );

    // A call without a file is placed by its cwd in a checkout alone: a
    // shell standing in the fixture is judged by the checkout's manifest,
    // which carries the shell rules the fixture's does not.
    let shell = Call::parse(
        &serde_json::json!({"tool_name": "Bash", "cwd": dir.to_str().unwrap(),
            "tool_input": {"command": "git commit --no-verify -m x"}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        shell.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    assert_eq!(
        judge(&workspaces, workspace(), &shell)
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
            .as_deref(),
        Some("git.no-verify")
    );
}

/// The main checkout: a file of the workspace itself resolves to the
/// workspace, and the path is relative to it.
#[test]
fn a_file_of_the_main_checkout_is_judged_by_its_manifest() {
    let own = edit_of(&workspace().join("crates/php_heap/src/lib.rs"));
    assert_eq!(
        own.workspace(workspace()),
        workspace().canonicalize().unwrap()
    );
    assert_eq!(
        own.path(workspace()).unwrap(),
        Path::new("crates/php_heap/src/lib.rs")
    );
    let manifest = Manifest::load(workspace()).unwrap();
    assert!(
        judge(&manifest, workspace(), &own)
            .unwrap()
            .denial()
            .is_none(),
        "php_heap is a home of the workspace's manifest"
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
    let manifest = std::fs::read_to_string(fixtures().join("rules/unsafe.homes/seams.toml"))
        .unwrap()
        .replace(
            "pin = \"../../../../oracle.toml\"",
            &format!("pin = \"{}\"", workspace().join("oracle.toml").display()),
        );
    write(&root, &format!("{fixture}/seams.toml"), &manifest);
    write(
        &root,
        &format!("{fixture}/ext/pcre/Cargo.toml"),
        "[package]\nname = \"pcre\"\n",
    );
    let site = "pub fn read(p: *const u8) -> u8 {\n    unsafe { *p }\n}\n";
    write(&root, &format!("{fixture}/ext/pcre/src/ffi/mod.rs"), site);
    sh(&root, &["git", "add", "-A"]);
    let (code, out, err) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 0, "a home of the fixture's manifest: {out}{err}");

    write(
        &root,
        &format!("{fixture}/crates/php_unit/src/lib.rs"),
        site,
    );
    sh(&root, &["git", "add", "-A"]);
    let (code, out, _) = permit(&root, "", &["--diff"]);
    assert_eq!(code, 1, "outside the fixture's homes: {out}");
    assert!(
        out.contains(&format!("deny {fixture}/crates/php_unit/src/lib.rs"))
            && out.contains("(fixture)"),
        "the path is the checkout's and the text the fixture's: {out}"
    );
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
    assert_eq!(reading.tree().len(), 40);
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

/// The function-hook adapter judges a call before the command hook does
/// (harness-design.md §4.4): its denial ends the call and is recorded
/// here, while an allow goes on to the command hook, which records the
/// notes, so a note is one line of the log and not two.
#[test]
fn above_the_command_hook_a_denial_is_recorded_and_a_note_is_left_to_it() {
    let root = scratch("above");
    let dir = fixtures();
    let stdin = std::fs::read_to_string(dir.join("rules/api.lock-line/note.json")).unwrap();
    let (code, out, err) = permit(&root, &stdin, &["--above-the-command-hook"]);
    assert_eq!(code, 0, "{err}");
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
    let (code, out, err) = permit(&root, &stdin, &["--above-the-command-hook"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"deny\""), "{out}");
    let reading = store
        .read("permit", "harness.self")
        .unwrap()
        .expect("the denial ends the call here and is recorded here");
    assert_eq!(reading.status, Status::Fail);
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
        "session_id": "s", "cwd": root, "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": "ext/pcre/src/cache.rs", "content": content},
    });
    let (code, out, err) = permit(&root, &call.to_string(), &[]);
    assert_eq!(code, 0, "{err}");
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
    // The judge's cost is a reading over the harness crate, whose sources
    // the judge is; the store's key is the manifest's scope.
    let reading = Store::of_workspace(&root)
        .read("permit", "xtask")
        .unwrap()
        .expect("the bench is a reading");
    assert_eq!(reading.numbers["runs"], 6.into());
    assert_eq!(reading.tree().len(), 40, "stamped over the harness crate");
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
    let (code, out, err) = permit(&root, "", &["--bench", "--runs", "3", "--bar", "0"]);
    assert_eq!(code, 0, "a measurement's exit is not its status: {err}");
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

/// The two halves of `lanes.nightly` the phpt runner asked for (#15): the
/// flag is denied wherever it stands in the call, not only beside the
/// subcommand, and upstream's runner is denied where it is run and not
/// where it is cited. The rule denied `--subset x --full` on neither
/// count before, and denied reading the file on the second.
#[test]
fn the_nightly_lane_is_denied_by_the_flag_and_not_by_a_citation() {
    let manifest = Manifest::load(workspace()).unwrap();
    let bash = |command: &str| {
        Call::parse(
            &serde_json::json!({
                "tool_name": "Bash",
                "tool_input": { "command": command, "description": "one call" }
            })
            .to_string(),
        )
        .unwrap()
    };
    let denied = |command: &str| {
        judge(&manifest, workspace(), &bash(command))
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
    };

    for command in [
        "cargo run -q -p xtask -- phpt --full",
        "cargo run -p xtask -- phpt --subset Zend/tests --full",
        "xtask phpt --subset Zend/tests --jit force",
        "cargo run -q -p xtask -- phpt --jit exit-fuzz --subset tests",
        "php run-tests.php -p target/release/php",
        "cd php-src && ./run-tests.php ext/pcre",
    ] {
        assert_eq!(
            denied(command).as_deref(),
            Some("lanes.nightly"),
            "the lane must be denied: {command}"
        );
    }

    for command in [
        "cargo run -q -p xtask -- phpt --subset pcre",
        "cargo test -p xtask --test phpt",
        "grep -n 'EXPECTF' /Users/x/php-src-8.5.9/run-tests.php",
        "echo 'upstream (run-tests.php:1885-1889) merges the streams'",
    ] {
        assert_eq!(
            denied(command),
            None,
            "the subset and a citation are not the lane: {command}"
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
fn scratch_with_a_stale_manifest(name: &str) -> PathBuf {
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

/// The fixture is the quiet kind of stale and not the loud kind: it parses,
/// and the lane it lets through is the one the manifest on disk denies
/// (#15, #16). Nothing in the exit says so.
#[test]
fn the_stale_fixture_parses_and_lets_through_what_the_manifest_on_disk_denies() {
    let stale = Manifest::parse(&stale_manifest()).unwrap();
    let now = Manifest::load(workspace()).unwrap();
    let call = bash_call("cargo run -p xtask -- phpt --subset Zend/tests --full");
    assert_eq!(
        judge(&now, workspace(), &call)
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
            .as_deref(),
        Some("lanes.nightly"),
        "the manifest on disk denies the lane"
    );
    assert!(
        judge(&stale, workspace(), &call)
            .unwrap()
            .denial()
            .is_none(),
        "and the stale one lets it through, exiting 0 as it does"
    );
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
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_ne!(code, 0, "the build that cannot judge exits non-zero: {out}");
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
    let (code, _, err) = permit(&root, &stdin, &[]);
    assert_ne!(code, 0, "{err}");
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
    assert_eq!(reading.tree().len(), 40);
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
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_ne!(code, 0, "the build that cannot judge exits non-zero: {out}");
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
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
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
    let (code, out, err) = permit(&root, &stdin, &[]);
    assert_eq!(code, 0, "{err}");
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

/// `unsafe.homes` judges code, and the harness's own sensors carry the
/// keyword in their negative controls, which are strings and comments: the
/// audit's tests pin `unsafe_sites("unsafe { a(); }")`, and while that read
/// as a site the file could not be written to at all (#18).
#[test]
fn an_unsafe_inside_a_string_or_a_comment_is_not_a_site() {
    let manifest = Manifest::load(workspace()).unwrap();
    let denied = |content: &str| {
        let text = serde_json::json!({
            "tool_name": "Write",
            "tool_input": { "file_path": "crates/php_engine/src/executor.rs", "content": content },
        })
        .to_string();
        let call = Call::parse(&text).unwrap();
        judge(&manifest, workspace(), &call)
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "unsafe.homes")
    };

    assert!(
        denied("let x = unsafe { *p };"),
        "a site outside the four homes is still denied"
    );
    assert!(denied("pub unsafe fn go() {}"));
    for allowed in [
        r#"let s = "unsafe { a(); }";"#,
        r##"let s = r#"unsafe fn b()"#;"##,
        "// unsafe { a(); }\nlet x = 1;",
        "/* unsafe fn b() */\nlet x = 1;",
        "let c = \'x\'; // a char literal is not a lifetime\nlet s = \"unsafe {\";",
    ] {
        assert!(!denied(allowed), "{allowed:?} carries no unsafe code");
    }

    // The file the ticket found: the audit's own source, unchanged.
    let audit = std::fs::read_to_string(workspace().join("xtask/src/unsafe_audit.rs")).unwrap();
    assert!(
        !denied(&audit),
        "the audit's own negative controls are strings, not sites"
    );
}

/// An unsafe attribute, `#[unsafe(no_mangle)]`, asserts an unsafe contract
/// as a block does, and is a site for both sensors of `unsafe.homes`: the
/// rule's fixture carries it outside a home and inside one (#46).
#[test]
fn an_unsafe_attribute_is_a_site_outside_a_home_and_not_a_finding_inside_one() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/unsafe.homes");
    let fired = |name: &str| {
        judge(&manifest, workspace(), &call(&dir.join(name)))
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "unsafe.homes")
    };
    assert!(
        fired("deny-attribute.json"),
        "the attribute outside a home is denied at the edit"
    );
    assert!(
        !fired("allow-attribute.json"),
        "the attribute inside a home is allowed at the edit"
    );
}

/// The two rows #28 proposes for a person to add: a ticket closes through
/// `xtask ticket close` and by nothing else, so `gh issue close` and `gh
/// issue reopen` are denied by `deny_argv` with `with`, the shape
/// `git.no-verify` has, and the same act through `gh api` with a closing
/// or reopening state by a `deny_pattern`. Each fixture is held under the
/// row, as the tracked file carries it or as the constructors build it.
#[test]
fn the_proposed_ticket_close_rules_fire_on_their_fixtures_and_not_on_their_neighbours() {
    let manifest = with_the_ticket_close_rules(Manifest::load(workspace()).unwrap());
    for rule in ["ticket.close", "ticket.close-api"] {
        let dir = fixtures().join("rules").join(rule);
        let allowed = judge(&manifest, workspace(), &call(&dir.join("allow.json"))).unwrap();
        assert!(allowed.denial().is_none(), "{rule}: {allowed:?}");
        for fixture in [dir.join("deny.json"), dir.join("deny-reopen.json")] {
            let judged = judge(&manifest, workspace(), &call(&fixture)).unwrap();
            assert_eq!(
                judged.denial().map(|f| f.rule.as_str()),
                Some(rule),
                "{}: {judged:?}",
                fixture.display()
            );
        }
    }
    for line in [
        "gh api --method PATCH /repos/o/r/issues/28 --input /tmp/close.json",
        "gh api graphql -f query='mutation { closeIssue(input: {issueId: \"I_1\"}) { issue { id } } }'",
    ] {
        let judged = judge(&manifest, workspace(), &bash(line)).unwrap();
        assert_eq!(
            judged.denial().map(|f| f.rule.as_str()),
            Some("ticket.close-api"),
            "{line}: {judged:?}"
        );
    }
    for line in [
        "gh pr close 3",
        "cargo xtask ticket close 28",
        "gh api repos/o/r/issues?state=closed",
    ] {
        let judged = judge(&manifest, workspace(), &bash(line)).unwrap();
        assert!(judged.denial().is_none(), "{line}: {judged:?}");
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
    let mut reading = Reading::stamped_over("witness", CRATE, root, Path::new(CRATE)).unwrap();
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

/// The `unless` qualifier's sensor (#105): an `exit=` the named
/// instrument's fresh reading covers is permitted, and one no reading
/// accounts for is denied with the rule id, which is where the row stood
/// for every hit before.
#[test]
fn an_exit_a_fresh_witness_reading_covers_is_permitted_and_one_without_is_denied() {
    let root = scratch("unless");
    let manifest = Manifest::load(&root).unwrap();
    let captured = block("exit=3");
    lay_down(&root, &captured);
    let write = write_call(FILE, &captured);

    let denied = denial(&root, &manifest, &write).expect("no reading covers the line yet");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    witness_reading(&root);
    assert_eq!(
        denial(&root, &manifest, &write),
        None,
        "the instrument measured the tree the line stands in"
    );

    // The call the rule is for: a status this tree does not carry, which
    // no reading covers however fresh it is.
    let authored = block("exit=7");
    let denied = denial(&root, &manifest, &write_call(FILE, &authored))
        .expect("an authored status is not in the tree");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    // The same line in the tree with the reading taken before it: the
    // stamp is not the crate's hash now, so the line is no measurement's.
    lay_down(&root, &authored);
    let denied = denial(&root, &manifest, &write_call(FILE, &authored))
        .expect("the reading predates the line");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    // And a reading over the crate as it now stands permits it again: the
    // sensor reads the stamp and never the words of the line.
    witness_reading(&root);
    assert_eq!(denial(&root, &manifest, &write_call(FILE, &authored)), None);
}

/// The evidence this cannot read is no evidence: a crate the manifest does
/// not name has no reading to stand on, however the tree looks.
#[test]
fn an_exit_in_a_file_no_crate_row_names_is_denied() {
    let root = scratch("unless-uncrated");
    let manifest = Manifest::load(&root).unwrap();
    let text = block("exit=3");
    let file = "vendor/other/src/lib.rs";
    std::fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
    std::fs::write(root.join(file), &text).unwrap();
    let denied = denial(&root, &manifest, &write_call(file, &text)).expect("no crate, no reading");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle_is_here() -> bool {
    let pin = Manifest::load(workspace()).unwrap().pin;
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
        .is_some()
}

/// End to end (#105): `xtask witness --accept` captures the oracle's
/// status onto the fence line and writes its reading, and `xtask permit
/// --diff` over that tree permits the line it wrote. Take the reading away
/// and the same tree is denied with the rule id.
#[test]
fn what_accept_wrote_permit_diff_permits_over_the_same_tree() {
    if !oracle_is_here() {
        return;
    }
    let root = scratch("unless-accept");
    lay_down(&root, &block("exit"));
    let readings = witness::run(
        &root,
        &Manifest::load(&root).unwrap(),
        &Registry::load(&root).unwrap(),
        &Ask {
            crates: vec![CRATE.to_owned()],
            accept: true,
            staged: false,
        },
    )
    .unwrap();
    Store::of_workspace(&root).record(&readings).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join(FILE)).unwrap(),
        block("exit=3"),
        "the oracle's status is on the fence line and nothing else moved"
    );

    let add = Command::new("git")
        .args(["add", "--", CRATE])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(add.success());
    let (code, out, err) = xtask(&root, &["permit", "--diff"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(!out.contains("deny "), "{out}");

    std::fs::remove_file(Store::of_workspace(&root).path("witness", CRATE).unwrap()).unwrap();
    let (code, out, err) = xtask(&root, &["permit", "--diff"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(
        out.contains("deny ") && out.contains(EXIT_AUTHORED) && out.contains(FILE),
        "{out}"
    );
}

/// The binary over the scratch root, with no stdin.
fn xtask(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}
