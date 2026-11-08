//! The Stop's turn (harness-design.md §4.5): what the Stop hook decides
//! over the values `done::hook` gathered, and the memory each agent of a
//! session keeps between its judged Stops (Round 2, Q32 (b)).
//!
//! A judged Stop is one the hook ran the list at, `stop_hook_active`
//! false, at `Stop` or at `SubagentStop`. The decision is a table over
//! what each checkout gave and what the last judged Stop of the same
//! session and agent held ([`decide`], Round 1, Q13 (d) and Q15 (a)):
//!
//! | what the checkouts gave | the turn |
//! |---|---|
//! | a checkout with no verdict | [`Turn::Unjudged`]: holds with the whole list, whatever the memory |
//! | nothing unmet | [`Turn::Met`]: passes and clears the memory |
//! | no bullet the last judged Stop did not name | [`Turn::Repeat`]: passes and is recorded |
//! | a bullet the last judged Stop did not name | [`Turn::New`]: holds with the whole list |
//!
//! There is no time bound. The memory is the bullets of the last judged
//! Stop, so a bullet that left the list and came back is new against it.
//! The second Stop stays unjudged: it releases the block and reads the
//! count the block had from the memory, which it leaves as it was.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::reading::{self, Store};
use crate::sha256::Sha256;
use crate::track;

/// What one checkout the Stop judged gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    /// Every condition met.
    Met,
    /// Held: the checkout, its unmet lines, and the list as the hold says
    /// it.
    Held {
        checkout: PathBuf,
        bullets: Vec<String>,
        list: String,
    },
    /// No verdict: the checkout could not be judged, or what its build
    /// handed back could not be read as a list.
    Unjudged { reason: String },
}

impl Part {
    /// What a checkout's own build handed back as `reason`, its list as
    /// text: its bullets are the lines under `unmet:` (the child's list is
    /// read from its reason, Round 2, Q32 (b)). A reason with no such line
    /// is a hold no bullet says, so it is unjudged.
    pub(crate) fn handed(checkout: &Path, reason: String) -> Part {
        let bullets = bullets(&reason);
        if bullets.is_empty() {
            return Part::Unjudged { reason };
        }
        Part::Held {
            checkout: checkout.to_path_buf(),
            bullets,
            list: reason,
        }
    }

    /// What the hold says of this part, `None` for a met one.
    fn said(&self) -> Option<&str> {
        match self {
            Part::Met => None,
            Part::Held { list, .. } => Some(list),
            Part::Unjudged { reason } => Some(reason),
        }
    }

    /// Each bullet's digest: its checkout and its line, so one line in two
    /// checkouts is two bullets.
    fn digests(&self) -> Vec<Sha256> {
        match self {
            Part::Held {
                checkout, bullets, ..
            } => {
                let at = checkout.to_string_lossy();
                (bullets.iter())
                    .map(|line| Sha256::of(format!("{at}\n{line}").as_bytes()))
                    .collect()
            }
            Part::Met | Part::Unjudged { .. } => Vec::new(),
        }
    }
}

/// The unmet lines of a list as `done` renders it: each `- ` line of the
/// block `unmet:` heads, which the next line that is no bullet ends.
fn bullets(list: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut under = false;
    for line in list.lines() {
        if line == "unmet:" {
            under = true;
        } else if let Some(bullet) = line.strip_prefix("- ").filter(|_| under) {
            found.push(bullet.to_owned());
        } else {
            under = false;
        }
    }
    found
}

/// What the last judged Stop of one session and agent held: its bullets'
/// digests, and how many unmet it counted, which the second Stop's release
/// records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Memory {
    pub(crate) bullets: BTreeSet<Sha256>,
    pub(crate) unmet: usize,
}

/// The file of `agent` of `session`'s memory: `<main store>/turn/<session>/`,
/// `main.json` for the main loop and `agent-<id>.json` for a subagent,
/// each id made a file name; `None` for a Stop that names no session,
/// which keeps no memory. The one place the path and its key are made.
pub(crate) fn path(root: &Path, session: &str, agent: Option<&str>) -> Option<PathBuf> {
    if session.is_empty() {
        return None;
    }
    let main = track::main_worktree(root).unwrap_or_else(|_| root.to_path_buf());
    let file = match agent.filter(|a| !a.is_empty()) {
        None => "main.json".to_owned(),
        Some(agent) => format!("agent-{}.json", file_name(agent)),
    };
    let dir = Store::of_workspace(&main).root().join("turn");
    Some(dir.join(file_name(session)).join(file))
}

/// `id` with every character but an ASCII letter, a digit and `-` made `_`.
fn file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Memory {
    /// The memory at `path`, `None` where there is none or it does not
    /// read, which judges the Stop as a first one: every bullet new.
    pub(crate) fn read(path: &Path) -> Option<Memory> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// `memory` written at `path`, or the file removed for `None`.
    pub(crate) fn keep(path: &Path, memory: Option<&Memory>) -> Result<()> {
        match memory {
            Some(memory) => reading::replace(path, &serde_json::to_vec(memory)?),
            None if path.exists() => Ok(std::fs::remove_file(path)?),
            None => Ok(()),
        }
    }
}

/// How the Stop ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Turn {
    /// Nothing unmet in any checkout: passes, and clears the memory.
    Met,
    /// Unmet, and no bullet the last judged Stop did not name: the same
    /// list or a shrunk one, passed and recorded (a repeat).
    Repeat,
    /// A bullet the last judged Stop did not name: holds with the whole
    /// list.
    New,
    /// A checkout gave no verdict: holds with the whole list, whatever the
    /// memory says.
    Unjudged,
}

impl Turn {
    /// Whether the turn is held.
    pub(crate) fn holds(self) -> bool {
        match self {
            Turn::Met | Turn::Repeat => false,
            Turn::New | Turn::Unjudged => true,
        }
    }
}

/// What the Stop decided: how it ends, the bullets added and removed
/// against the last judged Stop's, the list a hold says, and the memory
/// the next judged Stop reads, `None` to clear it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) turn: Turn,
    pub(crate) added: usize,
    pub(crate) removed: usize,
    pub(crate) list: String,
    pub(crate) memory: Option<Memory>,
}

/// The Stop over `parts`, one per checkout it judged, against `last`, the
/// memory of the last judged Stop of the same session and agent; `unmet`
/// is the count over every checkout, which the memory keeps for the
/// release.
pub(crate) fn decide(last: Option<&Memory>, parts: &[Part], unmet: usize) -> Decision {
    let now: BTreeSet<Sha256> = parts.iter().flat_map(Part::digests).collect();
    let before = last.map(|m| &m.bullets);
    let added = before.map_or(now.len(), |b| now.difference(b).count());
    let removed = before.map_or(0, |b| b.difference(&now).count());
    let unjudged = parts.iter().any(|p| matches!(p, Part::Unjudged { .. }));
    let held = parts.iter().any(|p| matches!(p, Part::Held { .. }));
    let turn = match (unjudged, held, added) {
        (true, _, _) => Turn::Unjudged,
        (false, false, _) => Turn::Met,
        (false, true, 0) => Turn::Repeat,
        (false, true, _) => Turn::New,
    };
    let said: Vec<&str> = parts.iter().filter_map(Part::said).collect();
    let memory = (turn != Turn::Met).then_some(Memory {
        bullets: now,
        unmet,
    });
    Decision {
        turn,
        added,
        removed,
        list: said.join("\n"),
        memory,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Memory, Part, Turn, bullets, decide, path};

    /// A list as `done` renders one, with `lines` unmet.
    fn list(lines: &[&str]) -> String {
        let mut out = String::from("done: 1 changed file since main (abc1234) in xtask\nunmet:\n");
        for line in lines {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("notes:\n- seams.toml is in the diff\n");
        out
    }

    fn held(lines: &[&str]) -> Part {
        Part::handed(Path::new("/w/t1"), list(lines))
    }

    /// The memory a Stop over `parts` leaves.
    fn after(last: Option<&Memory>, parts: &[Part]) -> Memory {
        decide(last, parts, 0)
            .memory
            .expect("a Stop that holds keeps its memory")
    }

    #[test]
    fn the_bullets_are_the_lines_under_unmet_and_no_note() {
        assert_eq!(bullets(&list(&["a", "b"])), ["a", "b"]);
        assert!(bullets("done could not judge /w/t1: no build").is_empty());
    }

    #[test]
    fn a_first_stop_with_anything_unmet_holds_and_counts_every_bullet_added() {
        let decision = decide(None, &[held(&["a", "b"])], 2);
        assert_eq!(decision.turn, Turn::New);
        assert!(decision.turn.holds());
        assert_eq!((decision.added, decision.removed), (2, 0));
        assert_eq!(decision.memory.map(|m| m.unmet), Some(2));
    }

    #[test]
    fn the_same_list_again_passes_as_a_repeat() {
        let last = after(None, &[held(&["a", "b"])]);
        let decision = decide(Some(&last), &[held(&["a", "b"])], 2);
        assert_eq!(decision.turn, Turn::Repeat);
        assert!(!decision.turn.holds());
        assert_eq!((decision.added, decision.removed), (0, 0));
    }

    #[test]
    fn a_shrunk_list_passes_as_a_repeat_and_counts_what_it_removed() {
        let last = after(None, &[held(&["a", "b"])]);
        let decision = decide(Some(&last), &[held(&["b"])], 1);
        assert_eq!(decision.turn, Turn::Repeat);
        assert_eq!((decision.added, decision.removed), (0, 1));
    }

    #[test]
    fn a_new_bullet_holds_with_the_whole_list() {
        let last = after(None, &[held(&["a", "b"])]);
        let decision = decide(Some(&last), &[held(&["a", "b", "c"])], 3);
        assert_eq!(decision.turn, Turn::New);
        assert_eq!((decision.added, decision.removed), (1, 0));
        assert_eq!(decision.list, list(&["a", "b", "c"]));
    }

    #[test]
    fn a_bullet_that_left_and_came_back_is_new() {
        let first = after(None, &[held(&["a", "b"])]);
        let shrunk = after(Some(&first), &[held(&["a"])]);
        let decision = decide(Some(&shrunk), &[held(&["a", "b"])], 2);
        assert_eq!(decision.turn, Turn::New);
        assert_eq!((decision.added, decision.removed), (1, 0));
    }

    #[test]
    fn one_line_in_another_checkout_is_another_bullet() {
        let last = after(None, &[held(&["a"])]);
        let elsewhere = Part::handed(Path::new("/w/t2"), list(&["a"]));
        let decision = decide(Some(&last), &[held(&["a"]), elsewhere], 2);
        assert_eq!(decision.turn, Turn::New);
        assert_eq!(decision.added, 1);
    }

    #[test]
    fn an_unjudged_checkout_holds_whatever_the_memory() {
        let last = after(None, &[held(&["a"])]);
        let unjudged = Part::Unjudged {
            reason: "done could not judge /w/t2: no build".to_owned(),
        };
        let decision = decide(Some(&last), &[held(&["a"]), unjudged.clone()], 2);
        assert_eq!(decision.turn, Turn::Unjudged);
        assert!(decision.turn.holds());
        assert!(
            decision
                .list
                .ends_with("done could not judge /w/t2: no build")
        );
        // Its second time, too: an unjudged checkout is never a repeat.
        let again = decide(decision.memory.as_ref(), &[unjudged], 1);
        assert_eq!(again.turn, Turn::Unjudged);
        // A handed-back hold no bullet says is one.
        let said = Part::handed(Path::new("/w/t2"), "done could not judge".to_owned());
        assert!(matches!(said, Part::Unjudged { .. }), "{said:?}");
    }

    #[test]
    fn a_met_stop_passes_clears_the_memory_and_counts_what_it_removed() {
        let last = after(None, &[held(&["a", "b"])]);
        let decision = decide(Some(&last), &[Part::Met], 0);
        assert_eq!(decision.turn, Turn::Met);
        assert_eq!((decision.added, decision.removed), (0, 2));
        assert_eq!(decision.memory, None);
        // So the next Stop's list is all new again.
        assert_eq!(decide(None, &[held(&["a"])], 1).turn, Turn::New);
        assert_eq!(decide(None, &[], 0).turn, Turn::Met);
    }

    #[test]
    fn the_memory_is_a_file_per_session_and_agent_and_a_met_stop_removes_it() {
        let dir = crate::scratch::Tree::new("turn-memory").unwrap();
        let root = dir.path();
        let main = path(root, "s/1", None).unwrap();
        let agent = path(root, "s/1", Some("a.b")).unwrap();
        assert_ne!(main, agent);
        assert_eq!(path(root, "s/1", Some("")), Some(main.clone()));
        assert_eq!(path(root, "", None), None, "no session, no memory");
        assert!(main.ends_with("turn/s_1/main.json"), "{}", main.display());
        assert!(
            agent.ends_with("turn/s_1/agent-a_b.json"),
            "{}",
            agent.display()
        );

        let memory = after(None, &[held(&["a"])]);
        Memory::keep(&agent, Some(&memory)).unwrap();
        assert_eq!(Memory::read(&agent), Some(memory));
        assert_eq!(Memory::read(&main), None, "the main loop's is its own");
        Memory::keep(&agent, None).unwrap();
        assert_eq!(Memory::read(&agent), None);
        Memory::keep(&agent, None).unwrap();
    }
}
