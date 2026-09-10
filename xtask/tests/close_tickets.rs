//! `scripts/close-tickets.sh`, the CI caller of `xtask ticket close` (#25),
//! run over a scratch repository whose commits carry the trailers, with a
//! stand-in `cargo` on `PATH` that hands `xtask ticket` to this crate's
//! own binary against the scratch root, and a recorded `gh` beside it
//! that answers `issue view` from a transcript and writes down every
//! other call. So the real instrument decides, over the real trailer
//! reader, and what the tracker was asked is read back as a fact; nothing
//! here reaches GitHub.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

const RUN: &str = "https://example.invalid/actions/runs/1";
const ZERO: &str = "0000000000000000000000000000000000000000";
const UNTICKED: &str = "## Acceptance criteria\n\n- [x] one\n- [ ] two\n- [ ] three\n";
const TICKED: &str = "## Acceptance criteria\n\n- [x] one\n- [X] two\n";

/// The scratch repository and the two stand-ins beside it.
struct Scratch {
    dir: PathBuf,
    root: PathBuf,
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Scratch {
    fn lay(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("xtask-close-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("root");
        scratch_workspace(workspace(), &root).unwrap();
        let bin = dir.join("bin");
        fs::create_dir_all(dir.join("issues")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        executable(
            &bin.join("cargo"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 if [ \"$1 $2\" = 'xtask ticket' ]; then shift 1; exec '{xtask}' --root \"$PWD\" \"$@\"; fi\nexit 0\n",
                log = dir.join("cargo.log").display(),
                xtask = env!("CARGO_BIN_EXE_xtask"),
            ),
        );
        executable(
            &bin.join("gh"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = issue ] && [ \"$2\" = view ]; then cat '{0}/issues/'\"$3\".json; exit $?; fi\n\
                 printf '%s\\n' \"$*\" >> '{0}/calls'\nexit 0\n",
                dir.display()
            ),
        );
        Scratch {
            dir,
            root: fs::canonicalize(&root).unwrap(),
        }
    }

    fn issue(&self, number: u64, state: &str, body: &str) {
        let issue = serde_json::json!({
            "number": number,
            "title": format!("ticket {number}"),
            "body": body,
            "state": state,
            "url": format!("https://example.invalid/issues/{number}"),
        });
        fs::write(
            self.dir.join(format!("issues/{number}.json")),
            issue.to_string(),
        )
        .unwrap();
    }

    /// One empty commit with `message`; its sha.
    fn commit(&self, message: &str) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/var/empty",
            ])
            .args(["commit", "-q", "--allow-empty", "-F", "-"])
            .env("GIT_AUTHOR_NAME", "x")
            .env("GIT_AUTHOR_EMAIL", "x@x")
            .env("GIT_COMMITTER_NAME", "x")
            .env("GIT_COMMITTER_EMAIL", "x@x")
            .current_dir(&self.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(message.as_bytes())?;
                child.wait_with_output()
            })
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.head()
    }

    fn head(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.root)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// The caller over `before..after`, the stand-ins first on `PATH`; the
    /// exit and both streams.
    fn close(&self, before: &str, after: &str) -> (i32, String) {
        let path = std::env::var("PATH").unwrap_or_default();
        let out = Command::new(workspace().join("scripts/close-tickets.sh"))
            .args([before, after, RUN])
            .current_dir(&self.root)
            .env("PATH", format!("{}:{path}", self.dir.join("bin").display()))
            .env_remove("GITHUB_STEP_SUMMARY")
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    /// What the tracker was asked besides `issue view`, one line each.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// The fixture range the ticket asks for, and the two outcomes: a commit
/// with no trailer and one citing a number in prose close nothing; a
/// ticket the instrument refuses is commented with the unticked boxes and
/// stays open; a ticket it closes is closed by the instrument, its edge
/// set from the `Opens:` trailer, and commented with the commit, the run
/// and where the readings are.
#[test]
fn a_push_closes_what_the_trailers_name_and_tells_each_ticket_what_the_instrument_decided() {
    let scratch = Scratch::lay("range");
    scratch.issue(5, "OPEN", TICKED);
    scratch.issue(7, "OPEN", UNTICKED);
    scratch.issue(8, "OPEN", TICKED);
    scratch.issue(9, "OPEN", TICKED);
    scratch.issue(10, "OPEN", "later");
    let before = scratch.head();
    scratch.commit("no trailer at all\n\nA body that says nothing of a ticket.\n");
    scratch.commit("a number in prose\n\nCites #5 in prose and Ticket: #5 in a paragraph that is not the last.\n\nSo the trailer block is this paragraph, with no trailer in it.\n");
    let refused = scratch
        .commit("unticked boxes\n\nThe boxes are not ticked yet.\n\nOpens: nothing\nTicket: #7\n");
    // A `---` line in the body is prose, not git's patch divider: the
    // trailers after it are still read.
    let closed = scratch.commit("ticked, opens nothing\n\nA rule in the body:\n\n---\n\nand the trailers after it.\n\nOpens: nothing\nTicket: #8\n");
    let opening = scratch.commit("ticked, opens one\n\nOpens: #10\nTicket: #9\n");
    let after = scratch.head();

    let (code, out) = scratch.close(&before, &after);
    assert_eq!(code, 0, "{out}");
    let calls = scratch.calls();
    assert!(
        !calls.iter().any(|c| c.contains(" 5 ")),
        "a number in prose closes nothing: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.starts_with("issue close 7 ")),
        "the refused ticket stays open: {calls:?}"
    );
    let told_7 = calls
        .iter()
        .find(|c| c.starts_with("issue comment 7 --body "))
        .unwrap_or_else(|| panic!("#7 is told: {calls:?}"));
    assert!(
        told_7.contains("Not closed")
            && told_7.contains(RUN)
            && told_7.contains(&refused)
            && told_7.contains("(1) two; (2) three"),
        "{told_7}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("issue close 8 --comment ")),
        "{calls:?}"
    );
    let told_8 = calls
        .iter()
        .find(|c| c.starts_with("issue comment 8 --body "))
        .unwrap_or_else(|| panic!("#8 is told: {calls:?}"));
    assert!(
        told_8.starts_with("issue comment 8 --body Closed by")
            && told_8.contains(RUN)
            && told_8.contains(&closed)
            && told_8.contains("ticked, opens nothing")
            && told_8.contains("`harness` artifact")
            && told_8.contains("`tickets` artifact"),
        "{told_8}"
    );
    assert!(
        calls.contains(&"issue edit 9 --add-blocking 10".to_owned())
            && calls
                .iter()
                .any(|c| c.starts_with("issue close 9 --comment ") && c.contains("opens #10")),
        "the Opens: trailer is the edge: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("issue comment 9 --body Closed by") && c.contains(&opening)),
        "{calls:?}"
    );
    assert!(
        out.contains("#7 not closed") && out.contains("#8 closed") && out.contains("#9 closed"),
        "{out}"
    );
    assert!(
        scratch.root.join("target/harness").is_dir(),
        "the readings landed in the scratch store"
    );
}

/// `Opens:` missing is the instrument's refusal, commented like any other;
/// a `Ticket:` value that names no ticket is the job's own failure, with
/// the tracker asked nothing for it; an `Opens:` value that is neither a
/// ticket nor `nothing` is told to the ticket and red, with the instrument
/// not asked; and a `before` git does not have makes the range `after`
/// alone.
#[test]
fn a_ticket_without_opens_is_refused_by_the_instrument_and_a_bad_value_fails_the_job() {
    let scratch = Scratch::lay("flags");
    scratch.issue(8, "OPEN", TICKED);
    scratch.issue(11, "OPEN", TICKED);
    scratch.commit("would close, but is before the range\n\nOpens: nothing\nTicket: #8\n");
    let without = scratch.commit("no opens\n\nTicket: #11\n");
    let (code, out) = scratch.close(ZERO, &without);
    assert_eq!(code, 0, "{out}");
    let calls = scratch.calls();
    assert!(
        !calls.iter().any(|c| c.contains(" 8 ")),
        "the range is `after` alone: {calls:?}"
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].starts_with("issue comment 11 --body Not closed")
            && calls[0].contains("--opens <n>")
            && calls[0].contains("--opens-nothing"),
        "{calls:?}"
    );

    let bad = scratch.commit("a value that is no ticket\n\nTicket: the-branch\n");
    let (code, out) = scratch.close(&without, &bad);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("names no ticket"), "{out}");
    assert_eq!(scratch.calls().len(), 1, "nothing asked for it");

    let neither = scratch.commit("opens what\n\nOpens: #10, later\nTicket: #8\n");
    let (code, out) = scratch.close(&bad, &neither);
    assert_eq!(code, 1, "{out}");
    let calls = scratch.calls();
    assert_eq!(
        calls.len(),
        2,
        "told, and the instrument not asked: {calls:?}"
    );
    assert!(
        calls[1].starts_with("issue comment 8 --body Not closed")
            && calls[1].contains("`Opens: later` is neither a ticket nor `nothing`"),
        "{calls:?}"
    );
    assert!(
        !scratch.root.join("target/harness").exists(),
        "no reading was written for it"
    );
}

/// The job that calls the caller (#25; harness-design.md §7): on a push to
/// `main` alone, after the regions, test and per-commit jobs are green,
/// with `issues: write` for the comment, the close and the edge, handing
/// the caller the pushed range and the run's URL; a pull request and a
/// track branch settle nothing.
#[test]
fn the_tickets_job_runs_on_a_push_to_main_alone_after_the_per_commit_job_with_issues_write() {
    let text = fs::read_to_string(workspace().join(".github/workflows/harness.yml")).unwrap();
    let at = text
        .find("\n  tickets:")
        .expect("the workflow has a tickets job");
    let per_commit = text
        .find("\n  per-commit:")
        .expect("the workflow has a per-commit job");
    assert!(
        at > per_commit,
        "the tickets job is laid down after per-commit"
    );
    let job = &text[at..];
    let lines = job
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>();
    let line = |key: &str| {
        lines
            .iter()
            .find(|l| l.trim_start().starts_with(key))
            .unwrap_or_else(|| panic!("the tickets job lacks {key}: {job}"))
    };
    let when = line("if:");
    assert!(
        when.contains("github.event_name == 'push'")
            && when.contains("github.ref == 'refs/heads/main'"),
        "a push to main alone: {when}"
    );
    // A status check function in `if:` replaces the default `success()`
    // gate on `needs:`, so one here would run the job over a red lane.
    for function in ["always()", "cancelled()", "failure()"] {
        assert!(
            !when.contains(function),
            "the job waits on the three jobs being green: {when}"
        );
    }
    let needs = line("needs:");
    for job in ["regions", "xtask", "per-commit"] {
        assert!(needs.contains(job), "after {job}: {needs}");
    }
    let permissions = job
        .find("permissions:")
        .expect("the job states its permissions");
    let permissions = &job[permissions..job[permissions..].find("steps:").unwrap() + permissions];
    assert!(
        permissions.contains("issues: write"),
        "the token can comment, close and edit: {permissions}"
    );
    let call = line("run: scripts/close-tickets.sh");
    for argument in [
        "github.event.before",
        "github.sha",
        "actions/runs/${{ github.run_id }}",
    ] {
        assert!(
            call.contains(argument),
            "the caller is handed {argument}: {call}"
        );
    }
    assert!(
        job.contains("GH_TOKEN: ${{ github.token }}"),
        "gh has the job's token: {job}"
    );
}
