//! `pipeline.self`, what main executes or is read for when it judges,
//! builds or runs CI (#4348 ruling 3, #4458, #4482): each path of the row
//! is refused to an `Edit` and a `Write`, a crate's own `Cargo.toml` and a
//! `build.rs` are let through, an added `computed exec` or `computed
//! transcript` opener line is refused in any file and one the file already
//! carried is not, nor a `computed use` or one behind a code file's comment
//! leader; and an agent's commit that stages any of them, however it was
//! written, is refused at the index while a person's is a note. And
//! `pipeline.lock`, a third-party package added to a `Cargo.lock`, held
//! by what a change adds to the lock rather than by its path (#4458).
//!
//! The opener is spelled here with its first byte escaped, so this file
//! carries no line the row refuses and `computed run` executes nothing
//! in it; a lock's lines are built from parts, so none starts a line here.

use super::*;

const RULE: &str = "pipeline.self";

const LOCK: &str = "pipeline.lock";

/// The opener of an exec region, and the closer after it.
const OPENER: &str = "\u{3c}!-- computed exec cmd=\"sh run.sh\" inputs=run.sh name=run | do not edit; run computed -->\n\u{3c}!-- /computed -->\n";

/// An `Edit` of `file` replacing `old` with `new`, or with `old` empty a
/// `Write` of `new` there: each carries only what its tool reads, since
/// a `content` makes any call the write of it.
fn touch(tool: &str, file: &Path, old: &str, new: &str) -> Call {
    let file = file.to_str().unwrap();
    let input = match tool {
        "Write" => serde_json::json!({"file_path": file, "content": new}),
        _ => serde_json::json!({"file_path": file, "old_string": old, "new_string": new}),
    };
    let text = serde_json::json!({"tool_name": tool, "tool_input": input}).to_string();
    Call::parse(&text).unwrap()
}

/// The row's rule among `judged`'s findings, as a denial.
fn denied_by_the_row(judged: &xtask::permit::Judgement) -> bool {
    judged
        .findings
        .iter()
        .any(|f| f.rule == RULE && f.kind == Kind::Deny)
}

/// The negative control: every path the row names is refused to both
/// tools by its absolute path, a `computed.toml` in any directory among
/// them (#4482), and the denial says whose the change is; the paths beside
/// them that stay an agent's, a crate's own `Cargo.toml`, a `build.rs`
/// (#4363's third ruling comment), a file under `scripts/` outside
/// `hooks/` and `ci/`, and `Cargo.lock`, which `pipeline.lock` holds by
/// what a change adds to it (#4458), are no finding of the row's.
#[test]
fn every_path_of_the_row_is_refused_and_a_crate_s_cargo_toml_is_not() {
    let manifest = Manifest::load(workspace()).unwrap();
    for file in [
        ".github/workflows/harness.yml",
        ".github/actions/stash/action.yml",
        ".github/actionlint.yaml",
        "scripts/hooks/pre-push",
        "scripts/ci/pinned-php.sh",
        "Cargo.toml",
        "deny.toml",
        ".cargo/config.toml",
        "rust-toolchain.toml",
        "computed.toml",
        "docs/computed.toml",
    ] {
        for tool in ["Edit", "Write"] {
            let call = touch(tool, &workspace().join(file), "a", "b");
            let judged = judge(&manifest, workspace(), &call).unwrap();
            let denial = judged
                .denial()
                .unwrap_or_else(|| panic!("{tool} {file} allowed"));
            assert_eq!(denial.rule, RULE, "{tool} {file}");
            let reason = denial.reason();
            assert!(
                reason.contains(RULE) && reason.contains("a person's"),
                "{reason}"
            );
            assert!(reason.contains("Instead:"), "{reason}");
        }
    }
    for file in [
        "ext/standard/Cargo.toml",
        "xtask/Cargo.toml",
        "crates/php_engine/build.rs",
        "scripts/census.sh",
        "docs/ci/README.md",
        "docs/computed.md",
        "Cargo.lock",
    ] {
        for tool in ["Edit", "Write"] {
            let call = touch(tool, &workspace().join(file), "a", "b");
            let judged = judge(&manifest, workspace(), &call).unwrap();
            assert!(!denied_by_the_row(&judged), "{tool} {file}: {judged:?}");
        }
    }
}

/// An added `computed exec` or `computed transcript` opener line is
/// refused in any file, a new one and an existing one alike, and so is an
/// opener whose command an edit changes, since that is an added line; an
/// edit that leaves the opener a file already carries where it stands, a
/// `computed tree` opener, which runs nothing, a `computed use`, which
/// runs what a `computed.toml` defines, and an opener behind a code file's
/// comment leader, which computed reads only where a hook or a
/// `[discover]` table names the file (#4482), are no finding of the row's.
#[test]
fn an_added_exec_opener_is_refused_anywhere_and_one_a_file_carries_is_not() {
    let manifest = Manifest::load(workspace()).unwrap();
    let root = scratch("pipeline-opener");
    write(
        &root,
        "docs/notes.md",
        &format!("# Notes\n\n{OPENER}\nprose\n"),
    );
    let at = |file: &str| root.join(file);
    let refused = [
        touch(
            "Write",
            &at("docs/new.md"),
            "",
            &format!("# New\n\n{OPENER}"),
        ),
        touch(
            "Edit",
            &at("README.md"),
            "no such text",
            &format!("\n{OPENER}"),
        ),
        touch(
            "Edit",
            &at("docs/notes.md"),
            "prose",
            &format!("prose\n{OPENER}"),
        ),
        touch("Edit", &at("docs/notes.md"), "sh run.sh", "sh other.sh"),
        touch(
            "Write",
            &at("xtask/src/new.rs"),
            "",
            &format!("const T: &str = \"\n{OPENER}\";\n"),
        ),
        touch(
            "Edit",
            &at("docs/notes.md"),
            "prose",
            "prose\n\u{3c}!-- computed transcript steps=\"echo x\" volatile -->\n\u{3c}!-- /computed -->\n",
        ),
    ];
    for call in &refused {
        let judged = judge(&manifest, &root, call).unwrap();
        let denial = judged
            .denial()
            .unwrap_or_else(|| panic!("allowed: {call:?}"));
        assert_eq!(denial.rule, RULE, "{call:?}");
    }
    let tree = "\u{3c}!-- computed tree src=. name=layout -->\n\u{3c}!-- /computed -->\n";
    let let_through = [
        touch("Edit", &at("docs/notes.md"), "prose", "more prose"),
        touch(
            "Write",
            &at("docs/notes.md"),
            "",
            &format!("# Notes moved\n\n{OPENER}"),
        ),
        touch(
            "Write",
            &at("docs/tree.md"),
            "",
            &format!("# Tree\n\n{tree}"),
        ),
        touch(
            "Write",
            &at("docs/prose.md"),
            "",
            "A computed exec region runs its command.\n",
        ),
        touch(
            "Edit",
            &at("docs/notes.md"),
            "prose",
            "prose\n\u{3c}!-- computed use recipe=adrs -->\n\u{3c}!-- /computed -->\n",
        ),
        touch(
            "Write",
            &at("xtask/src/leader.rs"),
            "",
            "// computed exec cmd=true\n// /computed\n",
        ),
    ];
    for call in &let_through {
        let judged = judge(&manifest, &root, call).unwrap();
        assert!(!denied_by_the_row(&judged), "{call:?}: {judged:?}");
    }
}

/// What a shell line, `cargo` or `xtask new-ext` writes into the row's
/// paths no `Edit` or `Write` rule sees, and it still does not reach main
/// by an agent's commit (a hook is refused to a shell line at the call
/// too, by `pipeline.hooks-shell`, since a hook runs before any commit):
/// `permit --diff` judges each staged file as the
/// `Write` of its content, and a `deny = true` rule's file in an agent's
/// commit is refused unless a steer reading covers it (#3644 ruling 3),
/// which steer writes for none of these. Here each file is laid by a
/// plain write, as any program lays it. A file that carried its opener
/// at `HEAD`, edited elsewhere, is no finding, and neither is a
/// `Cargo.lock`, which leaves the row's scope (#4458). In a person's
/// commit, one from no session, each refused file is a note: a person
/// commits these from the diff on a ticket (#4458).
#[test]
fn an_agent_s_commit_of_a_pipeline_path_is_refused_however_it_was_written_and_a_person_s_is_a_note()
{
    let root = scratch("pipeline-diff");
    write(
        &root,
        "docs/kept.md",
        &format!("# Kept\n\n{OPENER}\nprose\n"),
    );
    Repo::at(&root).commit("an opener the file carries");
    let laid = [
        (".github/workflows/harness.yml", "on: push\n".to_owned()),
        (
            "scripts/hooks/pre-commit",
            "#!/usr/bin/env bash\nexit 0\n".to_owned(),
        ),
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"ext/new\"]\n".to_owned(),
        ),
        (
            "docs/computed.toml",
            "[recipe.x]\ncmd = \"true\"\n".to_owned(),
        ),
        ("docs/notes.md", format!("# Notes\n\n{OPENER}")),
    ];
    for (path, text) in &laid {
        write(&root, path, text);
    }
    write(
        &root,
        "docs/kept.md",
        &format!("# Kept\n\n{OPENER}\nmore prose\n"),
    );
    write(&root, "Cargo.lock", "version = 4\n");
    let mut paths: Vec<&str> = laid.iter().map(|(p, _)| *p).collect();
    paths.extend(["docs/kept.md", "Cargo.lock"]);
    let mut add = vec!["add", "--"];
    add.extend(&paths);
    Repo::at(&root).git(&add);

    let ran = diff_from(&root, Some("an-agent"));
    assert_eq!(ran.code(), 1, "{ran}");
    for (path, _) in &laid {
        assert!(
            ran.stdout.contains(&format!("deny {path}: ")),
            "{path}: {ran}"
        );
    }
    assert!(
        ran.stdout.contains(RULE) && ran.stdout.contains("#3644"),
        "{ran}"
    );
    assert!(!ran.stdout.contains("docs/kept.md"), "{ran}");
    assert!(!ran.stdout.contains("deny Cargo.lock"), "{ran}");

    let ran = diff_from(&root, None);
    assert_eq!(ran.code(), 0, "a person's commit: {ran}");
    for (path, _) in &laid {
        assert!(
            ran.stdout.contains(&format!("note {path}: ")),
            "{path}: {ran}"
        );
    }
    assert!(!ran.stdout.contains("docs/kept.md"), "{ran}");
}

/// `pipeline.lock` (#4458): a third-party package added to a `Cargo.lock`,
/// a new `checksum` line or a git `source`, is refused to an `Edit` and a
/// `Write`, in the workspace's lock and in any other of the workspace; an
/// edge to a package the lock holds, a removal, a workspace crate's entry,
/// which carries neither line, and a lock written whole with its packages
/// reordered are no finding of the row's, since a line the file carried,
/// kept or moved, adds nothing (#510).
#[test]
fn a_package_added_to_a_lock_is_refused_and_a_lock_that_moves_among_its_own_is_not() {
    let manifest = Manifest::load(workspace()).unwrap();
    let root = scratch("pipeline-lock");
    let lock = locked(&[package("memchr", 'a'), package("libc", 'b')]);
    write(&root, "Cargo.lock", &lock);
    let at = |file: &str| root.join(file);
    let refused = [
        touch(
            "Edit",
            &at("Cargo.lock"),
            "version = 4\n",
            &format!("version = 4\n\n{}", package("left-pad", 'c')),
        ),
        touch(
            "Edit",
            &at("Cargo.lock"),
            "version = 4\n",
            "version = 4\n\n[[package]]\nname = \"left-pad\"\nversion = \"1.0.0\"\nsource = \"git+https://example.invalid/left-pad#1a2b3c\"\n",
        ),
        touch(
            "Write",
            &at("Cargo.lock"),
            "",
            &format!("{lock}\n{}", package("left-pad", 'c')),
        ),
        touch(
            "Write",
            &at("fuzz/Cargo.lock"),
            "",
            &locked(&[package("memchr", 'a')]),
        ),
    ];
    for call in &refused {
        let judged = judge(&manifest, &root, call).unwrap();
        let denial = judged
            .denial()
            .unwrap_or_else(|| panic!("allowed: {call:?}"));
        assert_eq!(denial.rule, LOCK, "{call:?}");
        assert!(denial.reason().contains("Instead:"), "{judged:?}");
    }
    let let_through = [
        touch(
            "Edit",
            &at("Cargo.lock"),
            " \"memchr\",\n",
            " \"libc\",\n \"memchr\",\n",
        ),
        touch("Edit", &at("Cargo.lock"), &package("libc", 'b'), ""),
        touch(
            "Edit",
            &at("Cargo.lock"),
            "version = 4\n",
            "version = 4\n\n[[package]]\nname = \"php_new\"\nversion = \"0.0.0\"\n",
        ),
        touch(
            "Write",
            &at("Cargo.lock"),
            "",
            &locked(&[package("libc", 'b'), package("memchr", 'a')]),
        ),
    ];
    for call in &let_through {
        let judged = judge(&manifest, &root, call).unwrap();
        assert!(
            judged.findings.iter().all(|f| f.rule != LOCK),
            "{call:?}: {judged:?}"
        );
    }
}

/// A `Cargo.lock` outside the workspace is no lock of its own (#4679): an
/// `Edit` and a `Write` there that add a registry package are no finding
/// of `pipeline.lock`'s, since no row's `adding` reaches a file outside
/// the workspace, as `Call::path` says; the same text written to the
/// workspace's own lock is refused, the control.
#[test]
fn a_package_added_to_a_lock_outside_the_workspace_is_no_finding_of_the_row() {
    let manifest = Manifest::load(workspace()).unwrap();
    let root = scratch("pipeline-lock-inside");
    let elsewhere = Tree::new("permit-pipeline-lock-outside").unwrap();
    let lock = locked(&[package("memchr", 'a')]);
    write(&root, "Cargo.lock", &lock);
    write(&elsewhere, "Cargo.lock", &lock);
    let added = format!("version = 4\n\n{}", package("left-pad", 'c'));
    let edit = |dir: &Path| touch("Edit", &dir.join("Cargo.lock"), "version = 4\n", &added);
    let whole = |dir: &Path| {
        let text = format!("{lock}\n{}", package("left-pad", 'c'));
        touch("Write", &dir.join("Cargo.lock"), "", &text)
    };
    for call in [edit(&elsewhere), whole(&elsewhere)] {
        let judged = judge(&manifest, &root, &call).unwrap();
        assert!(
            judged.findings.iter().all(|f| f.rule != LOCK),
            "{call:?}: {judged:?}"
        );
        assert!(!denied_by_the_row(&judged), "{call:?}: {judged:?}");
    }
    for call in [edit(&root), whole(&root)] {
        let judged = judge(&manifest, &root, &call).unwrap();
        let denial = judged
            .denial()
            .unwrap_or_else(|| panic!("allowed: {call:?}"));
        assert_eq!(denial.rule, LOCK, "{call:?}");
    }
}

/// A `Cargo.lock` that `cargo` rewrote is held at the commit as an
/// `Edit` is at the call: an agent's commit staging a lock that gained a
/// package is refused with the row's id and #3644, a person's is a note,
/// and one that only moved among its own packages, here dropping one and
/// reordering the rest, is no finding.
#[test]
fn an_agent_s_commit_of_a_lock_that_gained_a_package_is_refused_and_one_that_moved_is_not() {
    let root = scratch("pipeline-lock-diff");
    let held = [
        package("memchr", 'a'),
        package("libc", 'b'),
        package("cc", 'd'),
    ];
    write(&root, "Cargo.lock", &locked(&held));
    Repo::at(&root).commit("a lock");
    write(
        &root,
        "Cargo.lock",
        &locked(&[package("left-pad", 'c'), held[0].clone(), held[1].clone()]),
    );
    Repo::at(&root).git(&["add", "--", "Cargo.lock"]);
    let ran = diff_from(&root, Some("an-agent"));
    assert_eq!(ran.code(), 1, "{ran}");
    assert!(
        ran.stdout.contains("deny Cargo.lock: ")
            && ran.stdout.contains(LOCK)
            && ran.stdout.contains("#3644"),
        "{ran}"
    );
    let ran = diff_from(&root, None);
    assert_eq!(ran.code(), 0, "a person's commit: {ran}");
    assert!(ran.stdout.contains("note Cargo.lock: "), "{ran}");

    write(
        &root,
        "Cargo.lock",
        &locked(&[held[1].clone(), held[0].clone()]),
    );
    Repo::at(&root).git(&["add", "--", "Cargo.lock"]);
    assert!(
        Repo::at(&root)
            .git(&["diff", "--cached", "--name-only"])
            .contains("Cargo.lock"),
        "the moved lock is staged"
    );
    let ran = diff_from(&root, Some("an-agent"));
    assert!(!ran.stdout.contains("Cargo.lock"), "{ran}");
}

/// A staged path is the checkout's, whatever the work tree holds there
/// (#4679): a staged `Cargo.lock` that gained a package, its work tree's
/// file replaced by a link to a file outside the workspace, is refused at
/// the commit as the staged lock is without the link, since no commit
/// carries a file outside the repository.
#[test]
fn a_staged_lock_that_gained_a_package_is_refused_with_its_file_a_link_outside_the_workspace() {
    let root = scratch("pipeline-lock-linked");
    let elsewhere = Tree::new("permit-pipeline-lock-linked-outside").unwrap();
    let held = [package("memchr", 'a'), package("libc", 'b')];
    write(&root, "Cargo.lock", &locked(&held));
    Repo::at(&root).commit("a lock");
    let gained = locked(&[package("left-pad", 'c'), held[0].clone(), held[1].clone()]);
    write(&root, "Cargo.lock", &gained);
    Repo::at(&root).git(&["add", "--", "Cargo.lock"]);
    write(&elsewhere, "Cargo.lock", &gained);
    std::fs::remove_file(root.join("Cargo.lock")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    let ran = diff_from(&root, Some("an-agent"));
    assert_eq!(ran.code(), 1, "{ran}");
    assert!(
        ran.stdout.contains("deny Cargo.lock: ") && ran.stdout.contains(LOCK),
        "{ran}"
    );
}

/// A registry package's entry in a lock, its checksum `sum` repeated.
fn package(name: &str, sum: char) -> String {
    let checksum = sum.to_string().repeat(64);
    let source = "registry+https://github.com/rust-lang/crates.io-index";
    format!(
        "[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\nsource = \"{source}\"\nchecksum = \"{checksum}\"\n"
    )
}

/// A lock of `packages` and one workspace crate that depends on the first.
fn locked(packages: &[String]) -> String {
    let crate_ =
        "[[package]]\nname = \"php_ext\"\nversion = \"0.0.0\"\ndependencies = [\n \"memchr\",\n]\n";
    format!("version = 4\n\n{}\n{crate_}", packages.join("\n"))
}
