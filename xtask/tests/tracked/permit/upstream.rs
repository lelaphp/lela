//! `upstream/`, the folder that left the tree: an `Edit` and a
//! `Write` under it are refused by either spelling of the path, a folder
//! of that name anywhere else is let through, and so is one outside the
//! workspace, where the private project is; an agent's commit of a file a
//! shell line laid there is refused at the index.

use super::*;

const RULE: &str = "upstream.private";

/// `tool` on `file`, carrying what either tool reads.
fn touch(tool: &str, file: &Path) -> Call {
    let text = serde_json::json!({
        "tool_name": tool,
        "tool_input": {
            "file_path": file.to_str().unwrap(),
            "old_string": "a", "new_string": "b", "content": "b"
        }
    })
    .to_string();
    Call::parse(&text).unwrap()
}

/// The negative control: a file under the root's `upstream/`, named from
/// the root or by its absolute path, is refused to both tools, and the
/// denial says where the matter goes instead.
#[test]
fn a_file_under_upstream_is_refused_by_either_spelling_and_told_where_it_goes() {
    let manifest = Manifest::load(workspace()).unwrap();
    for file in [
        "upstream/php-src/draft.md",
        "./upstream/php-src/tests/draft.phpt",
        "upstream/README.md",
    ] {
        for tool in ["Edit", "Write"] {
            for spelled in [PathBuf::from(file), workspace().join(file)] {
                let judged = judge(&manifest, workspace(), &touch(tool, &spelled)).unwrap();
                let denial = judged
                    .denial()
                    .unwrap_or_else(|| panic!("{tool} {} allowed", spelled.display()));
                assert_eq!(denial.rule, RULE, "{tool} {}", spelled.display());
                let reason = denial.reason();
                assert!(reason.contains(RULE), "{reason}");
                assert!(reason.contains("private upstream project"), "{reason}");
                assert!(reason.contains("Instead:"), "{reason}");
            }
        }
    }
}

/// The scope is the root's folder and nothing else of the name: a folder
/// called `upstream` deeper in the tree, a source file of that name and a
/// file beside the folder are no finding of the rule's, and a file under
/// an `upstream/` outside the workspace, which is where the private
/// project keeps its own, is none the rule judges.
#[test]
fn a_folder_of_the_name_elsewhere_and_one_outside_the_workspace_are_let_through() {
    let manifest = Manifest::load(workspace()).unwrap();
    for file in [
        "research/data/c-to-rust/php-src-history/upstream/notes.txt",
        "xtask/src/phpt/upstream.rs",
        "docs/upstream/README.md",
        "upstream.md",
    ] {
        for tool in ["Edit", "Write"] {
            let judged = judge(&manifest, workspace(), &touch(tool, Path::new(file))).unwrap();
            assert!(
                judged.findings.iter().all(|f| f.rule != RULE),
                "{tool} {file}: {judged:?}"
            );
        }
    }
    let outside = Tree::new("permit-upstream-outside").unwrap();
    let draft = outside.join("private/upstream/php-src/draft.md");
    std::fs::create_dir_all(draft.parent().unwrap()).unwrap();
    for tool in ["Edit", "Write"] {
        let judged = judge(&manifest, workspace(), &touch(tool, &draft)).unwrap();
        assert!(judged.denial().is_none(), "{tool}: {judged:?}");
        assert!(judged.judged.iter().all(|id| id != RULE), "{judged:?}");
    }
}

/// What a shell line lays under `upstream/` no `Edit` or `Write` rule
/// sees, and the folder still does not come back into the tree by an
/// agent's hand: `permit --diff` judges each staged file as the `Write`
/// of its content, a `deny = true` rule's file in an agent's commit is
/// refused unless a steer reading covers it (harness revision 185, ruling
/// 3), and steer writes no file under `upstream/`. A file git reads as
/// binary is judged by its path alone, and refused the same. In a person's
/// commit, one from no session, each is a note.
#[test]
fn an_agent_s_commit_that_brings_upstream_back_is_refused_and_a_person_s_is_a_note() {
    let root = scratch("upstream-diff");
    let (draft, binary) = ("upstream/php-src/draft.md", "upstream/php-src/draft.bin");
    write(&root, draft, "A draft.\n");
    write(&root, binary, "\0\u{1}\0\u{2}");
    Repo::at(&root).git(&["add", "--", draft, binary]);
    let ran = diff_from(&root, Some("an-agent"));
    assert_eq!(ran.code(), 1, "{ran}");
    for path in [draft, binary] {
        assert!(ran.stdout.contains(&format!("deny {path}")), "{ran}");
    }
    assert!(ran.stdout.contains(RULE), "{ran}");
    assert!(ran.stdout.contains("private upstream project"), "{ran}");

    let ran = diff_from(&root, None);
    assert_eq!(ran.code(), 0, "a person's commit: {ran}");
    for path in [draft, binary] {
        assert!(ran.stdout.contains(&format!("note {path}")), "{ran}");
    }
    assert!(ran.stdout.contains(RULE), "{ran}");
}
