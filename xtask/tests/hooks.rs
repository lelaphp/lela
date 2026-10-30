//! The git hooks under `scripts/hooks` (harness-design.md §6 wiring,
//! ledger 11), run by git in a scratch repository: a stand-in `cargo` on
//! `PATH` records each call and answers as the instruments would, and
//! hands `xtask regions` to this crate's own binary against the scratch
//! root, so the hook's re-stage loop reads the real reader, and
//! `xtask vocab --message` too where a test asks for the real glossary; a
//! stand-in `computed` answers with one shape per case. What is read back is what
//! the hook did, the commit it allowed or refused, the files it staged
//! and the base it handed `done`, since a hook that exits without doing
//! its job is invisible to a substring over its text. Each runs git with
//! no session of its own, as a person's terminal does, unless the test
//! names one (#3621).
#![cfg(tracked)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::owner::{self, SESSION_VARIABLE, SessionId};
use xtask::reading::{Status, Store};
use xtask::scratch::Tree;

mod common {
    pub mod repo;
}
use common::repo::scratch_workspace;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The region files pre-commit renders, in the order it names them; the
/// gitignored local file is not one of them (#1312).
const REGIONS: [&str; 6] = [
    "AGENTS.md",
    "ext/AGENTS.md",
    "data/INDEX.md",
    "docs/divergence.md",
    "harness-design.md",
    "third-engine-design.md",
];

/// A scratch repository with the stand-ins beside it: `root` is the
/// repository, `bin` holds `cargo` and `computed`, `plugin-bin` holds
/// `claude` and `npx`, `cargo.log`, `computed.log` and `plugin.log` hold
/// one line per call, and `refuse` names the one `cargo` call that exits
/// 1, when a test writes it; `vocab-message`, when a test writes it, hands
/// commit-msg's `xtask vocab --message` to this crate's own binary. `claude_on_path` says whether `plugin-bin`
/// is on the hook's `PATH`: off, the hook finds no `claude` at all.
struct Scratch {
    dir: Tree,
    root: PathBuf,
    claude_on_path: bool,
}

/// The stand-in `computed` for each case, as the shell that answers `run`
/// and `check`; `computed`'s own lines are the shapes its spec names.
enum Computed {
    /// `run` rewrites `AGENTS.md` and says so; `check` finds it fresh.
    Rendered,
    /// `run` finds every region fresh; `check` finds one hand-edited.
    HandEdited,
    /// `run` answers with an untrusted exec region; `check` finds it fresh.
    Untrusted,
    /// A region whose command fails: `run` keeps the body and says so, and
    /// `check` finds the region unrendered, as `computed` 0.3.0 answers.
    Failing,
    /// A region left as a bare closer, as `computed merge` leaves one both
    /// sides re-rendered: `check` finds it unrendered until `run` renders
    /// it, which rewrites `AGENTS.md` and says so.
    Unrendered,
}

impl Computed {
    fn body(&self) -> &'static str {
        match self {
            Computed::Rendered => {
                "if [ \"$1\" = run ]; then echo rendered >> AGENTS.md; printf 'AGENTS.md:3 layout tree stale written\\n' >&2; exit 1; fi\nexit 0\n"
            }
            Computed::HandEdited => {
                "if [ \"$1\" = check ]; then printf 'AGENTS.md:3 layout tree hand-edited\\n' >&2; exit 1; fi\nexit 0\n"
            }
            Computed::Untrusted => {
                "if [ \"$1\" = run ]; then printf 'AGENTS.md:3 layout exec untrusted skipped; run `computed trust`\\n' >&2; exit 1; fi\nexit 0\n"
            }
            Computed::Failing => {
                "if [ \"$1\" = run ]; then printf 'AGENTS.md:3 layout exec unrendered failed; body kept\\n    exit status 3\\n' >&2; exit 1; fi\nif [ \"$1\" = check ]; then printf 'AGENTS.md:3 layout exec unrendered\\n' >&2; exit 1; fi\nexit 0\n"
            }
            Computed::Unrendered => {
                "if [ \"$1\" = run ] && ! grep -qx rendered AGENTS.md; then echo rendered >> AGENTS.md; printf 'AGENTS.md:3 layout exec unrendered written\\n' >&2; exit 1; fi\nif [ \"$1\" = check ] && ! grep -qx rendered AGENTS.md; then printf 'AGENTS.md:3 layout exec unrendered\\n' >&2; exit 1; fi\nexit 0\n"
            }
        }
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, text).unwrap();
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The scratch repository: the workspace's manifest and pin, the six
/// region files, one crate file, one plugin manifest, one commit, and the real hooks under
/// `scripts/hooks` as its `core.hooksPath`.
fn scratch(name: &str, computed: &Computed) -> Scratch {
    let dir = Tree::new(&format!("hooks-{name}")).unwrap();
    let root = dir.join("root");
    scratch_workspace(workspace(), &root);
    for file in REGIONS {
        write(&root, file, "# a region file\n\n<!-- computed layout -->\n");
    }
    write(&root, "crates/php_heap/src/lib.rs", "pub fn alloc() {}\n");
    // One plugin, as pre-commit finds each: a directory with a manifest.
    write(
        &root,
        "harness-plugins/harness/.claude-plugin/plugin.json",
        "{}\n",
    );
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("cargo"),
        &format!(
            "#!/bin/sh\n\
             if [ \"$1 $2\" = 'xtask worktree' ]; then shift 2; exec '{xtask}' --root \"$PWD\" worktree \"$@\"; fi\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ -f '{refuse}' ] && [ \"$(cat '{refuse}')\" = \"$1 $2\" ]; then exit 1; fi\n\
             if [ \"$1 $2\" = 'xtask regions' ]; then shift 2; exec '{xtask}' --root \"$PWD\" regions \"$@\"; fi\n\
             if [ \"$1 $2 $3\" = 'xtask vocab --message' ] && [ -f '{vocab}' ]; then shift 2; exec '{xtask}' --root \"$PWD\" vocab \"$@\"; fi\n\
             if [ \"$1 $2\" = 'xtask api-check' ]; then sleep 1; fi\n\
             if [ \"$1 $2\" = 'xtask test' ]; then HOOKS_TESTED='{tested}' '{probe}' --exact as_xtask_test_runs_a_map_test --ignored > /dev/null 2>&1; fi\n\
             printf 'ran %s\\n' \"$*\"\n\
             exit 0\n",
            log = dir.join("cargo.log").display(),
            refuse = dir.join("refuse").display(),
            vocab = dir.join("vocab-message").display(),
            tested = dir.join("tested-AGENTS.md").display(),
            probe = std::env::current_exe().unwrap().display(),
            xtask = xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")).display(),
        ),
    );
    executable(
        &bin.join("computed"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n{body}",
            log = dir.join("computed.log").display(),
            body = computed.body(),
        ),
    );
    // The committer's Claude Code and npm, as the declarations step finds
    // them. Each `claude` call is recorded with every argument bracketed,
    // so an empty one shows, and where and under what it ran. `auth
    // status` answers as the real one does, 0 logged in and 1 not, the
    // second while a test has written `logged-out`; `-p` extracts the
    // plugin-authoring skill's files under `CLAUDE_CODE_TMPDIR` as the
    // real one does when the skill is invoked, the declarations with the
    // header line the real one writes, unless a test has written
    // `writes-nothing`; `plugin validate` and `npx` record the call alone.
    let plugin_bin = dir.join("plugin-bin");
    fs::create_dir_all(&plugin_bin).unwrap();
    let log = dir.join("plugin.log");
    executable(
        &plugin_bin.join("claude"),
        &format!(
            "#!/bin/sh\n\
             {{ printf 'claude '; for a in \"$@\"; do printf '[%s]' \"$a\"; done; printf ' cwd=%s home=%s config=%s gate=%s cmux=%s tmp=%s\\n' \"$PWD\" \"$HOME\" \"${{CLAUDE_CONFIG_DIR:-}}\" \"${{CLAUDE_CODE_ENABLE_FUNCTION_HOOKS:-}}\" \"${{CMUX_CLAUDE_HOOKS_DISABLED:-}}\" \"${{CLAUDE_CODE_TMPDIR:-}}\"; }} >> '{log}'\n\
             if [ \"$1 $2\" = 'auth status' ]; then [ -f '{logged_out}' ] && exit 1; exit 0; fi\n\
             if [ \"$1\" = -p ] && [ ! -f '{writes_nothing}' ]; then dir=\"$CLAUDE_CODE_TMPDIR/claude-$(id -u)/bundled-skills/9.9.9/0123abcd/plugin-authoring/types\"; mkdir -p \"$dir\"; printf '// Written by Claude Code 9.9.9.\\n' > \"$dir/claude-code.d.ts\"; echo ok; fi\n\
             exit 0\n",
            log = log.display(),
            logged_out = dir.join("logged-out").display(),
            writes_nothing = dir.join("writes-nothing").display(),
        ),
    );
    executable(
        &plugin_bin.join("npx"),
        &format!(
            "#!/bin/sh\nprintf 'npx %s\\n' \"$*\" >> '{log}'\nexit 0\n",
            log = log.display(),
        ),
    );

    // The repository's own history and hook path, last: the stand-ins
    // above live beside the repository and not in it, so nothing of them
    // is in the commit whichever order the two go in.
    let scratch = Scratch {
        dir,
        root: fs::canonicalize(&root).unwrap(),
        claude_on_path: true,
    };
    scratch.git(&["add", "."], false);
    scratch.git(&["commit", "-q", "-m", "regions and a crate"], false);
    scratch.git(
        &[
            "config",
            "core.hooksPath",
            workspace().join("scripts/hooks").to_str().unwrap(),
        ],
        false,
    );
    scratch
}

impl Scratch {
    /// `git` in the repository, with the stand-ins first on `PATH` when
    /// `hooked`, so a hook git runs finds them; the exit and both streams.
    /// Without `claude`, `PATH` is the stand-ins and the system's own
    /// directories alone, so the hook finds no `claude` anywhere.
    fn run(&self, args: &[&str], hooked: bool) -> (i32, String) {
        self.run_as(args, hooked, None)
    }

    /// [`Scratch::run`] from `session`, as Claude Code hands its session
    /// to a call's processes, or from none, a person's terminal.
    fn run_as(&self, args: &[&str], hooked: bool, session: Option<&str>) -> (i32, String) {
        let path = if self.claude_on_path {
            format!(
                "{}:{}",
                self.dir.join("plugin-bin").display(),
                std::env::var("PATH").unwrap_or_default()
            )
        } else {
            "/usr/bin:/bin".to_owned()
        };
        let mut command = Command::new("git");
        command
            .args(["-c", "commit.gpgsign=false"])
            .current_dir(&self.root)
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .env("GIT_AUTHOR_NAME", "x")
            .env("GIT_AUTHOR_EMAIL", "x@x")
            .env("GIT_COMMITTER_NAME", "x")
            .env("GIT_COMMITTER_EMAIL", "x@x");
        match session {
            Some(id) => command.env(SESSION_VARIABLE, id),
            None => command.env_remove(SESSION_VARIABLE),
        };
        if hooked {
            command.env("PATH", format!("{}:{path}", self.dir.join("bin").display()));
        } else {
            command.args(["-c", "core.hooksPath=/var/empty"]);
        }
        command.args(args);
        let out = command.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    /// `git` that must succeed; its stdout.
    fn git(&self, args: &[&str], hooked: bool) -> String {
        let (code, out) = self.run(args, hooked);
        assert_eq!(code, 0, "git {args:?}: {out}");
        out
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"], false).trim().to_owned()
    }

    /// One line per call of the named stand-in.
    fn calls(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.dir.join(format!("{name}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The one `cargo` call, `<subcommand> <first argument>`, that exits 1.
    fn refuse(&self, call: &str) {
        fs::write(self.dir.join("refuse"), call).unwrap();
    }

    /// The stand-in `claude` answers `auth status` as one not logged in.
    fn log_out(&self) {
        fs::write(self.dir.join("logged-out"), "").unwrap();
    }

    /// The stand-in `claude -p` runs and extracts nothing, as a build
    /// whose plugin-authoring skill carries no declarations would.
    fn write_no_declarations(&self) {
        fs::write(self.dir.join("writes-nothing"), "").unwrap();
    }

    /// A change under the plugin, staged, and the commit through the hook
    /// by pathspec, the shape AGENTS.md asks for, so the test holds that
    /// what the hook stages reaches such a commit.
    fn stage_plugin_and_commit(&self) -> (i32, String) {
        let path = "harness-plugins/harness/hooks/harness.ts";
        write(&self.root, path, "export const register = () => {};\n");
        self.git(&["add", "--", path], false);
        self.run(&["commit", "-q", "-m", "the module", "--", path], true)
    }

    /// A change to the module and a hand-written declaration file, both
    /// staged, and the commit through the hook by pathspec.
    fn stage_declarations_and_commit(&self) -> (i32, String) {
        let module = "harness-plugins/harness/hooks/harness.ts";
        let types = "harness-plugins/types/claude-code.d.ts";
        write(&self.root, module, "export const register = () => {};\n");
        write(&self.root, types, "// Written by hand.\n");
        self.git(&["add", "--", module, types], false);
        self.run(
            &["commit", "-q", "-m", "the module", "--", module, types],
            true,
        )
    }

    /// A change under the crate, staged, and the commit through the hook,
    /// by pathspec where `by_path`.
    fn stage_and_commit(&self, by_path: bool) -> (i32, String) {
        let path = "crates/php_heap/src/lib.rs";
        write(&self.root, path, "pub fn alloc() {}\npub fn free() {}\n");
        self.git(&["add", "--", path], false);
        let mut args = vec!["commit", "-q", "-m", "a change under a crate"];
        if by_path {
            args.extend(["--", path]);
        }
        self.run(&args, true)
    }
}

/// A map test as `xtask test` runs it under the hook: the stand-in `cargo
/// xtask test` runs this with the hook's environment, and it runs itself
/// again under [`xtask::test::without_git`], as the instrument runs a test
/// executable, where it reads the map as the map tests do and writes it
/// where `HOOKS_TESTED` names. Run by nothing else, it does nothing.
#[test]
#[ignore = "the stand-in `cargo xtask test` runs it under the hook"]
fn as_xtask_test_runs_a_map_test() {
    let Some(tested) = std::env::var_os("HOOKS_TESTED") else {
        return;
    };
    if std::env::var_os("HOOKS_UNDER_TEST").is_some() {
        let root = std::env::current_dir().unwrap();
        let map = xtask::test::committed(&root, "AGENTS.md").unwrap_or_else(|e| format!("{e:#}"));
        fs::write(tested, map).unwrap();
        return;
    }
    let mut test = Command::new(std::env::current_exe().unwrap());
    test.args(["--exact", "as_xtask_test_runs_a_map_test", "--ignored"])
        .env("HOOKS_UNDER_TEST", "1");
    xtask::test::without_git(&mut test);
    assert!(test.status().unwrap().success());
}

// ---------------------------------------------------------------- pre-commit

/// The instruments that only read the tree first, `fmt` and then at once
/// `api-check`, `budget`, `unsafe-audit`, `vocab` and `permit --diff`;
/// then `xtask regions --staged` over the six region files and not the
/// gitignored local one, which reaches `computed run`, and `computed
/// check` over the six; then `clippy` and `test` (#1252), and no
/// `witness`, which `done` asks of a branch once (#2982), and no
/// `crosscheck`, whose exit the hook never read (revision 140). The region
/// file `computed` rewrote is re-staged before `test` runs, so the map
/// tests read the map the commit carries from the index (#1416): a hook
/// that renders after `test` hands it the map before the render and fails
/// here. The commit carries the rendered file and the tree is clean after.
/// commit-msg runs last, handing `vocab --message` the file git wrote the
/// message to, as a whole path (#3624).
#[test]
fn pre_commit_re_stages_the_region_it_rewrote_and_the_commit_is_allowed() {
    let scratch = scratch("rendered", &Computed::Rendered);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit(false);
    assert_eq!(code, 0, "{out}");
    assert_ne!(scratch.head(), before, "the commit was allowed: {out}");

    let regions = REGIONS.join(" ");
    // A group runs at once, so its steps start in any order (#1144); what
    // they print comes out whole and in the hook's order, though
    // `api-check`, the first, ends last.
    let reading = [
        "xtask api-check --staged",
        "xtask budget --staged",
        "xtask unsafe-audit --staged",
        "xtask vocab --staged",
        "xtask permit --diff",
    ];
    let mut calls = scratch.calls("cargo");
    calls[1..6].sort_unstable();
    let rendering = format!("xtask regions --staged {regions}");
    let mut expected: Vec<String> = ["fmt --check"]
        .into_iter()
        .chain(reading)
        .chain([rendering.as_str()])
        .chain(["xtask clippy --staged", "xtask test --staged"])
        .map(str::to_owned)
        .chain([message(&scratch)])
        .collect();
    expected[1..6].sort_unstable();
    assert_eq!(calls, expected);
    let tested = fs::read_to_string(scratch.dir.join("tested-AGENTS.md")).unwrap();
    assert!(
        tested.ends_with("rendered\n"),
        "`xtask test` read the map from the index before the render: {tested}"
    );
    let printed: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("ran ")).collect();
    assert_eq!(printed[1..6], reading, "{out}");
    let message = message(&scratch);
    let building = ["xtask clippy --staged", "xtask test --staged", &message];
    assert_eq!(printed[6..], building, "{out}");
    assert_eq!(
        scratch.calls("computed"),
        [
            format!("run {regions}"),
            format!("check {}", REGIONS.join(" ")),
        ]
    );

    let committed = scratch.git(&["show", "--name-only", "--format=", "HEAD"], false);
    let mut files: Vec<_> = committed.lines().collect();
    files.sort_unstable();
    assert_eq!(files, ["AGENTS.md", "crates/php_heap/src/lib.rs"]);
    let agents = scratch.git(&["show", "HEAD:AGENTS.md"], false);
    assert!(agents.ends_with("rendered\n"), "{agents}");
    let status = scratch.git(&["status", "--porcelain"], false);
    assert_eq!(status, "", "what computed wrote is in the commit");
    assert_eq!(
        scratch.calls("plugin"),
        Vec::<String>::new(),
        "nothing under harness-plugins/ was staged, so the declarations step did not run"
    );
}

/// The `cargo` call commit-msg makes: `vocab --message` over the file git
/// wrote, named whole.
fn message(scratch: &Scratch) -> String {
    let file = scratch.root.join(".git/COMMIT_EDITMSG");
    format!("xtask vocab --message {}", file.display())
}

/// A commit by pathspec, the shape AGENTS.md asks for, is written from a
/// second index git hands the hook, and the checkout's own stays locked
/// and stale through it: the map test `xtask test` runs reads the render
/// the hook re-staged, the map the commit carries, and not the one before
/// it, though the instrument takes git's variables from its tests (#2747).
#[test]
fn a_commit_by_pathspec_tests_the_map_it_carries() {
    let scratch = scratch("rendered-by-path", &Computed::Rendered);
    let (code, out) = scratch.stage_and_commit(true);
    assert_eq!(code, 0, "{out}");
    let tested = fs::read_to_string(scratch.dir.join("tested-AGENTS.md")).unwrap();
    assert!(
        tested.ends_with("rendered\n"),
        "`xtask test` read the map from the checkout's index and not the commit's: {tested}"
    );
    let agents = scratch.git(&["show", "HEAD:AGENTS.md"], false);
    assert!(agents.ends_with("rendered\n"), "{agents}");
}

/// The fields a stand-in `claude` call recorded after its arguments.
fn fields(call: &str) -> std::collections::BTreeMap<&str, &str> {
    let at = call.find(" cwd=").unwrap_or_else(|| panic!("{call}"));
    call[at + 1..]
        .split(' ')
        .filter_map(|field| field.split_once('='))
        .collect()
}

/// The declarations step (#62, #1736): with a path under
/// `harness-plugins/` staged, the committer's own Claude Code rewrites the
/// tracked declaration file. `claude auth status` first, then one `-p`
/// that invokes the plugin-authoring skill, whose files the build extracts
/// under `CLAUDE_CODE_TMPDIR`, and its `types/claude-code.d.ts` is what is
/// copied in. It runs from a directory outside the clone under the
/// committer's own `HOME` and `CLAUDE_CONFIG_DIR`, where the login is, and
/// loads none of their settings sources, MCP servers or tools, nor the
/// hooks a launcher would add, with the variable that gates function
/// hooks; then `tsc` through the pinned `npx`, then `plugin validate
/// --strict`, then `plugin test` (#2247); what was written is re-staged,
/// so the commit, by pathspec, carries the file whose header names the
/// version.
#[test]
fn pre_commit_rewrites_the_declarations_when_the_plugin_is_staged() {
    let scratch = scratch("declarations", &Computed::Rendered);
    let before = scratch.head();
    let (code, out) = scratch.stage_plugin_and_commit();
    assert_eq!(code, 0, "{out}");
    assert_ne!(scratch.head(), before, "the commit was allowed: {out}");
    assert!(!out.contains("pre-commit: claude"), "{out}");

    let root = scratch.root.display().to_string();
    let calls = scratch.calls("plugin");
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert!(
        calls[0].starts_with("claude [auth][status] "),
        "{}",
        calls[0]
    );
    let types = &calls[1];
    assert!(
        types.starts_with(
            "claude [-p][--setting-sources][][--strict-mcp-config][--tools][]\
             [--model][haiku][--no-session-persistence][/plugin-authoring "
        ),
        "{types}"
    );
    let at = fields(types);
    assert_eq!(at["gate"], "1", "{types}");
    assert_eq!(at["cmux"], "1", "{types}");
    assert_eq!(
        Some(at["home"].to_owned()),
        std::env::var("HOME").ok(),
        "HOME is the committer's own: {types}"
    );
    assert_eq!(
        at["config"],
        std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default(),
        "CLAUDE_CONFIG_DIR is the committer's own: {types}"
    );
    for name in ["cwd", "tmp"] {
        let dir = at[name];
        assert!(!dir.is_empty(), "{name} is empty: {types}");
        assert!(
            !dir.starts_with(&format!("{root}/")) && dir != root,
            "{name} is inside the clone: {types}"
        );
    }
    assert_eq!(
        calls[2],
        "npx -y -p typescript@5.9.3 tsc -p harness-plugins"
    );
    // The validate and the tests run in the clone, under the committer's
    // own environment.
    assert!(
        calls[3].starts_with(&format!(
            "claude [plugin][validate][harness-plugins/harness][--strict] cwd={root} "
        )),
        "{}",
        calls[3]
    );
    assert!(
        calls[4].starts_with(&format!(
            "claude [plugin][test][harness-plugins/harness] cwd={root} "
        )),
        "{}",
        calls[4]
    );

    let committed = scratch.git(&["show", "--name-only", "--format=", "HEAD"], false);
    let mut files: Vec<_> = committed.lines().collect();
    files.sort_unstable();
    assert_eq!(
        files,
        [
            "AGENTS.md",
            "harness-plugins/harness/hooks/harness.ts",
            "harness-plugins/types/claude-code.d.ts"
        ]
    );
    let header = scratch.git(
        &["show", "HEAD:harness-plugins/types/claude-code.d.ts"],
        false,
    );
    assert_eq!(header, "// Written by Claude Code 9.9.9.\n");
    let status = scratch.git(&["status", "--porcelain"], false);
    assert_eq!(status, "", "what claude wrote is in the commit");
}

/// Without `claude` on `PATH` the declarations step is skipped by name,
/// nothing else of it runs, and the commit is allowed: CI holds the
/// module to the committed file.
#[test]
fn pre_commit_without_claude_skips_the_declarations_by_name_and_allows_the_commit() {
    let mut scratch = scratch("no-claude", &Computed::Rendered);
    scratch.claude_on_path = false;
    let before = scratch.head();
    let (code, out) = scratch.stage_plugin_and_commit();
    assert_eq!(code, 0, "{out}");
    assert_ne!(scratch.head(), before, "the commit was allowed: {out}");
    assert!(
        out.contains("pre-commit: claude is not on PATH; skipped"),
        "{out}"
    );
    assert!(out.contains("harness-plugins/types"), "{out}");
    assert_eq!(scratch.calls("plugin"), Vec::<String>::new());
    let committed = scratch.git(&["show", "--name-only", "--format=", "HEAD"], false);
    assert!(!committed.contains("harness-plugins/types"), "{committed}");
}

/// A `claude` that is not logged in writes no declarations, and says so
/// by name (#1736): no `-p` runs, the commit stages no declaration, so it
/// is allowed, and `tsc`, `plugin validate` and `plugin test` still hold
/// the module to the committed file.
#[test]
fn pre_commit_not_logged_in_skips_the_declarations_by_name_and_allows_the_commit() {
    let scratch = scratch("logged-out", &Computed::Rendered);
    scratch.log_out();
    let before = scratch.head();
    let (code, out) = scratch.stage_plugin_and_commit();
    assert_eq!(code, 0, "{out}");
    assert_ne!(scratch.head(), before, "the commit was allowed: {out}");
    assert!(
        out.contains("pre-commit: claude is not logged in (`claude auth status`)"),
        "{out}"
    );
    assert!(
        out.contains("harness-plugins/types was not rewritten"),
        "{out}"
    );
    let calls = scratch.calls("plugin");
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(
        calls[0].starts_with("claude [auth][status] "),
        "{}",
        calls[0]
    );
    assert_eq!(
        calls[1],
        "npx -y -p typescript@5.9.3 tsc -p harness-plugins"
    );
    assert!(
        calls[2].starts_with("claude [plugin][validate]"),
        "{}",
        calls[2]
    );
    assert!(
        calls[3].starts_with("claude [plugin][test]"),
        "{}",
        calls[3]
    );
    let committed = scratch.git(&["show", "--name-only", "--format=", "HEAD"], false);
    assert!(!committed.contains("harness-plugins/types"), "{committed}");
}

/// A commit that stages a declaration file of its own is refused while
/// `claude` is not logged in: no Claude Code here wrote what it carries,
/// and the hook names why.
#[test]
fn pre_commit_not_logged_in_refuses_a_commit_that_stages_the_declarations() {
    let scratch = scratch("logged-out-types", &Computed::Rendered);
    scratch.log_out();
    let before = scratch.head();
    let (code, out) = scratch.stage_declarations_and_commit();
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains("pre-commit: claude is not logged in (`claude auth status`)"),
        "{out}"
    );
    assert!(
        out.contains("the staged harness-plugins/types was written by no Claude Code here"),
        "{out}"
    );
}

/// A build whose `-p` exits 0 and extracts no declarations, as one without
/// the plugin-authoring skill's files would, is named as such, and the
/// commit that stages no declaration goes on, held by `tsc` to the
/// committed file: an exit alone is never read as the file written.
#[test]
fn pre_commit_names_a_build_that_writes_no_declarations_and_allows_the_commit() {
    let scratch = scratch("no-declarations", &Computed::Rendered);
    scratch.write_no_declarations();
    let before = scratch.head();
    let (code, out) = scratch.stage_plugin_and_commit();
    assert_eq!(code, 0, "{out}");
    assert_ne!(scratch.head(), before, "the commit was allowed: {out}");
    assert!(
        out.contains("extracted no plugin-authoring/types/claude-code.d.ts"),
        "{out}"
    );
    assert!(
        out.contains("harness-plugins/types was not rewritten"),
        "{out}"
    );
    let calls = scratch.calls("plugin");
    assert_eq!(calls.len(), 5, "{calls:?}");
    let committed = scratch.git(&["show", "--name-only", "--format=", "HEAD"], false);
    assert!(!committed.contains("harness-plugins/types"), "{committed}");
}

/// A region refusal ends before building instruments start (#3611),
/// preserving the staged change for the person who can resolve it.
fn assert_no_builds(scratch: &Scratch) {
    let calls = scratch.calls("cargo");
    for instrument in ["xtask clippy", "xtask test"] {
        assert!(
            !calls.iter().any(|call| call.starts_with(instrument)),
            "a refused region started {instrument}: {calls:?}"
        );
    }
}

/// A region a hand edited is refused with the line naming
/// `computed check` and what it said; the change stays staged for the person.
#[test]
fn a_hand_edited_region_refuses_the_commit_naming_computed_check() {
    let scratch = scratch("hand-edited", &Computed::HandEdited);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit(false);
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(out.contains("run `computed check`"), "{out}");
    assert!(out.contains("AGENTS.md:3 layout tree hand-edited"), "{out}");
    let staged = scratch.git(&["diff", "--cached", "--name-only"], false);
    assert_eq!(staged.trim(), "crates/php_heap/src/lib.rs");
    assert_no_builds(&scratch);
}

/// A region whose command fails is never committed unrendered: `xtask
/// regions` passes on `computed run`'s exit 1, and the `computed check`
/// after it refuses the commit naming the region (#2744).
#[test]
fn a_region_whose_command_fails_refuses_the_commit() {
    let scratch = scratch("failing-render", &Computed::Failing);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit(false);
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(out.contains("exec unrendered failed; body kept"), "{out}");
    assert!(out.contains("could not be rendered"), "{out}");
    assert!(out.contains("AGENTS.md:3 layout exec unrendered"), "{out}");
    assert_no_builds(&scratch);
}

/// A root without a trust grant is refused with the `computed trust`
/// line as a person's, through `xtask regions`, and nothing is re-staged.
#[test]
fn an_untrusted_root_refuses_the_commit_naming_computed_trust() {
    let scratch = scratch("untrusted", &Computed::Untrusted);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit(false);
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains(&format!("computed trust {}", scratch.root.display())),
        "{out}"
    );
    assert!(out.contains("the regions were not rendered"), "{out}");
    let staged = scratch.git(&["diff", "--cached", "--name-only"], false);
    assert_eq!(staged.trim(), "crates/php_heap/src/lib.rs");
    assert_no_builds(&scratch);
}

/// An instrument that fails past the first group refuses the commit by
/// name: `test` still runs after a `clippy` refusal, so the commit says
/// both (revision 140).
#[test]
fn a_failing_instrument_refuses_the_commit_by_name() {
    let scratch = scratch("failing", &Computed::Rendered);
    scratch.refuse("xtask clippy");
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit(false);
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains("pre-commit: cargo xtask clippy --staged failed"),
        "{out}"
    );
    let calls = scratch.calls("cargo");
    assert!(
        calls.iter().any(|c| c == "xtask test --staged"),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.starts_with("xtask witness")),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.starts_with("xtask regions")),
        "{calls:?}"
    );
}

/// A merge committed through `git commit` after `--no-commit`, the way
/// /specs merges into `main`, runs `clippy` and `test` and no `witness`,
/// as no commit does: the merge check's `witness --unmeasured` measures
/// the merged tree (revision 140, #2982).
#[test]
fn a_merge_commit_runs_no_witness() {
    let scratch = scratch("merge-witness", &Computed::Rendered);
    let main = scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned();
    scratch.git(&["checkout", "-q", "-b", "t9", &main], false);
    write(
        &scratch.root,
        "crates/php_heap/src/lib.rs",
        "pub fn alloc() {}\npub fn free() {}\n",
    );
    scratch.git(&["commit", "-q", "-am", "the work of #9"], false);
    scratch.git(&["checkout", "-q", &main], false);
    scratch.git(&["merge", "-q", "--no-ff", "--no-commit", "t9"], false);
    let (code, out) = scratch.run(&["commit", "-q", "-m", "Merge t9: the work (#9)"], true);
    assert_eq!(code, 0, "{out}");
    let calls = scratch.calls("cargo");
    for ran in ["xtask clippy --staged", "xtask test --staged"] {
        assert!(calls.iter().any(|c| c == ran), "{ran}: {calls:?}");
    }
    assert!(
        !calls.iter().any(|c| c.starts_with("xtask witness")),
        "{calls:?}"
    );
}

/// One session writes in a checkout, its owner (#3618 M1, #3621): with an
/// owner record naming `the-owner`, a commit from another session is
/// refused by pre-commit before any instrument starts, naming the owner,
/// the session and the checkout, and so is a merge git commits itself,
/// by pre-merge-commit, the merge left standing; the owner's commit and
/// a person's, made from no session, go through.
#[test]
fn a_commit_from_another_session_than_the_checkout_s_owner_is_refused() {
    let scratch = scratch("owner", &Computed::Rendered);
    let main = scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned();
    owner::record(&scratch.root, &SessionId::new("the-owner").unwrap()).unwrap();
    let path = "crates/php_heap/src/lib.rs";
    let commit = |text: &str, session: Option<&str>| {
        write(&scratch.root, path, text);
        scratch.git(&["add", "--", path], false);
        scratch.run_as(
            &["commit", "-q", "-m", "a change", "--", path],
            true,
            session,
        )
    };
    let before = scratch.head();
    let (code, out) = commit("pub fn free() {}\n", Some("another-session"));
    assert_eq!(code, 1, "{out}");
    let named = format!(
        "{} is session the-owner's and this is session another-session",
        scratch.root.display()
    );
    assert!(out.contains(&named), "{out}");
    assert!(out.contains("Instead: work in your own worktree"), "{out}");
    assert!(scratch.calls("cargo").is_empty(), "nothing ran: {out}");
    assert_eq!(scratch.head(), before, "{out}");

    let (code, out) = commit("pub fn free() {}\n", Some("the-owner"));
    assert_eq!(code, 0, "the owner's commit: {out}");
    let (code, out) = commit("pub fn alloc() {}\n", None);
    assert_eq!(code, 0, "a person's commit: {out}");

    scratch.git(&["checkout", "-q", "-b", "t9"], false);
    write(&scratch.root, "notes.txt", "b\n");
    scratch.git(&["add", "--", "notes.txt"], false);
    scratch.git(&["commit", "-q", "-m", "the work of #9"], false);
    scratch.git(&["checkout", "-q", &main], false);
    let before = scratch.head();
    let merge = [
        "merge",
        "-q",
        "--no-ff",
        "t9",
        "-m",
        "Merge t9: the work (#9)",
    ];
    let (code, out) = scratch.run_as(&merge, true, Some("another-session"));
    assert_ne!(code, 0, "{out}");
    assert!(
        out.contains(&named) && out.contains("pre-merge-commit"),
        "{out}"
    );
    assert_eq!(scratch.head(), before, "{out}");
    scratch.git(&["merge", "--abort"], false);
    let (code, out) = scratch.run_as(&merge, true, Some("the-owner"));
    assert_eq!(code, 0, "the owner's merge: {out}");
}

/// git reads the file `-F` names only after pre-commit, so a missing one
/// is refused by the hook before any instrument starts, by name; one that
/// exists is committed (revision 140).
#[test]
fn a_missing_message_file_is_refused_before_anything_runs() {
    let scratch = scratch("message-file", &Computed::Rendered);
    let path = "crates/php_heap/src/lib.rs";
    write(&scratch.root, path, "pub fn alloc() {}\npub fn free() {}\n");
    scratch.git(&["add", "--", path], false);
    let missing = scratch.dir.join("no-such.msg");
    let before = scratch.head();
    let (code, out) = scratch.run(
        &["commit", "-q", "-F", missing.to_str().unwrap(), "--", path],
        true,
    );
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(out.contains("not a file git can read"), "{out}");
    assert_eq!(scratch.calls("cargo"), Vec::<String>::new());

    let message = scratch.dir.join("change.msg");
    fs::write(&message, "a change under a crate\n").unwrap();
    let (code, out) = scratch.run(
        &["commit", "-q", "-F", message.to_str().unwrap(), "--", path],
        true,
    );
    assert_eq!(code, 0, "{out}");
    assert_ne!(scratch.head(), before, "the commit was allowed: {out}");
}

/// A refusal among the instruments that only read the tree ends the hook
/// there (#1252): `vocab` refuses, the rest of its group still runs, and
/// neither `clippy`, `test` nor anything after them is started,
/// so the commit is refused in the seconds the group takes and not after a
/// build.
#[test]
fn a_vocab_refusal_ends_the_hook_before_clippy_runs() {
    let scratch = scratch("vocab", &Computed::Rendered);
    scratch.refuse("xtask vocab");
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit(false);
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains("pre-commit: cargo xtask vocab --staged failed"),
        "{out}"
    );
    assert!(out.contains("refused before clippy and test ran"), "{out}");
    let mut calls = scratch.calls("cargo");
    calls[1..].sort_unstable();
    assert_eq!(
        calls,
        [
            "fmt --check",
            "xtask api-check --staged",
            "xtask budget --staged",
            "xtask permit --diff",
            "xtask unsafe-audit --staged",
            "xtask vocab --staged",
        ]
    );
    assert_eq!(scratch.calls("computed"), Vec::<String>::new());
}

// ---------------------------------------------------------------- commit-msg

/// commit-msg notes the message against the glossary its staged crate is
/// held to and refuses nothing (#3624, ruled on #3629), through the real
/// `vocab --message` against the workspace's `CONTEXT.md`:
/// `fixtures/hooks/commit-msg/avoided.txt` is committed, each word printed
/// with its line and the word the glossary says, and the reading under
/// `message` fails listing them; `clean.txt`, which carries avoided words
/// only in a code span, its subject's path and a trailer, is committed
/// and its reading passes. A hook that never reads the message, reads it
/// from where it ran rather than where git wrote it, records nothing or
/// refuses a commit fails here.
#[test]
fn commit_msg_notes_an_avoided_word_by_line_and_refuses_nothing() {
    let scratch = scratch("commit-msg", &Computed::Rendered);
    fs::write(scratch.dir.join("vocab-message"), "").unwrap();
    let glossary = fs::read(workspace().join("CONTEXT.md")).unwrap();
    fs::write(scratch.root.join("CONTEXT.md"), glossary).unwrap();
    // The store the reading lands in is ignored, as a checkout's is.
    write(&scratch.root, ".gitignore", "/target/\n");
    scratch.git(&["add", "--", "CONTEXT.md", ".gitignore"], false);
    scratch.git(&["commit", "-q", "-m", "the glossary"], false);
    let fixture = xtask::package_dir().join("fixtures/hooks/commit-msg");
    let store = Store::of_workspace(&scratch.root);
    let commit = |name: &str, text: &str| {
        let path = "crates/php_heap/src/lib.rs";
        write(&scratch.root, path, text);
        scratch.git(&["add", "--", path], false);
        let before = scratch.head();
        let file = fixture.join(name);
        let (code, out) = scratch.run(&["commit", "-q", "-F", file.to_str().unwrap()], true);
        assert_eq!(code, 0, "{name}: {out}");
        assert_ne!(scratch.head(), before, "{name} was committed: {out}");
        let reading = store.read("vocab", "message").unwrap().unwrap();
        (out, reading)
    };

    let (out, reading) = commit("avoided.txt", "pub fn alloc() {}\npub fn free() {}\n");
    for line in [
        ":1: \"helper\" in the commit message; the glossary says \"Command\"",
        ":3: \"helper\" in the commit message; the glossary says \"Command\"",
        ":3: \"output\" in the commit message; the glossary says \"Reading\"",
    ] {
        assert!(out.contains(line), "{line}: {out}");
        assert!(reading.message.contains(line), "{line}: {reading:#?}");
    }
    assert!(out.contains("noted and not refused"), "{out}");
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(reading.rule, "vocab.avoid", "{reading:#?}");

    let (_, reading) = commit("clean.txt", "pub fn alloc() {}\n");
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
}

/// A revision a branch adds is written `NEXT` and numbered by the merge
/// into `main` (#2982), its ledger row and every citation of it: a commit
/// on `main` whose Markdown or Rust still cites one, as
/// `fixtures/hooks/next/cited.md` and `cited.rs` do in each shape that has
/// landed, one wrapped onto the next line among them, is refused before
/// anything runs, naming each line, as it is on `staging`, the merge
/// queue's branch, which `main` fast-forwards to (#3622), and the same
/// commit on a ticket's branch is not.
/// Prose that names NEXT and cites no revision, the convention quoted in
/// backticks and a line that ends in "revision" among it, is no citation.
#[test]
fn a_revision_still_cited_as_next_is_refused_on_main_and_not_on_a_branch() {
    let fixture = xtask::package_dir().join("fixtures/hooks/next");
    let read = |name: &str| fs::read_to_string(fixture.join(name)).unwrap();
    let scratch = scratch("next", &Computed::Rendered);
    scratch.git(&["checkout", "-q", "-B", "main"], false);
    write(&scratch.root, "docs/notes.md", &read("prose.md"));
    scratch.git(&["add", "--", "docs/notes.md"], false);
    let (code, out) = scratch.run(&["commit", "-q", "-m", "prose"], true);
    assert_eq!(code, 0, "prose is no citation: {out}");

    fs::remove_file(scratch.dir.join("cargo.log")).unwrap();
    write(&scratch.root, "docs/ledger.md", &read("cited.md"));
    write(
        &scratch.root,
        "crates/php_heap/src/trace.rs",
        &read("cited.rs"),
    );
    let staged = ["docs/ledger.md", "crates/php_heap/src/trace.rs"];
    scratch.git(&[&["add", "--"][..], &staged].concat(), false);
    let (code, out) = scratch.run(&["commit", "-q", "-m", "citations"], true);
    assert_eq!(code, 1, "{out}");
    for line in [3, 5, 7, 9, 11] {
        assert!(
            out.contains(&format!("docs/ledger.md:{line}:")),
            "{line}: {out}"
        );
    }
    for line in [2, 3] {
        assert!(
            out.contains(&format!("crates/php_heap/src/trace.rs:{line}:")),
            "{line}: {out}"
        );
    }
    assert!(out.contains("is still NEXT"), "{out}");
    assert_eq!(scratch.calls("cargo"), Vec::<String>::new());

    scratch.git(&["checkout", "-q", "-b", "staging"], false);
    let (code, out) = scratch.run(&["commit", "-q", "-m", "citations"], true);
    assert_eq!(code, 1, "staging: {out}");
    assert!(out.contains("docs/ledger.md:3:"), "staging: {out}");

    scratch.git(&["checkout", "-q", "-b", "t9"], false);
    let (code, out) = scratch.run(&["commit", "-q", "-m", "citations"], true);
    assert_eq!(code, 0, "{out}");
}

/// The lines of the workspace's .gitattributes that set a
/// `conflict-marker-size`, the one place a file is exempted (#2996).
fn marker_exemptions() -> String {
    fs::read_to_string(workspace().join(".gitattributes"))
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with('#') && line.contains("conflict-marker-size"))
        .flat_map(|line| [line, "\n"])
        .collect()
}

/// A commit whose staged text adds a line git's check reads as a
/// conflict marker left in is refused before anything runs, naming file and
/// line (#2996); a reStructuredText section rule of the same seven `=`
/// is admitted by the workspace's .gitattributes.
#[test]
fn a_conflict_marker_left_in_is_refused_naming_file_and_line() {
    let scratch = scratch("markers", &Computed::Rendered);
    write(&scratch.root, ".gitattributes", &marker_exemptions());
    write(&scratch.root, "doc.rst", "Title\n=======\n");
    scratch.git(&["add", "--", ".gitattributes", "doc.rst"], false);
    let (code, out) = scratch.run(&["commit", "-q", "-m", "a section rule"], true);
    assert_eq!(code, 0, "{out}");

    let left = "a\n<<<<<<< ours\nb\n=======\nc\n>>>>>>> theirs\n";
    write(&scratch.root, "notes.txt", left);
    scratch.git(&["add", "--", "notes.txt"], false);
    fs::remove_file(scratch.dir.join("cargo.log")).unwrap();
    let (code, out) = scratch.run(&["commit", "-q", "-m", "a conflict"], true);
    assert_eq!(code, 1, "{out}");
    for line in [2, 4, 6] {
        let named = format!("notes.txt:{line}: leftover conflict marker");
        assert!(out.contains(&named), "{named}: {out}");
    }
    assert_eq!(scratch.calls("cargo"), Vec::<String>::new());
}

/// Both sides of a merge change one line; `git commit` concluding it with
/// the markers left in and staged is refused naming them, and so is a
/// merge `git merge` commits itself, through pre-merge-commit, whose other
/// side carries a marker line (#2996). Each merge stands uncommitted.
#[test]
fn a_merge_that_carries_a_conflict_marker_is_refused() {
    let scratch = scratch("merge-markers", &Computed::Rendered);
    let main = scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned();
    write(&scratch.root, "notes.txt", "a\n");
    scratch.git(&["add", "--", "notes.txt"], false);
    scratch.git(&["commit", "-q", "-m", "notes"], false);
    scratch.git(&["checkout", "-q", "-b", "t9"], false);
    write(&scratch.root, "notes.txt", "b\n");
    scratch.git(&["commit", "-q", "-am", "the work of #9"], false);
    scratch.git(&["checkout", "-q", &main], false);
    write(&scratch.root, "notes.txt", "c\n");
    scratch.git(&["commit", "-q", "-am", "the work on main"], false);
    let before = scratch.head();
    let (code, _) = scratch.run(&["merge", "-q", "--no-ff", "--no-commit", "t9"], false);
    assert_ne!(code, 0, "the merge conflicts");
    scratch.git(&["add", "--", "notes.txt"], false);
    let (code, out) = scratch.run(&["commit", "-q", "-m", "Merge t9: the work (#9)"], true);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("notes.txt:1: leftover conflict marker"),
        "{out}"
    );
    assert_eq!(scratch.head(), before, "{out}");
    scratch.git(&["merge", "--abort"], false);

    scratch.git(&["checkout", "-q", "-b", "t8", &main], false);
    write(&scratch.root, "other.txt", "x\n>>>>>>> theirs\n");
    scratch.git(&["add", "--", "other.txt"], false);
    scratch.git(&["commit", "-q", "-m", "the work of #8"], false);
    scratch.git(&["checkout", "-q", &main], false);
    let (code, out) = scratch.run(
        &[
            "merge",
            "-q",
            "--no-ff",
            "t8",
            "-m",
            "Merge t8: the work (#8)",
        ],
        true,
    );
    assert_ne!(code, 0, "{out}");
    assert!(
        out.contains("other.txt:2: leftover conflict marker"),
        "{out}"
    );
    assert!(out.contains("pre-merge-commit"), "{out}");
    assert_eq!(scratch.head(), before, "{out}");
}

/// No file the workspace's index holds carries a line the commit hooks
/// would refuse as a conflict marker left in: each that holds one on
/// purpose is exempted in .gitattributes (#2996).
#[test]
fn no_tracked_file_carries_a_conflict_marker_left_in() {
    let empty = Command::new("git")
        .args(["hash-object", "-t", "tree", "/dev/null"])
        .current_dir(workspace())
        .output()
        .unwrap();
    let empty = String::from_utf8(empty.stdout).unwrap();
    let out = Command::new("git")
        .args(["diff", "--cached", "--check", empty.trim()])
        .current_dir(workspace())
        .output()
        .unwrap();
    let markers: Vec<_> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.ends_with(": leftover conflict marker"))
        .map(str::to_owned)
        .collect();
    assert_eq!(markers, Vec::<String>::new());
}

// ---------------------------------------------------------- post-commit, post-merge

/// A merge closes the tickets it names (#2311): a merge `git merge`
/// commits itself reaches `xtask ticket merged` through post-merge, since
/// `ORIG_HEAD`; one committed by `git commit` after `--no-commit`, the way
/// /specs merges, reaches it through post-commit, since `HEAD^1`; and a
/// commit with one parent reaches it through neither.
#[test]
fn a_merge_reaches_ticket_merged_and_a_plain_commit_does_not() {
    let scratch = scratch("merged", &Computed::Rendered);
    let main = scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned();
    let merged = |scratch: &Scratch| {
        scratch
            .calls("cargo")
            .into_iter()
            .filter(|call| call.starts_with("xtask ticket merged"))
            .collect::<Vec<_>>()
    };
    scratch.git(&["commit", "-q", "--allow-empty", "-m", "one parent"], true);
    assert!(merged(&scratch).is_empty(), "{:?}", scratch.calls("cargo"));

    for n in ["7", "8"] {
        scratch.git(&["checkout", "-q", "-b", &format!("t{n}"), &main], false);
        let work = format!("the work of #{n}");
        scratch.git(&["commit", "-q", "--allow-empty", "-m", &work], false);
    }
    scratch.git(&["checkout", "-q", &main], false);
    scratch.git(
        &[
            "merge",
            "-q",
            "--no-ff",
            "t7",
            "-m",
            "Merge t7: the work (#7)",
        ],
        true,
    );
    assert_eq!(merged(&scratch), ["xtask ticket merged --since ORIG_HEAD"]);

    scratch.git(&["merge", "-q", "--no-ff", "--no-commit", "t8"], false);
    scratch.git(&["commit", "-q", "-m", "Merge t8: the work (#8)"], true);
    assert_eq!(
        merged(&scratch),
        [
            "xtask ticket merged --since ORIG_HEAD",
            "xtask ticket merged --since HEAD^1"
        ]
    );
}

/// A merge into the main checkout that changes the harness rebuilds
/// `target/release/xtask`, the judge every tool call tries first, in the
/// background (#2982); one that leaves the harness alone does not.
#[test]
fn a_merge_that_changes_the_harness_rebuilds_the_release_judge() {
    let scratch = scratch("rebuild", &Computed::Rendered);
    let main = scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned();
    let rebuilds = |scratch: &Scratch| {
        let built = || {
            scratch
                .calls("cargo")
                .iter()
                .filter(|call| call.as_str() == "build -q --release -p xtask")
                .count()
        };
        for _ in 0..50 {
            if built() > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        built()
    };
    for (n, path) in [
        ("7", "crates/php_heap/src/lib.rs"),
        ("8", "xtask/src/lib.rs"),
    ] {
        scratch.git(&["checkout", "-q", "-b", &format!("t{n}"), &main], false);
        write(&scratch.root, path, &format!("// the work of #{n}\n"));
        scratch.git(&["add", "--", path], false);
        scratch.git(&["commit", "-q", "-m", &format!("the work of #{n}")], false);
    }
    scratch.git(&["checkout", "-q", &main], false);
    scratch.git(&["merge", "-q", "--no-ff", "--no-commit", "t7"], false);
    scratch.git(&["commit", "-q", "-m", "Merge t7: the work (#7)"], true);
    assert_eq!(rebuilds(&scratch), 0, "{:?}", scratch.calls("cargo"));
    scratch.git(&["merge", "-q", "--no-ff", "--no-commit", "t8"], false);
    scratch.git(&["commit", "-q", "-m", "Merge t8: the work (#8)"], true);
    assert_eq!(rebuilds(&scratch), 1, "{:?}", scratch.calls("cargo"));
}

// ---------------------------------------------------------------- pre-merge-commit

/// A merge `git merge` commits itself runs no pre-commit, only
/// pre-merge-commit, and what that hook stages never reaches the merge's
/// tree, which git wrote before it ran; so a region `computed merge` left
/// unrendered, both sides having re-rendered it, was committed as a bare
/// closer four times on `main` (#2744). Pre-merge-commit refuses that
/// merge naming the region and the way through, and the merge stands
/// uncommitted; `git commit` then renders it through pre-commit, and the
/// merge commit carries the render.
#[test]
fn a_merge_that_leaves_a_region_unrendered_is_refused_until_committed_through_pre_commit() {
    let scratch = scratch("unrendered", &Computed::Unrendered);
    let main = scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned();
    scratch.git(&["checkout", "-q", "-b", "t9", &main], false);
    scratch.git(
        &["commit", "-q", "--allow-empty", "-m", "the work of #9"],
        false,
    );
    scratch.git(&["checkout", "-q", &main], false);
    let before = scratch.head();
    let (code, out) = scratch.run(
        &[
            "merge",
            "-q",
            "--no-ff",
            "t9",
            "-m",
            "Merge t9: the work (#9)",
        ],
        true,
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(scratch.head(), before, "the merge was not committed: {out}");
    assert!(out.contains("AGENTS.md:3 layout exec unrendered"), "{out}");
    assert!(out.contains("git commit"), "{out}");
    assert_eq!(
        scratch.calls("computed"),
        [format!("check {}", REGIONS.join(" "))]
    );
    scratch.git(&["rev-parse", "-q", "--verify", "MERGE_HEAD"], false);

    scratch.git(&["commit", "-q", "-m", "Merge t9: the work (#9)"], true);
    let t9 = scratch.git(&["rev-parse", "t9"], false);
    assert_eq!(scratch.git(&["rev-parse", "HEAD^2"], false), t9);
    let agents = scratch.git(&["show", "HEAD:AGENTS.md"], false);
    assert!(agents.ends_with("rendered\n"), "{agents}");
}

/// A merge `git merge` commits itself runs pre-merge-commit and not
/// pre-commit: a branch whose work still cites a revision as `NEXT`, as
/// `fixtures/hooks/next/cited.rs` does, is refused there too before
/// `computed`, naming the line, and the merge stands uncommitted.
#[test]
fn a_merge_that_brings_a_revision_still_cited_as_next_onto_main_is_refused() {
    let cited = xtask::package_dir().join("fixtures/hooks/next/cited.rs");
    let scratch = scratch("merge-next", &Computed::Rendered);
    scratch.git(&["checkout", "-q", "-B", "main"], false);
    scratch.git(&["checkout", "-q", "-b", "t9"], false);
    let file = "crates/php_heap/src/trace.rs";
    write(&scratch.root, file, &fs::read_to_string(cited).unwrap());
    scratch.git(&["add", "--", file], false);
    scratch.git(&["commit", "-q", "-m", "the work of #9"], false);
    scratch.git(&["checkout", "-q", "main"], false);
    let before = scratch.head();
    let (code, out) = scratch.run(
        &[
            "merge",
            "-q",
            "--no-ff",
            "t9",
            "-m",
            "Merge t9: the work (#9)",
        ],
        true,
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(scratch.head(), before, "the merge was not committed: {out}");
    assert!(out.contains(&format!("{file}:2:")), "{out}");
    assert!(
        out.contains("pre-merge-commit: the revision cited above is still NEXT"),
        "{out}"
    );
    assert_eq!(scratch.calls("computed"), Vec::<String>::new());
}

/// A fast-forward onto `main` writes no commit, so neither commit hook
/// reads what it brings (#3138): pre-push reads it instead. The branch
/// carrying `fixtures/hooks/next/cited.rs` is pushed as itself and not
/// read; `main` fast-forwarded onto it is refused naming the line, the
/// remote's `main` unmoved and `done` not asked, and so is the same
/// `main` pushed to a remote that has no `main` yet, read whole.
#[test]
fn a_push_of_main_that_fast_forwards_a_revision_still_cited_as_next_is_refused() {
    let cited = xtask::package_dir().join("fixtures/hooks/next/cited.rs");
    let scratch = scratch("push-next", &Computed::Rendered);
    scratch.git(&["checkout", "-q", "-B", "main"], false);
    remote(&scratch);
    let (code, out) = scratch.run(&["push", "-q", "origin", "main"], true);
    assert_eq!(code, 0, "{out}");
    let before = scratch.head();

    scratch.git(&["checkout", "-q", "-b", "t9"], false);
    let file = "crates/php_heap/src/trace.rs";
    write(&scratch.root, file, &fs::read_to_string(cited).unwrap());
    scratch.git(&["add", "--", file], false);
    scratch.git(&["commit", "-q", "-m", "the work of #9"], false);
    let (code, out) = scratch.run(&["push", "-q", "origin", "t9"], true);
    assert_eq!(code, 0, "another branch is not read: {out}");
    assert_eq!(
        scratch.calls("cargo"),
        [
            "xtask done --caller pre-push",
            "xtask done --caller pre-push"
        ]
    );

    scratch.git(&["checkout", "-q", "main"], false);
    scratch.git(&["merge", "-q", "--ff-only", "t9"], false);
    let (code, out) = scratch.run(&["push", "-q", "origin", "main"], true);
    assert_ne!(code, 0, "{out}");
    assert!(out.contains(&format!("{file}:2:")), "{out}");
    assert!(
        out.contains("pre-push: the revision cited above is still NEXT"),
        "{out}"
    );
    let held = scratch.git(&["rev-parse", "origin/main"], false);
    assert_eq!(held.trim(), before, "the push was refused");
    assert_eq!(scratch.calls("cargo").len(), 2, "done was not asked");

    let fresh = scratch.dir.join("fresh.git");
    scratch.git(&["init", "-q", "--bare", fresh.to_str().unwrap()], false);
    let (code, out) = scratch.run(&["push", "-q", fresh.to_str().unwrap(), "main"], true);
    assert_ne!(code, 0, "a remote with no main yet reads main whole: {out}");
    assert!(out.contains(&format!("{file}:2:")), "{out}");
    assert_eq!(scratch.calls("cargo").len(), 2, "done was not asked");

    // A remote sha this clone does not hold, a `main` moved there and not
    // fetched, is a range nothing can read: refused, not read as clean.
    let unheld = "1234567890abcdef1234567890abcdef12345678";
    let out = Command::new(workspace().join("scripts/hooks/next-cited"))
        .args([unheld, "HEAD"])
        .current_dir(&scratch.root)
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!("{unheld} is no commit")),
        "{out:?}"
    );
}

/// computed reads the working tree and the merge commits the index: a
/// region file whose staged copy is unrendered and whose working copy is
/// rendered would pass `computed check` and be committed unrendered, so
/// pre-merge-commit refuses the merge naming the file, before `computed`.
#[test]
fn a_merge_whose_region_file_differs_from_its_working_copy_is_refused() {
    let scratch = scratch("unstaged-render", &Computed::Unrendered);
    let main = scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned();
    scratch.git(&["checkout", "-q", "-b", "t9", &main], false);
    scratch.git(
        &["commit", "-q", "--allow-empty", "-m", "the work of #9"],
        false,
    );
    scratch.git(&["checkout", "-q", &main], false);
    let agents = fs::read_to_string(scratch.root.join("AGENTS.md")).unwrap();
    write(&scratch.root, "AGENTS.md", &format!("{agents}rendered\n"));
    let before = scratch.head();
    let (code, out) = scratch.run(
        &[
            "merge",
            "-q",
            "--no-ff",
            "t9",
            "-m",
            "Merge t9: the work (#9)",
        ],
        true,
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(scratch.head(), before, "the merge was not committed: {out}");
    assert!(out.contains("AGENTS.md\n"), "{out}");
    assert!(out.contains("not what the merge would commit"), "{out}");
    assert_eq!(scratch.calls("computed"), Vec::<String>::new());
}

// ---------------------------------------------------------------- pre-push

/// A bare remote beside the repository, as `origin`.
fn remote(scratch: &Scratch) -> String {
    let bare = scratch.dir.join("remote.git");
    let out = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&bare)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    scratch.git(&["remote", "add", "origin", bare.to_str().unwrap()], false);
    scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned()
}

/// The first push of a ref the remote lacks hands `done` no base, since
/// git's remote sha is all zeros; the next push hands it the sha the
/// remote has, so `done` judges what the push adds; and a `done` that
/// is unmet refuses the push, the remote ref unmoved.
#[test]
fn pre_push_hands_done_the_remote_sha_and_no_base_for_a_ref_the_remote_lacks() {
    let scratch = scratch("push", &Computed::Rendered);
    let branch = remote(&scratch);
    let first = scratch.head();

    let (code, out) = scratch.run(&["push", "-q", "origin", &branch], true);
    assert_eq!(code, 0, "{out}");
    assert_eq!(scratch.calls("cargo"), ["xtask done --caller pre-push"]);

    scratch.git(&["commit", "-q", "--allow-empty", "-m", "one more"], false);
    let second = scratch.head();
    let (code, out) = scratch.run(&["push", "-q", "origin", &branch], true);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.calls("cargo"),
        [
            "xtask done --caller pre-push".to_owned(),
            format!("xtask done --caller pre-push --base {first}")
        ]
    );

    scratch.git(&["commit", "-q", "--allow-empty", "-m", "unmet"], false);
    scratch.refuse("xtask done");
    let (code, out) = scratch.run(&["push", "-q", "origin", &branch], true);
    assert_ne!(code, 0, "{out}");
    assert_eq!(
        scratch.calls("cargo").last().unwrap(),
        &format!("xtask done --caller pre-push --base {second}")
    );
    let held = scratch.git(&["rev-parse", &format!("origin/{branch}")], false);
    assert_eq!(held.trim(), second, "the push was refused");
}

// ------------------------------------------------------------- the merge queue

/// The niceness of the process `pid`, as `ps` prints it.
fn niceness(pid: &str) -> i32 {
    let out = Command::new("ps")
        .args(["-o", "nice=", "-p", pid])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// The merge queue's commits run at full priority (#3622): pre-commit
/// renices a commit in a linked worktree, a builder's, so it yields the
/// cores to the merge, and the merge worktree, the linked worktree on
/// `staging`, is that merge, so its hook is not reniced. Each call of the
/// stand-in `cargo` records the niceness it ran at.
#[test]
fn a_commit_on_staging_runs_at_full_priority_and_a_builder_s_yields() {
    let scratch = scratch("renice", &Computed::Rendered);
    let own = niceness(&std::process::id().to_string());
    let log = scratch.dir.join("nice.log");
    let cargo = scratch.dir.join("bin/cargo");
    let body = fs::read_to_string(&cargo).unwrap().replacen(
        "#!/bin/sh\n",
        &format!("#!/bin/sh\nps -o nice= -p $$ >> '{}'\n", log.display()),
        1,
    );
    executable(&cargo, &body);
    let mut seen = Vec::new();
    for branch in ["staging", "t9"] {
        let dir = scratch.dir.join(branch);
        let path = dir.to_str().unwrap();
        scratch.git(&["worktree", "add", "-q", "-b", branch, path], false);
        write(&dir, "crates/php_heap/src/lib.rs", "pub fn free() {}\n");
        scratch.git(
            &["-C", path, "add", "--", "crates/php_heap/src/lib.rs"],
            false,
        );
        let (code, out) = scratch.run(&["-C", path, "commit", "-q", "-m", "a change"], true);
        assert_eq!(code, 0, "{branch}: {out}");
        let levels: Vec<i32> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| l.trim().parse().unwrap())
            .collect();
        fs::remove_file(&log).unwrap();
        assert!(!levels.is_empty(), "{branch}: {out}");
        seen.push(levels);
    }
    assert!(
        seen[0].iter().all(|&n| n == own),
        "staging: {:?} at {own}",
        seen[0]
    );
    // The first call is the owner's, `xtask worktree whose`, made before
    // the renice; every one after it, the builds among them, yields.
    if own < 20 {
        assert!(
            seen[1].len() > 1 && seen[1][1..].iter().all(|&n| n > own),
            "t9: {:?} at {own}",
            seen[1]
        );
    }
}

/// The merge queue's fast-forward closes the tickets (#3622): a merge
/// committed on `staging` reaches `xtask ticket merged` through
/// post-commit, which closes nothing off `main`, and `main`'s `git merge
/// --ff-only staging` reaches it through post-merge, since `ORIG_HEAD`,
/// which reads the merges the fast-forward brought.
#[test]
fn a_fast_forward_of_main_to_staging_reaches_ticket_merged() {
    let scratch = scratch("staging-ff", &Computed::Rendered);
    let main = scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned();
    let merged = |scratch: &Scratch| {
        scratch
            .calls("cargo")
            .into_iter()
            .filter(|call| call.starts_with("xtask ticket merged"))
            .collect::<Vec<_>>()
    };
    scratch.git(&["checkout", "-q", "-b", "t7", &main], false);
    scratch.git(
        &["commit", "-q", "--allow-empty", "-m", "the work of #7"],
        false,
    );
    scratch.git(&["checkout", "-q", "-b", "staging", &main], false);
    scratch.git(&["merge", "-q", "--no-ff", "--no-commit", "t7"], false);
    scratch.git(&["commit", "-q", "-m", "Merge t7: the work (#7)"], true);
    assert_eq!(merged(&scratch), ["xtask ticket merged --since HEAD^1"]);
    scratch.git(&["checkout", "-q", &main], false);
    scratch.git(&["merge", "-q", "--ff-only", "staging"], true);
    assert_eq!(
        merged(&scratch),
        [
            "xtask ticket merged --since HEAD^1",
            "xtask ticket merged --since ORIG_HEAD"
        ]
    );
}
