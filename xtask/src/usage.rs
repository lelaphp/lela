//! What a session spent, per turn from the plugin, joined to the ticket
//! it landed or left standing (#210; harness-design.md §4.2, §5).
//!
//! `track work` records what a session came to and nothing of what it
//! spent: the session is `claude -p …` with its streams the caller's, so
//! tokens, cost and turns leave with the process. The one live figure is
//! the plugin's: `$.session.usage()` answers the `/cost` ledger and the
//! context window's fill, and `turn.complete` fires once a turn, so a
//! reading per turn is live by construction and the last one is the
//! final figure. One source, then: the plugin pipes what `usage()`
//! answered to `xtask usage record`, which writes a reading of instrument
//! [`INSTRUMENT`] scoped to the branch's ticket where one is recorded and
//! to the session id otherwise, so an interactive session on `main` still
//! shows up. Usage is a reading, not a judgement: a `record` that does not
//! run never blocks the turn, and the reading passes whatever the numbers
//! say.
//!
//! The join is `track work`'s ([`Spent`]): `hold` reads the last usage
//! reading under the ticket's scope and puts `usd`, `tokens` and `turn` on
//! the ticket's `track-work` reading, and the run's own reading carries
//! the `usd` total ([`Total`]). A ticket with no usage reading, a stand-in
//! program in a test, carries none of the three and renders blank, not
//! zero. The last reading is read off the append-only log and not the
//! per-key file: a retired worktree's readings reach the main checkout's
//! store through its log alone (`xtask worktree retire`), and a worktree
//! left standing still holds its own.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::key::Snapshot;
use crate::reading::{Number, Reading, Status, Store};
use crate::ticket;
use crate::track;

/// The instrument's name, the `instrument` field of every reading
/// `record` writes.
pub const INSTRUMENT: &str = "usage";

/// The keys a usage reading carries; the first three are what the join
/// puts on a `track-work` reading.
const USD: &str = "usd";
const TOKENS: &str = "tokens";
const TURN: &str = "turn";
const WINDOW: &str = "window";
const PERCENT: &str = "percent";

/// What `$.session.usage()` answers, as the plugin pipes it: the live
/// context window and, where the host keeps a ledger, what the session
/// has cost. The rate-limit windows ride along in the JSON and are not
/// read; a figure the engine does not have is left out there and stays
/// left out here, never zeroed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionUsage {
    pub context: ContextUsage,
    pub cost: Option<Cost>,
}

/// The status line's `context_window` figures: the tokens the last
/// response was answered over, the model's window, and the one over the
/// other as a whole percentage; `tokens` and `percent` are there once a
/// response came back.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub window: u64,
    pub percent: Option<f64>,
}

/// What the session has cost, as `/cost` totals it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Cost {
    pub usd: f64,
}

impl SessionUsage {
    /// The object the plugin pipes, parsed; a text that is not one is a
    /// refusal naming what was expected.
    pub fn parse(text: &str) -> Result<SessionUsage> {
        serde_json::from_str(text.trim())
            .context("stdin is not a SessionUsage object, `{\"context\": {\"window\": …}, …}`")
    }
}

/// The scope a usage reading is written under: the ticket recorded on
/// `HEAD`'s branch, the scope its `ticket` and `track-work` readings are
/// written under, or `session` where the branch records none. The record
/// alone is read, with no merge base taken, so a track that is missing
/// is not a reason to lose the reading.
pub fn scope_of(root: &Path, session: &str) -> Result<String> {
    Ok(match track::head_ticket(root)? {
        Some(ticket) => ticket::scope(ticket),
        None => session.to_owned(),
    })
}

/// The reading one turn writes: `usd`, `tokens`, `window`, `percent` and
/// `turn` in its numbers, each where `usage` had it, the session on the
/// reading and the window's fill in its message. Stamped over the
/// workspace, which is what a ticket's reading is stamped over, since a
/// turn has no tree of its own. A pass whatever the numbers say: usage is
/// measured, never judged.
pub fn reading(root: &Path, usage: &SessionUsage, session: &str, turn: u64) -> Result<Reading> {
    let scope = scope_of(root, session)?;
    let mut reading = ticket::stamped_over(INSTRUMENT, &scope, &Snapshot::disk(root)?)?;
    reading.status = Status::Pass;
    session.clone_into(&mut reading.session);
    reading.count(TURN, turn)?;
    reading.count(WINDOW, usage.context.window)?;
    if let Some(tokens) = usage.context.tokens {
        reading.count(TOKENS, tokens)?;
    }
    if let Some(percent) = usage.context.percent {
        reading.set(PERCENT, percent);
    }
    if let Some(cost) = &usage.cost {
        reading.set(USD, dollars(cost.usd));
    }
    reading.message = match (usage.context.tokens, usage.context.percent) {
        (Some(tokens), Some(percent)) => format!(
            "turn {turn}: {tokens} of a {} token window, {percent}%",
            usage.context.window
        ),
        _ => format!(
            "turn {turn}: no response yet against a {} token window",
            usage.context.window
        ),
    };
    Ok(reading)
}

/// `xtask usage record --session <id> --turn <n>`: the object on stdin
/// read, one reading written and printed. Recorded as a measurement,
/// which owes no exit.
pub fn record(root: &Path, store: &Store, session: &str, turn: u64) -> Result<()> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
        .context("reading the SessionUsage object from stdin")?;
    let usage = SessionUsage::parse(&text)?;
    let reading = reading(root, &usage, session, turn)?;
    println!("{reading}");
    store.record_measurement(std::slice::from_ref(&reading))
}

/// Dollars to the tenth of a cent, so a sum of them renders as a price
/// and not as a float's tail.
fn dollars(usd: f64) -> f64 {
    (usd * 10_000.0).round() / 10_000.0
}

/// What the last usage reading under a scope said, the three numbers the
/// join carries onto a `track-work` reading; each `None` where the
/// reading had none, so a ledger the host does not keep renders blank.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spent {
    pub usd: Option<f64>,
    pub tokens: Option<i64>,
    pub turn: Option<i64>,
}

impl Spent {
    /// The three numbers of one usage reading.
    pub fn of(reading: &Reading) -> Spent {
        let float = |key: &str| reading.numbers.get(key).map(|n| n.as_f64());
        let int = |key: &str| reading.int(key);
        Spent {
            usd: float(USD),
            tokens: int(TOKENS),
            turn: int(TURN),
        }
    }

    /// The last usage reading under `scope` in the first of `stores` that
    /// holds one, oldest store first: the worktree's own where one still
    /// stands, then the main checkout's, which a retire landed in. Read
    /// off the scope's file and its index ([`Store::last_of`]), and never
    /// the log (#2164). None where no store has one.
    pub fn last(stores: &[&Store], scope: &str) -> Result<Option<Spent>> {
        for store in stores {
            if let Some(reading) = store.last_of(INSTRUMENT, scope)? {
                return Ok(Some(Spent::of(&reading)));
            }
        }
        Ok(None)
    }

    /// The three numbers put on `reading`, each where this has one.
    pub fn carry(self, reading: &mut Reading) {
        let numbers = [
            (USD, self.usd.map(Number::Float)),
            (TOKENS, self.tokens.map(Number::Int)),
            (TURN, self.turn.map(Number::Int)),
        ];
        let had = numbers
            .into_iter()
            .filter_map(|(key, n)| Some((key.to_owned(), n?)));
        reading.numbers.extend(had);
    }
}

/// The run's `usd` total over the tickets it worked: the sum of what each
/// step's usage reading said, and none while no step had one, so a run
/// of stand-ins renders blank.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Total {
    usd: Option<f64>,
}

impl Total {
    /// One step's usage added, where it had a cost.
    pub fn add(&mut self, spent: Option<Spent>) {
        if let Some(usd) = spent.and_then(|s| s.usd) {
            self.usd = Some(dollars(self.usd.unwrap_or(0.0) + usd));
        }
    }

    /// The total as the run's `usd`, where any step had one.
    pub fn carry(self, reading: &mut Reading) {
        if let Some(usd) = self.usd {
            reading.set(USD, usd);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{INSTRUMENT, SessionUsage, Spent, Total, reading, scope_of};
    use crate::git::git;
    use crate::reading::{Number, Reading, Status, Store};
    use crate::scratch::Tree;
    use crate::track::{Ticket, record_ticket};

    /// A repository with one commit on `main`: what a reading is stamped
    /// over, and where a branch records its ticket or not.
    fn repository(name: &str) -> Tree {
        let tree = Tree::new(&format!("usage-{name}")).unwrap();
        git(&tree, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(tree.join("a.txt"), "a\n").unwrap();
        git(&tree, &["add", "--", "a.txt"]).unwrap();
        crate::scratch::committing(&tree, &["commit", "-q", "-m", "scratch"]);
        tree
    }

    /// What the plugin pipes on a priced turn.
    const PRICED: &str = r#"{"context":{"tokens":152000,"window":200000,"percent":76},"rateLimits":[{"window":"five_hour","percent":12}],"cost":{"usd":3.21456}}"#;

    /// On a branch recording a ticket the reading is the ticket's, with the
    /// five numbers, the session and the fill in its message; the usd is
    /// to the tenth of a cent.
    #[test]
    fn a_turn_on_a_branch_with_a_ticket_is_read_under_the_ticket() {
        let tree = repository("ticket");
        git(&tree, &["checkout", "-q", "-b", "t-x/t210"]).unwrap();
        record_ticket(&tree, "t-x/t210", Ticket::new(210)).unwrap();
        let usage = SessionUsage::parse(PRICED).unwrap();
        let reading = reading(&tree, &usage, "3f2a1b0c-session", 14).unwrap();
        assert_eq!(reading.instrument, INSTRUMENT);
        assert_eq!(reading.scope, "210");
        assert_eq!(reading.status, Status::Pass);
        assert_eq!(reading.session, "3f2a1b0c-session");
        assert_eq!(reading.numbers["turn"], Number::Int(14));
        assert_eq!(reading.numbers["tokens"], Number::Int(152_000));
        assert_eq!(reading.numbers["window"], Number::Int(200_000));
        assert_eq!(reading.numbers["percent"], Number::Float(76.0));
        assert_eq!(reading.numbers["usd"], Number::Float(3.2146));
        assert_eq!(
            reading.message,
            "turn 14: 152000 of a 200000 token window, 76%"
        );
        assert!(!reading.key().is_empty(), "stamped over the workspace");
    }

    /// On a branch recording none the reading is the session's, and a
    /// figure `usage()` left out is left out of the numbers: the first
    /// turn, before a response, has no tokens, no percent and no cost.
    #[test]
    fn a_turn_on_a_branch_with_no_ticket_is_read_under_the_session() {
        let tree = repository("session");
        assert_eq!(scope_of(&tree, "an-id").unwrap(), "an-id");
        let usage =
            SessionUsage::parse(r#"{"context":{"window":200000},"rateLimits":[]}"#).unwrap();
        let reading = reading(&tree, &usage, "an-id", 1).unwrap();
        assert_eq!(reading.scope, "an-id");
        assert_eq!(
            reading
                .numbers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["turn", "window"]
        );
        assert_eq!(
            reading.message,
            "turn 1: no response yet against a 200000 token window"
        );
        let spent = Spent::of(&reading);
        assert_eq!(spent.usd, None);
        assert_eq!(spent.tokens, None);
        assert_eq!(spent.turn, Some(1));
    }

    /// A text that is not the object is a refusal naming the shape.
    #[test]
    fn a_text_that_is_not_the_object_is_refused_by_shape() {
        let err = SessionUsage::parse("not json").unwrap_err().to_string();
        assert!(err.contains("SessionUsage"), "{err}");
    }

    /// The join with a usage reading: the last one under the ticket's
    /// scope in the log carries its `usd`, `tokens` and `turn` onto the
    /// ticket's reading, and the run's total sums the usd; the worktree's
    /// store is read before the main checkout's, since a retire moves the
    /// log.
    #[test]
    fn the_join_carries_the_last_reading_under_the_scope_and_the_run_totals_it() {
        let tree = Tree::new("usage-join").unwrap();
        let worktree = Store::at(tree.join("worktree"));
        let track = Store::at(tree.join("track"));
        let mut earlier = Reading::new(INSTRUMENT, "210");
        earlier.numbers.insert("usd".to_owned(), Number::Float(1.0));
        earlier
            .numbers
            .insert("tokens".to_owned(), Number::Int(1000));
        earlier.numbers.insert("turn".to_owned(), Number::Int(1));
        let mut later = earlier.clone();
        later.numbers.insert("usd".to_owned(), Number::Float(3.25));
        later
            .numbers
            .insert("tokens".to_owned(), Number::Int(152_000));
        later.numbers.insert("turn".to_owned(), Number::Int(14));
        let mut other = later.clone();
        "211".clone_into(&mut other.scope);
        other.numbers.insert("usd".to_owned(), Number::Float(0.5));
        track.record(&[earlier, later.clone(), other]).unwrap();
        let mut standing = later.clone();
        standing
            .numbers
            .insert("usd".to_owned(), Number::Float(9.0));
        worktree.record(&[standing]).unwrap();

        let spent = Spent::last(&[&track], "210").unwrap().unwrap();
        assert_eq!(
            spent,
            Spent {
                usd: Some(3.25),
                tokens: Some(152_000),
                turn: Some(14)
            },
            "the last under the scope, not the first and not another scope's"
        );
        let mut step = Reading::new("track-work", "210");
        spent.carry(&mut step);
        assert_eq!(step.numbers["usd"], Number::Float(3.25));
        assert_eq!(step.numbers["tokens"], Number::Int(152_000));
        assert_eq!(step.numbers["turn"], Number::Int(14));

        let first_store_wins = Spent::last(&[&worktree, &track], "210").unwrap().unwrap();
        assert_eq!(first_store_wins.usd, Some(9.0));

        let mut total = Total::default();
        total.add(Some(spent));
        total.add(Spent::last(&[&track], "211").unwrap());
        total.add(None);
        let mut run = Reading::new("track-work", "run");
        total.carry(&mut run);
        assert_eq!(run.numbers["usd"], Number::Float(3.75));
    }

    /// The join without one: a ticket no usage reading names carries none
    /// of the three numbers, and a run of such tickets no total, so both
    /// render blank and not zero.
    #[test]
    fn the_join_without_a_usage_reading_carries_nothing() {
        let tree = Tree::new("usage-none").unwrap();
        let store = Store::at(tree.join("track"));
        let mut unrelated = Reading::new("ticket", "210");
        unrelated
            .numbers
            .insert("usd".to_owned(), Number::Float(4.0));
        store.record(&[unrelated]).unwrap();
        let spent = Spent::last(&[&store], "210").unwrap();
        assert_eq!(spent, None, "another instrument's reading is not usage");
        assert_eq!(
            Spent::last(&[&Store::at(Path::new("/nonexistent"))], "210").unwrap(),
            None
        );
        let mut step = Reading::new("track-work", "210");
        let mut total = Total::default();
        total.add(spent);
        total.carry(&mut step);
        assert!(step.numbers.is_empty(), "{:?}", step.numbers);
    }
}
