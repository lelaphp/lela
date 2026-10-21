//! The call handed to the build of the checkout it is in (#1047).
//!
//! The hook runs the checkout's `xtask`, and `permit` places a call in
//! whichever checkout of the repository its file or its working directory
//! is in (harness-design.md revision 56). A worktree laid before a
//! `seams.toml` change carries rules no build of the checkout was compiled
//! against, so this build does not judge the call and no rebuild of the
//! checkout would ([`Judge::Checkout`]). The worktree's own build judges
//! it: the same walk the hook makes, over the worktree's candidates, each
//! handed the hook's stdin and `--root` the worktree, and the first that
//! exits 0 is the decision, its stdout this build's own.
//!
//! The candidate asked is always the worktree's root, and a call there is
//! in no checkout but the root, so a build handed a call never hands it
//! on again. A walk that ends with no candidate judging is
//! [`Refusal::HandOff`], the call unjudged, never allowed.
//!
//! [`Judge::Checkout`]: crate::built::Judge::Checkout

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::anyhow;

use super::Adapter;
use crate::built::Refusal;

/// One build a walk may hand the call to: a binary built in the checkout,
/// or `cargo run` there, which rebuilds from the tree and closes the walk.
enum Candidate {
    Built(PathBuf),
    Cargo,
}

impl Candidate {
    /// The walk over `checkout` in the adapter's order: the two built
    /// binaries in the order that adapter's own walk takes them (§4.4,
    /// release first under the command hook and debug first above it, so
    /// one stale build does not silence both), then cargo.
    fn walk(checkout: &Path, adapter: Adapter) -> [Candidate; 3] {
        let [first, second] = match adapter {
            Adapter::CommandHook => ["release", "debug"],
            Adapter::AboveTheCommandHook => ["debug", "release"],
        };
        let built =
            |profile: &str| Candidate::Built(checkout.join("target").join(profile).join("xtask"));
        [built(first), built(second), Candidate::Cargo]
    }

    /// The command that asks this candidate to judge the call, `None` for
    /// a binary that is not there.
    fn command(&self, checkout: &Path, adapter: Adapter) -> Option<Command> {
        let mut command = match self {
            Candidate::Built(binary) => {
                if !binary.is_file() {
                    return None;
                }
                Command::new(binary)
            }
            Candidate::Cargo => {
                let mut cargo = Command::new("cargo");
                cargo
                    .args(["run", "-q", "--manifest-path"])
                    .arg(checkout.join("Cargo.toml"))
                    .args(["-p", "xtask", "--"]);
                cargo
            }
        };
        command
            .arg("--root")
            .arg(checkout)
            .arg("permit")
            .current_dir(checkout)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if adapter == Adapter::AboveTheCommandHook {
            command.arg("--above-the-command-hook");
        }
        Some(command)
    }
}

/// Hand the call, `stdin` as the hook gave it, to the build of `checkout`,
/// and return what that build printed: its decision, which is this
/// build's. The judging build's stderr is passed on; a candidate passed
/// over says nothing, and the last one's stderr is in the refusal, since
/// cargo closes the walk and its words are the ones a person needs.
pub(super) fn hand_off(checkout: &Path, stdin: &str, adapter: Adapter) -> Result<Vec<u8>, Refusal> {
    let mut last = String::from("no candidate was there to ask");
    for candidate in Candidate::walk(checkout, adapter) {
        let Some(command) = candidate.command(checkout, adapter) else {
            continue;
        };
        match ask(command, stdin) {
            Ok(out) if out.status.success() => {
                std::io::stderr().write_all(&out.stderr).ok();
                return Ok(out.stdout);
            }
            Ok(out) => String::from_utf8_lossy(&out.stderr)
                .trim()
                .clone_into(&mut last),
            Err(err) => last = format!("{err:#}"),
        }
    }
    Err(Refusal::HandOff(anyhow!(
        "{} carries a manifest this build was not compiled against, and none of its own \
         candidates judged this call: {last}",
        checkout.join(crate::inputs::MANIFEST_FILE).display()
    )))
}

/// One candidate asked: the call written to its stdin from a thread of its
/// own, so a build that fails before reading it, and writes its failure to
/// a pipe nobody drains yet, cannot stall the walk.
pub(super) fn ask(mut command: Command, stdin: &str) -> anyhow::Result<Output> {
    let mut child = command.spawn()?;
    let mut pipe = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("the candidate's stdin was not piped"))?;
    std::thread::scope(|scope| {
        // A candidate that exits without reading its stdin closes the
        // pipe; its exit says what happened, not the write.
        scope.spawn(move || pipe.write_all(stdin.as_bytes()).ok());
        Ok(child.wait_with_output()?)
    })
}
