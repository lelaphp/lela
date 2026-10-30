//! The replay corpus (#3619): every denial the store's log kept from
//! 2026-10-15 to 2026-10-29 under a rule that places a shell line by the
//! checkout it runs in, each kept as its call, the directory the call
//! stood in, where a merge stood, and the decision the rule should have
//! made, classed by hand once. The calls a merge's owner made in its own
//! checkout are left to the owner record (#3621), since whose merge it was
//! decides them and the line alone does not.
//!
//! Each `replay.jsonl` under a rule's fixtures is replayed over the judge
//! as it stands: the wrongly denied calls must pass and the rightly denied
//! ones must fail, so a change to placement that brings back the window's
//! regression (7afa9c7c4, a command past a `cd` placed where the shell
//! started as well) fails here before it merges.

use super::*;

/// One line of a corpus.
#[derive(serde::Deserialize)]
struct Replayed {
    at: String,
    cwd: String,
    /// The checkouts a merge stood in when the call was judged.
    merging: Vec<String>,
    expect: String,
    command: String,
}

/// The places a corpus names, laid down: main's checkout, a linked
/// worktree for every `@WORKTREES@/t<n>` recording ticket `n` as `xtask
/// worktree add` does, a directory for every other path a line names under
/// the coordinator's or the sessions' scratch, and another repository.
struct World {
    _trees: [Tree; 2],
    places: Vec<(&'static str, PathBuf)>,
}

impl World {
    fn lay(name: &str, lines: &[Replayed]) -> World {
        let tree = scratch(&format!("replay-{name}"));
        let main = tree.path().canonicalize().unwrap();
        let beside = Tree::new(&format!("replay-{name}-outside")).unwrap();
        let outside = beside.path().canonicalize().unwrap();
        let worktrees = outside.join("worktrees");
        let other = outside.join("other");
        Repo::init(&other).commit("other");
        let places = vec![
            ("@MAIN@", main.clone()),
            ("@WORKTREES@", worktrees.clone()),
            ("@TMP@", outside.join("tmp")),
            ("@OTHER@", other),
            ("@PROJECTS@", outside.join("projects")),
            ("@HOME@", outside.join("home")),
        ];
        let named = regex_lite::Regex::new(r"@[A-Z]+@(/[A-Za-z0-9_./-]*)?").unwrap();
        let repo = Repo::at(&main);
        for line in lines {
            for found in named
                .find_iter(&line.command)
                .chain(named.find_iter(&line.cwd))
            {
                let path = place(&places, found.as_str());
                let Ok(under) = path.strip_prefix(&worktrees) else {
                    if !path.starts_with(&main) {
                        std::fs::create_dir_all(directory_of(&path)).unwrap();
                    }
                    continue;
                };
                let Some(first) = under.components().next() else {
                    continue;
                };
                let first = first.as_os_str().to_str().unwrap();
                let checkout = worktrees.join(first);
                if let Some(ticket) = first.strip_prefix('t').filter(|n| n.parse::<u32>().is_ok())
                    && !checkout.exists()
                {
                    let at = checkout.to_str().unwrap();
                    repo.git(&["worktree", "add", "-q", "-b", first, at]);
                    repo.git(&["config", &format!("branch.{first}.ticket"), ticket]);
                }
                std::fs::create_dir_all(directory_of(&path)).unwrap();
            }
        }
        World {
            _trees: [tree, beside],
            places,
        }
    }

    fn at(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (name, path) in &self.places {
            text = text.replace(name, path.to_str().unwrap());
        }
        text
    }
}

/// `named`, a placeholder and the path after it, as a path in `places`.
fn place(places: &[(&str, PathBuf)], named: &str) -> PathBuf {
    let (name, path) = places.iter().find(|(n, _)| named.starts_with(n)).unwrap();
    PathBuf::from(named.replacen(name, path.to_str().unwrap(), 1))
}

/// The directory a path names: itself, or its parent where its last
/// component reads as a file's name.
fn directory_of(path: &Path) -> PathBuf {
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains('.') && !n.starts_with('.'));
    match path.parent() {
        Some(parent) if file => parent.to_path_buf(),
        _ => path.to_path_buf(),
    }
}

/// Replay `rule`'s corpus over the judge: every call judged in the world
/// it was made in, its merges standing, and held to its decision.
fn replay(rule: &str) {
    let manifest = Manifest::load(workspace()).unwrap();
    let corpus = fixtures().join("rules").join(rule).join("replay.jsonl");
    let lines: Vec<Replayed> = std::fs::read_to_string(&corpus)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!lines.is_empty(), "{}", corpus.display());
    assert!(
        lines.iter().any(|l| l.expect == "allow"),
        "{rule}: no must-pass"
    );
    let world = World::lay(rule, &lines);
    let mut wrong = Vec::new();
    for line in &lines {
        let markers: Vec<PathBuf> = line
            .merging
            .iter()
            .map(|at| {
                let checkout = PathBuf::from(world.at(at));
                let git_dir = Repo::at(&checkout).git(&["rev-parse", "--absolute-git-dir"]);
                PathBuf::from(git_dir).join("MERGE_HEAD")
            })
            .collect();
        for marker in &markers {
            std::fs::write(marker, "0".repeat(40) + "\n").unwrap();
        }
        let cwd = world.at(&line.cwd);
        let call = Call::parse(
            &serde_json::json!({"tool_name": "Bash", "session_id": "replay", "cwd": cwd,
                "tool_input": {"command": world.at(&line.command)}})
            .to_string(),
        )
        .unwrap();
        let root = call.workspace(Path::new(&cwd));
        let judged = judge(&manifest, &root, &call).unwrap();
        let denied = judged.denial().is_some_and(|f| f.rule == rule);
        if denied != (line.expect == "deny") {
            wrong.push(format!(
                "{} must {}: {}\n    {:?}",
                line.at,
                line.expect,
                line.command.lines().next().unwrap_or_default(),
                judged.denial().map(Finding::reason)
            ));
        }
        for marker in &markers {
            std::fs::remove_file(marker).unwrap();
        }
    }
    assert!(
        wrong.is_empty(),
        "{rule}: {} of {} replayed calls judged otherwise:\n{}",
        wrong.len(),
        lines.len(),
        wrong.join("\n")
    );
}

#[test]
fn the_window_s_git_operation_denials_replay_as_classed() {
    replay("git.operation");
}

#[test]
fn the_window_s_merge_commits_itself_denials_replay_as_classed() {
    replay("git.merge-commits-itself");
}

#[test]
fn the_window_s_builder_subset_denials_replay_as_classed() {
    replay("lanes.builder-subset");
}
