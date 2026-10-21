//! The divergence registry a test reads, parsed from the text it states.
//! Each file of `tests/common` is one topic: an integration test includes
//! the topics it calls by name, as `mod common { pub mod registry; }`, and
//! the unit tests under `src` include theirs once, in lib.rs, by `#[path]`.
//! A binary takes a topic whole and may call part of it.
#![allow(
    dead_code,
    reason = "a binary includes the topic whole and calls part of it"
)]

use xtask::divergence::Registry;

/// The registry `text` states; a fixture that does not parse fails the
/// test that wrote it.
pub fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// One `[[divergence]]` row as a test states it (harness §4.3): of the
/// kind its id's prefix names, accepted under a reason a person wrote and
/// since a fixed day; each method changes the one field it names.
#[derive(Debug, Clone)]
pub struct Row {
    id: String,
    reason: String,
    status: String,
    covers: Vec<String>,
}

impl Row {
    /// The row over `id`, accepted.
    pub fn accepted(id: &str) -> Row {
        Row {
            id: id.to_owned(),
            reason: "A person wrote why this one is allowed to differ.".to_owned(),
            status: "accepted".to_owned(),
            covers: Vec::new(),
        }
    }

    /// The row over `id` as `compare`'s caller proposes one: open, with
    /// no reason yet.
    pub fn open(id: &str) -> Row {
        Row::accepted(id).status("open").reason("")
    }

    #[must_use]
    pub fn status(mut self, status: &str) -> Row {
        status.clone_into(&mut self.status);
        self
    }

    #[must_use]
    pub fn reason(mut self, reason: &str) -> Row {
        reason.clone_into(&mut self.reason);
        self
    }

    /// The other ids the row's one reason holds for.
    #[must_use]
    pub fn covers(mut self, ids: &[&str]) -> Row {
        self.covers = ids.iter().map(|&id| id.to_owned()).collect();
        self
    }

    /// The row as `docs/divergence.toml` spells it.
    pub fn toml(&self) -> String {
        let Row {
            id,
            reason,
            status,
            covers,
        } = self;
        let kind = id.split_once(':').map_or("", |(kind, _)| kind);
        let covers = if covers.is_empty() {
            String::new()
        } else {
            let ids: Vec<String> = covers.iter().map(|id| format!("{id:?}")).collect();
            format!("covers = [{}]\n", ids.join(", "))
        };
        format!(
            "[[divergence]]\nid = {id:?}\nkind = {kind:?}\nreason = {reason:?}\nstatus = {status:?}\nsince = \"2026-09-08\"\n{covers}"
        )
    }
}

/// The registry `rows` state, in their order.
pub fn rows(rows: &[Row]) -> Registry {
    registry(&rows.iter().map(Row::toml).collect::<String>())
}
