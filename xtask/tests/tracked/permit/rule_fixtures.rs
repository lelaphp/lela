//! Each judged rule of the tracked manifest held to its fixtures: the
//! call it fires on and the neighbour it lets through, a rule with
//! none, the stale and whole-file fixtures, and the proposed rules'
//! own.

use super::*;

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
    // A rule scoped by `where` is judged in a checkout whose branch
    // records a ticket, which the tree the tests run in need not be.
    let ticketed = checkout("fixtures-ticketed", Some(1));
    // One scoped to `where = "operating"` in a checkout whose merge no
    // session of the fixture's started (#2476), and every other `Bash`
    // rule in a checkout where none stands, since the tests' own checkout
    // holds a merge while its merge commit's hook runs them; a write is
    // judged against the workspace's files.
    let merging = operating("fixtures-operating");
    let quiet = checkout("fixtures-quiet", None);
    for rule in judged {
        let dir = fixtures().join("rules").join(&rule.id);
        let kind = Kind::of(rule);
        let workspace = || match rule.within {
            Where::Anywhere if rule.tool.iter().any(|t| t == "Bash") => quiet.path(),
            Where::Anywhere => workspace(),
            Where::Ticketed => ticketed.path(),
            Where::Operating => merging.path(),
            Where::Main | Where::Checkout => quiet.path(),
        };
        // Every control of the rule's directory: each `deny*.json` (or
        // `note*.json`) must fire, each `allow*.json` must not (#625).
        let named = |prefix: &str| -> Vec<PathBuf> {
            let mut all: Vec<PathBuf> = std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .map(|entry| entry.unwrap().path())
                .filter(|p| {
                    let name = p.file_name().unwrap().to_str().unwrap();
                    name.starts_with(prefix) && p.extension().is_some_and(|e| e == "json")
                })
                .collect();
            all.sort();
            assert!(!all.is_empty(), "{}: no {prefix}*.json", dir.display());
            all
        };
        let firing = named(match kind {
            Kind::Deny => "deny",
            Kind::Note => "note",
        });
        for fires in &firing {
            let judgement = judge(&manifest, workspace(), &call(fires)).unwrap();
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
                    Some(rule.id.as_str()),
                    "{}",
                    fires.display()
                );
            }
        }
        // `allow-main.json` is a neighbour in main's checkout, where a
        // ticketed rule does not judge, an `allow-starter*.json` one
        // in a checkout whose operation the fixture's session started,
        // which a walk does not record, and an `allow-excused*.json` one
        // under an `unless_argv` a person has yet to paste (#3068): each
        // one's own test holds it.
        let neighbours = named("allow").into_iter().filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            !name.starts_with("allow-starter")
                && !unpasted(rule, &name)
                && (rule.within == Where::Anywhere || !p.ends_with("allow-main.json"))
        });
        for neighbour in neighbours {
            let judgement = judge(&manifest, workspace(), &call(&neighbour)).unwrap();
            assert!(
                judgement.findings.iter().all(|f| f.rule != rule.id),
                "{}: the neighbour {} fired: {judgement:?}",
                rule.id,
                neighbour.display()
            );
        }
    }
}

/// Every rule with a `tool` or a `scope` is either judged here, with its
/// fixture, or carries one of the five predicates that read the tree after
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
        let after_the_write = p.deny_methods.is_some()
            || p.check.is_some()
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
    let quiet = checkout("pathspec-quiet", None);
    let judged = judge(&manifest, quiet.path(), &call(&dir.join("deny.json"))).unwrap();
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
    let judged = judge(&manifest, quiet.path(), &call(&dir.join("allow.json"))).unwrap();
    assert!(
        judged.findings.iter().all(|f| f.rule != "git.pathspec"),
        "the neighbour fired: {judged:?}"
    );
    for line in [
        "git add .",
        "git add -u",
        "git commit -a -m msg",
        "git add -- .",
        "git -C /Users/x/dax-php commit -F /tmp/msg",
    ] {
        let judged = judge(&manifest, quiet.path(), &bash(line)).unwrap();
        assert_eq!(
            judged.denial().map(|f| f.rule.as_str()),
            Some("git.pathspec"),
            "{line}: {judged:?}"
        );
    }
    for line in [
        "git add -- AGENTS.md",
        "git commit -F /tmp/msg -- xtask/src",
        "git add -A -- xtask/fixtures/rules/git.pathspec",
        "git worktree add -b t/x .claude/worktrees/t/x main",
        "cargo add serde",
    ] {
        let judged = judge(&manifest, quiet.path(), &bash(line)).unwrap();
        assert!(judged.denial().is_none(), "{line}: {judged:?}");
    }
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
        Some("lanes.nightly-flags"),
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

/// The whole-file case (#134, #510): the shape the ticket named, a `Write`
/// of a whole file with a status `--accept` captured in it, as one fixture
/// beside the `Edit` calls the directory carries. The fixture is judged
/// twice: over this workspace, where the file at that path carries no such
/// line, the write adds one and is denied with the rule id; over a tree
/// where the same file stands, it adds none and is permitted with no
/// reading behind it, which is the file a session must be able to write
/// again. Beside it, `beside-a-pinned-one.json` edits that same tree, the
/// verdicts untouched, and brings one more: the negative control of the
/// same pair, denied though the line it types stands in the file already
/// and the crate's reading is fresh.
#[test]
fn the_whole_file_fixture_is_denied_here_and_permitted_where_the_file_carries_it() {
    let dir = fixtures().join("rules/witness.exit-authored");
    let written = call(&dir.join("whole-file.json"));
    assert_eq!(
        written.tool_input.file_path.as_deref(),
        Some(FILE),
        "the case is over the crate the scratch half lays down"
    );
    let text = written
        .tool_input
        .content
        .clone()
        .expect("a Write carries the whole file");
    assert!(text.contains("exit=255"), "a captured status: {text}");

    let manifest = Manifest::load(workspace()).unwrap();
    let denied = denial(workspace(), &manifest, &written)
        .expect("the file at that path carries no such line");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");

    let root = scratch("unless-whole-file");
    let manifest = Manifest::load(&root).unwrap();
    lay_down(&root, &text);
    assert_eq!(
        denial(&root, &manifest, &written),
        None,
        "the whole file as it stands adds no verdict"
    );

    witness_reading(&root);
    let beside = call(&dir.join("beside-a-pinned-one.json"));
    let denied = denial(&root, &manifest, &beside)
        .expect("the edit brings a verdict the file does not carry yet");
    assert!(denied.contains(EXIT_AUTHORED), "{denied}");
}

/// Every `Bash` rule judged at the call holds to each of its fixtures
/// (#1832, #1860): every `deny*.json` is denied by the rule and every
/// `allow*.json` let through, whether the rule reads the line as a
/// `deny_pattern` or the command's argv as `deny_argv` or `require_argv`.
/// A rule scoped `where = "ticketed"` is judged in a checkout whose branch
/// records a ticket, but for an `allow-main*` call, the merge check's,
/// which passes in main's checkout alone. A rule with an `exempt_label`
/// asks the tracker, an `allow-starter*` call passes an operation its
/// session started, which a walk does not record, and an
/// `allow-excused*` call an `unless_argv` yet to be pasted: each is held
/// by its own test.
#[test]
fn every_bash_rule_holds_to_each_of_its_fixtures() {
    let manifest = Manifest::load(workspace()).unwrap();
    let ticketed = checkout("bash-fixtures-ticketed", Some(1832));
    let main = checkout("bash-fixtures-main", None);
    let merging = operating("bash-fixtures-operating");
    let rules: Vec<&Rule> = manifest
        .rules
        .iter()
        .filter(|r| {
            judged_at_the_call(r) && r.tool.iter().any(|t| t == "Bash") && r.exempt_label.is_empty()
        })
        .collect();
    // `git.no-verify`, `ticket.close-api`, `lanes.nightly`,
    // `lanes.nightly-flags`, `lanes.builder-subset` and `computed.force`
    // at #1877: a filter that finds fewer reads nothing.
    assert!(
        rules.len() >= 6,
        "{:?}",
        rules.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    for rule in rules {
        let dir = fixtures().join("rules").join(&rule.id);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names
            .iter()
            // A rule's replay corpus is the replay's to judge ([`super::replay`]).
            .filter(|n| Path::new(n).extension().is_some_and(|e| e == "json"))
            .filter(|n| !n.starts_with("allow-starter") && !unpasted(rule, n))
        {
            let root = match rule.within {
                Where::Ticketed if name.starts_with("allow-main") => main.path(),
                Where::Ticketed => ticketed.path(),
                Where::Anywhere => workspace(),
                Where::Operating => merging.path(),
                Where::Main | Where::Checkout => main.path(),
            };
            let judged = judge(&manifest, root, &call(&dir.join(name))).unwrap();
            let fired = judged
                .findings
                .iter()
                .any(|f| f.rule == rule.id && f.kind == Kind::Deny);
            assert_eq!(fired, name.starts_with("deny"), "{}: {name}", rule.id);
        }
    }
}
