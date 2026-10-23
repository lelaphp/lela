//! The reply read at the Stop for what the close did not name (#32). The
//! structural conditions on a close, every box ticked (#28) and every
//! successor named or `--opens-nothing` asserted (#29), let one case past:
//! every box ticked, nothing asserted opened, and the reply that ends the
//! turn hands two things to a person. The one place that sees the reply is
//! the Stop hook, whose stdin carries `last_assistant_message` beside
//! `stop_hook_active` (harness-design.md §13 D.16), and `done --hook`
//! already reads that stdin.
//!
//! Whether this session closed a ticket is a query over the store and not
//! a guess: a `ticket` reading of a close carries the session the call
//! named, the way a `permit` reading carries the hook's `session_id`, and
//! a session that closed nothing has its reply read by nothing here. A
//! close whose reading names no session is nobody's and is not read
//! either, since holding every later session to it would block each of
//! them once. And a close is this turn's while no Stop of this session
//! has passed since it: the `done` reading a Stop writes over the
//! workspace carries the session, and a close taken before this session's
//! latest one was read by that Stop already, so a turn three later is not
//! held to it.
//!
//! The word list is the sensor and lives where the rules do: a manifest
//! row with `tool = "Stop"` and a `deny_pattern` over the reply, sentence
//! by sentence, with the text the block prints (`Rule::at_the_stop`).
//! A sentence that names a ticket number other than the one closed is
//! naming a filed thing, which is the behaviour this wants, and does not
//! fire whatever else it says. This is a heuristic beside the structural
//! conditions: a false positive costs one turn, since the second Stop
//! passes on `stop_hook_active` and the recovery is to file the ticket and
//! stop again; a false negative is the state of the world without it.

use std::path::Path;

use anyhow::{Context, Result};
use regex_lite::Regex;

use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::reading::{Reading, Status, Store, epoch_seconds, joined};
use crate::ticket;
use crate::track::Ticket;

/// What the reply read at the Stop was found to carry: the rule that
/// fired, the tickets this session closed, and the sentences that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unfiled {
    pub(crate) rule: String,
    pub(crate) text: String,
    pub(crate) closed: Vec<Ticket>,
    pub(crate) sentences: Vec<String>,
}

impl Unfiled {
    /// The tickets closed, as they are cited: `#28, #29`.
    fn closed_cited(&self) -> String {
        joined(&self.closed, ", ")
    }

    /// The block's reason: the rule's text, what this session closed, the
    /// sentences that read as work left unfiled, and what to do.
    pub(crate) fn render(&self) -> String {
        let closed = self.closed_cited();
        let sentences = joined(self.sentences.iter().map(|s| format!("- {s:?}")), "\n");
        format!(
            "{} ({})\nThis session closed {closed}; its last reply reads as work left unfiled, in these sentences:\n{sentences}\nFile each as a ticket, `ready-for-agent` or `for-a-person`, and name its number in the reply, or say which ticket, divergence row or proposed rule already holds it; then stop again. A heuristic: the second Stop passes.\n",
            self.text, self.rule
        )
    }

    /// The reading: `done` over the rule, carrying the session, the
    /// sentences counted, failed.
    pub(crate) fn reading(&self, snapshot: &Snapshot, session_id: &str) -> Result<Reading> {
        let mut reading = Reading::stamped_over(
            crate::done::INSTRUMENT,
            &self.rule,
            snapshot,
            Path::new("."),
        )?;
        reading.rule.clone_from(&self.rule);
        reading.status = Status::Fail;
        reading.count("closed", self.closed.len() as u64)?;
        reading.count("sentences", self.sentences.len() as u64)?;
        session_id.clone_into(&mut reading.session);
        reading.message = format!(
            "the reply after closing {} reads as work left unfiled in {} sentence{}: {}",
            self.closed_cited(),
            self.sentences.len(),
            if self.sentences.len() == 1 { "" } else { "s" },
            self.sentences.join(" | ")
        );
        Ok(reading)
    }
}

/// The tickets `session_id` closed this turn, as the store holds them:
/// the latest `ticket` reading per ticket that is a close (it counts
/// `opens`), passed, names this session, and is no older than the
/// session's latest Stop in the log, the `done` reading over the
/// workspace that carries the session; a close the previous Stop already
/// read is not this turn's. An empty session names nobody and matches
/// none.
pub(crate) fn closed_by(store: &Store, session_id: &str) -> Result<Vec<Ticket>> {
    if session_id.is_empty() {
        return Ok(Vec::new());
    }
    let previous_stop = store
        .history(crate::done::INSTRUMENT)?
        .readings
        .iter()
        .filter(|r| {
            r.instrument == crate::done::INSTRUMENT
                && r.scope == crate::done::WORKSPACE
                && r.session == session_id
        })
        .map(|r| epoch_seconds(&r.at))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max();
    let mut closed: Vec<Ticket> = store
        .readings()?
        .readings
        .into_iter()
        .filter(|r| {
            r.instrument == ticket::INSTRUMENT
                && r.status == Status::Pass
                && r.numbers.contains_key("opens")
                && r.session == session_id
        })
        .filter(|r| {
            previous_stop.is_none_or(|stop| epoch_seconds(&r.at).is_ok_and(|at| at >= stop))
        })
        .filter_map(|r| r.scope.parse().ok().map(Ticket::new))
        .collect();
    closed.sort_by_key(|t| t.number());
    closed.dedup();
    Ok(closed)
}

/// Read the reply against every rule at the Stop, for a session that
/// closed `closed`: the first rule whose pattern matches a sentence, with
/// every sentence it matched. `None` for a session that closed nothing,
/// whose reply is not read at all, and for a reply no rule fires on.
pub(crate) fn read(manifest: &Manifest, closed: &[Ticket], reply: &str) -> Result<Option<Unfiled>> {
    if closed.is_empty() {
        return Ok(None);
    }
    let numbered = ticket_number();
    for rule in manifest.rules.iter().filter(|r| r.at_the_stop()) {
        let Some(pattern) = &rule.predicate.deny_pattern else {
            continue;
        };
        let regex = Regex::new(pattern)
            .with_context(|| format!("rule \"{}\": deny_pattern is not a regex", rule.id))?;
        let sentences: Vec<String> = sentences(reply)
            .into_iter()
            .filter(|s| !names_another_ticket(&numbered, s, closed))
            .filter(|s| regex.is_match(s))
            .map(str::to_owned)
            .collect();
        if !sentences.is_empty() {
            return Ok(Some(Unfiled {
                rule: rule.id.clone(),
                text: rule.text.clone(),
                closed: closed.to_vec(),
                sentences,
            }));
        }
    }
    Ok(None)
}

/// The sentences of a reply: split at a line end and after `.`, `!` or
/// `?` followed by whitespace, each trimmed of the list marker and the
/// whitespace around it, the empty ones dropped. The `.` of a numbered
/// list's marker, `1.` with nothing but digits before it, ends nothing.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut start = 0;
        let bytes = line.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            // `.` is ASCII, so `i` is a char boundary; the slice is taken
            // for a stop alone, never inside a wider character.
            let marker = *b == b'.' && {
                let so_far = line[start..i].trim();
                !so_far.is_empty() && so_far.bytes().all(|d| d.is_ascii_digit())
            };
            let ends = matches!(b, b'.' | b'!' | b'?')
                && !marker
                && bytes.get(i + 1).is_none_or(u8::is_ascii_whitespace);
            if ends {
                out.push(&line[start..=i]);
                start = i + 1;
            }
        }
        out.push(&line[start..]);
    }
    let numbered = Regex::new(r"^\d+\.\s+")
        .unwrap_or_else(|err| unreachable!("the list marker pattern: {err}"));
    out.into_iter()
        .map(|s| {
            let s = s.trim();
            let s = ["- ", "* ", "+ "]
                .iter()
                .find_map(|m| s.strip_prefix(m))
                .unwrap_or(s);
            numbered.find(s).map_or(s, |m| &s[m.end()..]).trim()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// A ticket cited by number, `#40`.
fn ticket_number() -> Regex {
    Regex::new(r"#(\d+)\b").unwrap_or_else(|err| unreachable!("the ticket number pattern: {err}"))
}

/// Whether the sentence names a ticket by number other than one closed:
/// `#40` in a reply that closed `#28` names a filed thing.
fn names_another_ticket(numbered: &Regex, sentence: &str, closed: &[Ticket]) -> bool {
    numbered.captures_iter(sentence).any(|c| {
        c.get(1)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .is_some_and(|n| !closed.contains(&Ticket::new(n)))
    })
}

#[cfg(test)]
mod tests {
    use super::{names_another_ticket, read, sentences, ticket_number};
    use crate::manifest::{Manifest, OneOrMany, Rule};
    use crate::track::Ticket;

    fn manifest_with(pattern: &str) -> Manifest {
        let mut manifest = Manifest::named("xtask");
        let mut rule = Rule::named("ticket.reply-unfiled", "");
        rule.scope = OneOrMany::None;
        rule.tool = OneOrMany::One("Stop".to_owned());
        rule.predicate.deny = None;
        rule.predicate.deny_pattern = Some(pattern.to_owned());
        "the text".clone_into(&mut rule.text);
        manifest.rules.push(rule);
        manifest
    }

    #[test]
    fn a_reply_is_split_into_sentences_at_line_ends_and_after_stops_before_whitespace() {
        assert_eq!(
            sentences(
                "Done. Two things.\n- One is a person's!\n* v1.2 stands? Yes\n1. First. 2. Second\n\n"
            ),
            [
                "Done.",
                "Two things.",
                "One is a person's!",
                "v1.2 stands?",
                "Yes",
                "First.",
                "Second"
            ]
        );
    }

    #[test]
    fn a_sentence_naming_a_ticket_other_than_the_one_closed_names_a_filed_thing() {
        let closed = [Ticket::new(28)];
        let numbered = ticket_number();
        assert!(names_another_ticket(&numbered, "filed as #40.", &closed));
        assert!(!names_another_ticket(
            &numbered,
            "#28's rows are a person's.",
            &closed
        ));
        assert!(!names_another_ticket(&numbered, "no number here", &closed));
    }

    /// The sentences that fire are named, one that names a filed ticket
    /// does not, a session that closed nothing is not read at all, and the
    /// reading counts them under the rule.
    #[test]
    fn the_reply_of_a_session_that_closed_a_ticket_is_read_and_of_one_that_did_not_is_not() {
        let manifest = manifest_with(r"(?i)\b(left for a person|worth filing)\b");
        let closed = [Ticket::new(28)];
        let reply = "The close went through. Left for a person: the rows. Filed the rest as #40, worth filing. Nothing else.";
        let found = read(&manifest, &closed, reply).unwrap().unwrap();
        assert_eq!(found.sentences, ["Left for a person: the rows."]);
        assert_eq!(found.rule, "ticket.reply-unfiled");
        assert!(read(&manifest, &[], reply).unwrap().is_none());
        assert!(
            read(&manifest, &closed, "Every box ticked; closed.")
                .unwrap()
                .is_none()
        );
        let rendered = found.render();
        assert!(
            rendered.starts_with("the text (ticket.reply-unfiled)\n"),
            "{rendered}"
        );
        assert!(rendered.contains("This session closed #28;"), "{rendered}");
        assert!(
            rendered.contains("- \"Left for a person: the rows.\""),
            "{rendered}"
        );
        assert!(rendered.contains("the second Stop passes"), "{rendered}");
    }
}
