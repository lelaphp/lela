//! The two git hooks under `scripts/hooks` (harness-design.md §6 wiring,
//! ledger 11), run by git in a scratch repository: a stand-in `cargo` on
//! `PATH` records each call and answers as the instruments would, and
//! hands `xtask regions` to this crate's own binary against the scratch
//! root, so the hook's re-stage loop reads the real reader; a stand-in
//! `computed` answers with one shape per case. What is read back is what
//! the hook did, the commit it allowed or refused, the files it staged
//! and the base it handed `done`, since a hook that exits without doing
//! its job is invisible to a substring over its text.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::scratch::Tree;

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
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
/// 1, when a test writes it. `claude_on_path` says whether `plugin-bin`
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
/// region files, one crate file, one commit, and the real hooks under
/// `scripts/hooks` as its `core.hooksPath`.
fn scratch(name: &str, computed: &Computed) -> Scratch {
    let dir = Tree::new(&format!("hooks-{name}")).unwrap();
    let root = dir.join("root");
    scratch_workspace(workspace(), &root).unwrap();
    for file in REGIONS {
        write(&root, file, "# a region file\n\n<!-- computed layout -->\n");
    }
    write(&root, "crates/php_heap/src/lib.rs", "pub fn alloc() {}\n");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("cargo"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ -f '{refuse}' ] && [ \"$(cat '{refuse}')\" = \"$1 $2\" ]; then exit 1; fi\n\
             if [ \"$1 $2\" = 'xtask regions' ]; then shift 2; exec '{xtask}' --root \"$PWD\" regions \"$@\"; fi\n\
             if [ \"$1 $2\" = 'xtask api-check' ]; then sleep 1; fi\n\
             if [ \"$1 $2\" = 'xtask test' ]; then git show :AGENTS.md > '{tested}' 2>&1; fi\n\
             printf 'ran %s\\n' \"$*\"\n\
             exit 0\n",
            log = dir.join("cargo.log").display(),
            refuse = dir.join("refuse").display(),
            tested = dir.join("tested-AGENTS.md").display(),
            xtask = env!("CARGO_BIN_EXE_xtask"),
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
        let path = "harness-plugins/hooks/harness.ts";
        write(&self.root, path, "export const register = () => {};\n");
        self.git(&["add", "--", path], false);
        self.run(&["commit", "-q", "-m", "the module", "--", path], true)
    }

    /// A change to the module and a hand-written declaration file, both
    /// staged, and the commit through the hook by pathspec.
    fn stage_declarations_and_commit(&self) -> (i32, String) {
        let module = "harness-plugins/hooks/harness.ts";
        let types = "harness-plugins/types/claude-code.d.ts";
        write(&self.root, module, "export const register = () => {};\n");
        write(&self.root, types, "// Written by hand.\n");
        self.git(&["add", "--", module, types], false);
        self.run(
            &["commit", "-q", "-m", "the module", "--", module, types],
            true,
        )
    }

    /// A change under the crate, staged, and the commit through the hook.
    fn stage_and_commit(&self) -> (i32, String) {
        write(
            &self.root,
            "crates/php_heap/src/lib.rs",
            "pub fn alloc() {}\npub fn free() {}\n",
        );
        self.git(&["add", "--", "crates/php_heap/src/lib.rs"], false);
        self.run(&["commit", "-q", "-m", "a change under a crate"], true)
    }
}

// ---------------------------------------------------------------- pre-commit

/// The instruments that only read the tree first, `fmt` and then at once
/// `api-check`, `budget`, `unsafe-audit`, `vocab` and `permit --diff`;
/// then `xtask regions --staged` over the six region files and not the
/// gitignored local one, which reaches `computed run`, and `computed
/// check` over the six; then `clippy` and `test`, then `witness` and `crosscheck` at once
/// (#1252), crosscheck on the staged extensions alone (#1312). The region
/// file `computed` rewrote is re-staged before `test` runs, so the map
/// tests read the map the commit carries from the index (#1416): a hook
/// that renders after `test` hands it the map before the render and fails
/// here. The commit carries the rendered file and the tree is clean after.
#[test]
fn pre_commit_re_stages_the_region_it_rewrote_and_the_commit_is_allowed() {
    let scratch = scratch("rendered", &Computed::Rendered);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
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
    let building = ["xtask witness --staged", "xtask crosscheck --staged"];
    let mut calls = scratch.calls("cargo");
    calls[1..6].sort_unstable();
    calls[9..11].sort_unstable();
    let rendering = format!("xtask regions --staged {regions}");
    let mut expected: Vec<String> = ["fmt --check"]
        .into_iter()
        .chain(reading)
        .chain([rendering.as_str()])
        .chain(["xtask clippy --staged", "xtask test --staged"])
        .chain(building)
        .map(str::to_owned)
        .collect();
    expected[1..6].sort_unstable();
    expected[9..11].sort_unstable();
    assert_eq!(calls, expected);
    let tested = fs::read_to_string(scratch.dir.join("tested-AGENTS.md")).unwrap();
    assert!(
        tested.ends_with("rendered\n"),
        "`xtask test` read the map from the index before the render: {tested}"
    );
    let printed: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("ran ")).collect();
    assert_eq!(printed[1..6], reading, "{out}");
    assert_eq!(printed[8..], building, "{out}");
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
            "claude [plugin][validate][harness-plugins][--strict] cwd={root} "
        )),
        "{}",
        calls[3]
    );
    assert!(
        calls[4].starts_with(&format!(
            "claude [plugin][test][harness-plugins] cwd={root} "
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
            "harness-plugins/hooks/harness.ts",
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

/// A region a hand edited is refused with the line naming
/// `computed check` and what it said; the change stays staged for the person.
#[test]
fn a_hand_edited_region_refuses_the_commit_naming_computed_check() {
    let scratch = scratch("hand-edited", &Computed::HandEdited);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(out.contains("run `computed check`"), "{out}");
    assert!(out.contains("AGENTS.md:3 layout tree hand-edited"), "{out}");
    let staged = scratch.git(&["diff", "--cached", "--name-only"], false);
    assert_eq!(staged.trim(), "crates/php_heap/src/lib.rs");
}

/// A root without a trust grant is refused with the `computed trust`
/// line as a person's, through `xtask regions`, and nothing is re-staged.
#[test]
fn an_untrusted_root_refuses_the_commit_naming_computed_trust() {
    let scratch = scratch("untrusted", &Computed::Untrusted);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains(&format!("computed trust {}", scratch.root.display())),
        "{out}"
    );
    assert!(out.contains("the regions were not rendered"), "{out}");
    let staged = scratch.git(&["diff", "--cached", "--name-only"], false);
    assert_eq!(staged.trim(), "crates/php_heap/src/lib.rs");
}

/// An instrument that fails past the first group refuses the commit by
/// name, and the ones after it still run: every command's exit is read,
/// none short-circuits.
#[test]
fn a_failing_instrument_refuses_the_commit_and_the_rest_still_run() {
    let scratch = scratch("failing", &Computed::Rendered);
    scratch.refuse("xtask clippy");
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains("pre-commit: cargo xtask clippy --staged failed"),
        "{out}"
    );
    let calls = scratch.calls("cargo");
    for after in ["xtask test --staged", "xtask witness --staged"] {
        assert!(calls.iter().any(|c| c == after), "{after}: {calls:?}");
    }
    assert!(
        calls.iter().any(|c| c.starts_with("xtask regions")),
        "{calls:?}"
    );
}

/// A refusal among the instruments that only read the tree ends the hook
/// there (#1252): `vocab` refuses, the rest of its group still runs, and
/// neither `clippy`, `test`, `witness` nor anything after them is started,
/// so the commit is refused in the seconds the group takes and not after a
/// build.
#[test]
fn a_vocab_refusal_ends_the_hook_before_clippy_runs() {
    let scratch = scratch("vocab", &Computed::Rendered);
    scratch.refuse("xtask vocab");
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains("pre-commit: cargo xtask vocab --staged failed"),
        "{out}"
    );
    assert!(
        out.contains("refused before clippy, test and witness ran"),
        "{out}"
    );
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
    let checked = format!("check {}", REGIONS.join(" "));
    assert_eq!(scratch.calls("computed"), [checked.as_str(), &checked]);
    scratch.git(&["rev-parse", "-q", "--verify", "MERGE_HEAD"], false);

    scratch.git(&["commit", "-q", "-m", "Merge t9: the work (#9)"], true);
    let t9 = scratch.git(&["rev-parse", "t9"], false);
    assert_eq!(scratch.git(&["rev-parse", "HEAD^2"], false), t9);
    let agents = scratch.git(&["show", "HEAD:AGENTS.md"], false);
    assert!(agents.ends_with("rendered\n"), "{agents}");
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
    assert_eq!(scratch.calls("cargo"), ["xtask done"]);

    scratch.git(&["commit", "-q", "--allow-empty", "-m", "one more"], false);
    let second = scratch.head();
    let (code, out) = scratch.run(&["push", "-q", "origin", &branch], true);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.calls("cargo"),
        [
            "xtask done".to_owned(),
            format!("xtask done --base {first}")
        ]
    );

    scratch.git(&["commit", "-q", "--allow-empty", "-m", "unmet"], false);
    scratch.refuse("xtask done");
    let (code, out) = scratch.run(&["push", "-q", "origin", &branch], true);
    assert_ne!(code, 0, "{out}");
    assert_eq!(
        scratch.calls("cargo").last().unwrap(),
        &format!("xtask done --base {second}")
    );
    let held = scratch.git(&["rev-parse", &format!("origin/{branch}")], false);
    assert_eq!(held.trim(), second, "the push was refused");
}
