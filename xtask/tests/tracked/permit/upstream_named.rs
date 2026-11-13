//! `upstream.named`, the row #4618 proposes: an `Edit` or a `Write` that
//! adds a name of a file or folder under the private folder is refused, as
//! the export's upstream bar reads a name (#4594, #4595, #4598). The row's
//! `adding` and the bar's `cutlib.NAMES_ONE` are one idea in two regex
//! dialects, so both are held to one list of forms, `forms.tsv` beside the
//! row's fixtures, which `scripts/export/test_export.py` reads too.
//!
//! The row is read from `seams.toml` once a person has settled it there,
//! and until then from `proposal.toml` beside its fixtures, the whole file
//! `cargo xtask steer seams.toml --from` lands; so the tests hold the row
//! in both states, and the walk over the tracked rules takes its fixtures
//! once it is tracked. No name is spelled in this file, which the row does
//! not exempt: every form is read from the list.

use super::*;

const RULE: &str = "upstream.named";

/// The row's fixtures, its list of forms and its proposal.
fn dir() -> PathBuf {
    fixtures().join("rules").join(RULE)
}

/// The text of the manifest that carries the row: `seams.toml` where a
/// person has landed it, and the proposal beside its fixtures until then.
fn carrying() -> String {
    let tracked = std::fs::read_to_string(workspace().join("seams.toml")).unwrap();
    let landed = Manifest::parse(&tracked).unwrap().rule(RULE).is_some();
    if landed {
        tracked
    } else {
        std::fs::read_to_string(dir().join("proposal.toml")).unwrap()
    }
}

/// The tracked manifest with the row: as it carries it, or with the
/// proposal's row after every other, so a write under the folder is still
/// `upstream.private`'s denial first, as the proposal places it.
fn with_the_row() -> Manifest {
    let mut manifest = Manifest::load(workspace()).unwrap();
    if manifest.rule(RULE).is_none() {
        let proposed = Manifest::parse(&carrying()).unwrap();
        let row = proposed.rule(RULE).cloned();
        manifest
            .rules
            .push(row.expect("the proposal carries the row"));
    }
    manifest
}

/// One row of `forms.tsv`.
struct Listed {
    list: String,
    name: String,
    form: String,
}

/// Every row of `forms.tsv`, `#` starting a comment line, as
/// `cutlib.rows` reads it.
fn forms() -> Vec<Listed> {
    let text = std::fs::read_to_string(dir().join("forms.tsv")).unwrap();
    let rows = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'));
    rows.map(|line| match line.split('\t').collect::<Vec<_>>()[..] {
        [list, name, form] => Listed {
            list: list.to_owned(),
            name: name.to_owned(),
            form: form.to_owned(),
        },
        _ => panic!("forms.tsv: not three fields: {line:?}"),
    })
    .collect()
}

/// The `[[rule]]` block of `id` in `text`, as the export's test reads it.
fn block<'a>(text: &'a str, id: &str) -> &'a str {
    let id = format!("id = \"{id}\"");
    text.split("[[rule]]")
        .find(|block| block.contains(&id))
        .unwrap_or_else(|| panic!("no row {id}"))
}

/// A `Write` of `content` at `file`, or an `Edit` appending it.
fn touch(tool: &str, file: &Path, content: &str) -> Call {
    let file = file.to_str().unwrap();
    let input = match tool {
        "Write" => serde_json::json!({"file_path": file, "content": content}),
        _ => serde_json::json!({"file_path": file, "old_string": "", "new_string": content}),
    };
    let text = serde_json::json!({"tool_name": tool, "tool_input": input}).to_string();
    Call::parse(&text).unwrap()
}

/// Whether the row denies the write; a finding of the row's that is not
/// a denial fails the test.
fn denied(manifest: &Manifest, root: &Path, call: &Call) -> bool {
    let judged = judge(manifest, root, call).unwrap();
    let found = judged.findings.iter().find(|f| f.rule == RULE);
    if let Some(found) = found {
        assert_eq!(found.kind, Kind::Deny, "{judged:?}");
        assert_eq!(judged.denial().map(|f| f.rule.as_str()), Some(RULE));
    }
    found.is_some()
}

/// The list, form by form, under the row: a form the bar refuses is
/// refused to both tools, in a file the row does not exempt, and one the
/// bar passes is no finding of the row's; each manifest row the list
/// names, whole, passes. The export's suite holds the bar to the same list.
#[test]
fn the_row_refuses_each_form_the_bar_refuses_and_passes_the_rest() {
    let manifest = with_the_row();
    let file = Path::new("docs/a-made-up-note.md");
    let text = carrying();
    let forms = forms();
    for list in ["names-one", "names-none", "row", "stays-refused"] {
        assert!(forms.iter().any(|f| f.list == list), "forms.tsv: no {list}");
    }
    for Listed { list, name, form } in &forms {
        let (content, refused) = match list.as_str() {
            "names-one" | "stays-refused" => (format!("see {form} for it\n"), true),
            "names-none" => (format!("see {form} for it\n"), false),
            "row" => (block(&text, form).to_owned(), false),
            _ => panic!("forms.tsv: {name}: no list {list}"),
        };
        for tool in ["Edit", "Write"] {
            let call = touch(tool, file, &content);
            assert_eq!(
                denied(&manifest, workspace(), &call),
                refused,
                "{list} {name}, {tool}"
            );
        }
    }
}

/// The row's own words: the denial carries its id, its text and its
/// remedy, and the row's `adding` is the pattern the proposal or the
/// tracked file writes, a regex `permit`'s dialect reads.
#[test]
fn the_denial_says_what_is_refused_and_to_describe_the_thing_without_the_path() {
    let manifest = with_the_row();
    let row = manifest.rule(RULE).unwrap();
    assert!(row.predicate.adding.is_some(), "{row:?}");
    let call = call(&dir().join("deny.json"));
    let judged = judge(&manifest, workspace(), &call).unwrap();
    let denial = judged.denial().unwrap_or_else(|| panic!("{judged:?}"));
    assert_eq!(denial.rule, RULE);
    let reason = denial.reason();
    for words in [RULE, row.text.as_str(), "without its path", "Instead:"] {
        assert!(reason.contains(words), "{words}: {reason}");
    }
}

/// The fixtures, as the walk over the tracked rules reads them once the
/// row is tracked: each `deny*.json` refused, each `allow*.json` no
/// finding of the row's, the exempt path among them.
#[test]
fn the_row_denies_its_fixture_and_not_its_neighbours() {
    let manifest = with_the_row();
    let mut seen = (0, 0);
    for entry in std::fs::read_dir(dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let fires = name.starts_with("deny");
        assert!(fires || name.starts_with("allow"), "{name}");
        assert_eq!(
            denied(&manifest, workspace(), &call(&path)),
            fires,
            "{name}"
        );
        if fires {
            seen.0 += 1;
        } else {
            seen.1 += 1;
        }
    }
    assert!(seen.0 >= 1 && seen.1 >= 2, "{seen:?}");
}

/// The exempt paths are let through, and their neighbours are not: this
/// file, the row's own test, is not exempt, nor is a file beside the
/// export's tool or a fixture of another rule.
#[test]
fn the_exempt_paths_pass_and_their_neighbours_are_refused() {
    let manifest = with_the_row();
    let named = &forms()[0].form;
    let content = format!("see {named} for it\n");
    for (file, refused) in [
        ("scripts/export/a-made-up-note.md", false),
        (
            "xtask/fixtures/rules/upstream.private/a-made-up.json",
            false,
        ),
        ("xtask/tests/tracked/permit/upstream.rs", false),
        ("xtask/fixtures/rules/upstream.named/a-made-up.json", false),
        ("xtask/tests/tracked/permit/upstream_named.rs", true),
        ("scripts/a-made-up-note.md", true),
        ("xtask/fixtures/rules/pipeline.lock/a-made-up.json", true),
    ] {
        for tool in ["Edit", "Write"] {
            let call = touch(tool, Path::new(file), &content);
            assert_eq!(
                denied(&manifest, workspace(), &call),
                refused,
                "{tool} {file}"
            );
        }
    }
}

/// What a write adds is judged, not what the file carries (#510): an edit
/// beside a name the file already holds is let through, and one that adds
/// a second of it is refused. A file outside the workspace, where the
/// private project keeps its own, is none the row judges, and a write under
/// the folder itself is `upstream.private`'s denial first.
#[test]
fn a_name_the_file_carries_and_a_file_outside_the_workspace_are_let_through() {
    let manifest = with_the_row();
    let named = &forms()[0].form;
    let tree = Tree::new("permit-upstream-named").unwrap();
    let carried = format!("see {named} for it\nline two\n");
    write(tree.path(), "notes.md", &carried);
    let edit = |old: &str, new: &str| {
        let input =
            serde_json::json!({"file_path": "notes.md", "old_string": old, "new_string": new});
        Call::parse(&serde_json::json!({"tool_name": "Edit", "tool_input": input}).to_string())
            .unwrap()
    };
    assert!(!denied(
        &manifest,
        tree.path(),
        &edit("line two", "line three")
    ));
    let again = format!("line two, and {named} again");
    assert!(denied(&manifest, tree.path(), &edit("line two", &again)));

    let outside = Tree::new("permit-upstream-named-outside").unwrap();
    let ticket = outside.join("private/tickets/a-made-up-defect.md");
    for tool in ["Edit", "Write"] {
        let judged = judge(&manifest, workspace(), &touch(tool, &ticket, &carried)).unwrap();
        assert!(
            judged.judged.iter().all(|id| id != RULE),
            "{tool}: {judged:?}"
        );
    }

    let under = Path::new("upstream").join("README.md");
    let judged = judge(&manifest, workspace(), &touch("Write", &under, &carried)).unwrap();
    assert_eq!(
        judged.denial().map(|f| f.rule.as_str()),
        Some("upstream.private")
    );
}
