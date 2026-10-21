//! A manifest a test builds rather than edits as text: [`Tracked`] is a
//! `seams.toml`, the workspace's or a fixture's, held as a TOML table and
//! changed by the row, rule or table a test names, so a person's edit to
//! a row the test does not name never turns it red; [`pin`] is the oracle
//! pin read straight from `oracle.toml`, for a test that needs the pin and
//! not the manifest. A test whose subject is the tracked file itself reads
//! it with `Manifest::load` and sits in a `mod tracked` of its own file.
//! Each file of `tests/common` is one topic: an integration test includes
//! the topics it calls by name, as `mod common { pub mod manifest; }`, and
//! the unit tests under `src` include theirs once, in lib.rs, by `#[path]`.
//! A binary takes a topic whole and may call part of it.
#![allow(
    dead_code,
    reason = "a binary includes the topic whole and calls part of it"
)]

use std::fs;
use std::path::Path;

use serde::Deserialize;
use toml::{Table, Value};
use xtask::manifest::{self, Crate, Manifest};
use xtask::oracle::Pin;

/// A `seams.toml` as a table, changed row by row and written whole.
#[derive(Debug, Clone)]
pub struct Tracked {
    table: Table,
}

impl Tracked {
    /// The workspace's own `seams.toml`.
    pub fn workspace() -> Tracked {
        Tracked::of(workspace())
    }

    /// The `seams.toml` standing at `root`.
    pub fn of(root: &Path) -> Tracked {
        let path = root.join(manifest::FILE);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        Tracked {
            table: text
                .parse()
                .unwrap_or_else(|err| panic!("{} is no table: {err}", path.display())),
        }
    }

    /// The `[[crate]]` row `name` as `edit` leaves it, a new row by that
    /// name where the file has none.
    pub fn with_crate(mut self, name: &str, edit: impl FnOnce(&mut Crate)) -> Tracked {
        let row = self.row("crate", "name", name);
        let mut parsed = if row.is_empty() {
            Crate::named(name)
        } else {
            Crate::deserialize(Value::Table(row.clone())).unwrap()
        };
        edit(&mut parsed);
        *row = Table::try_from(&parsed).unwrap();
        self
    }

    /// The `[[rule]]` row `id` as `edit` leaves it, a new row where the
    /// file has none; the row keeps its `id` whatever the edit does.
    pub fn with_rule(mut self, id: &str, edit: impl FnOnce(&mut Table)) -> Tracked {
        let row = self.row("rule", "id", id);
        edit(row);
        row.insert("id".to_owned(), Value::from(id));
        self
    }

    /// `row` added to the array of tables `array`, `[[bench]]` or any
    /// other the grammar reads.
    pub fn with_row(mut self, array: &str, row: Table) -> Tracked {
        self.table
            .entry(array)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap()
            .push(Value::Table(row));
        self
    }

    /// The top-level table `key` replaced by `table`, or taken out for
    /// `None`.
    pub fn with_table(mut self, key: &str, table: Option<Table>) -> Tracked {
        match table {
            Some(table) => self.table.insert(key.to_owned(), Value::Table(table)),
            None => self.table.remove(key),
        };
        self
    }

    /// `[oracle] pin` naming `path`, relative to the root the manifest is
    /// written at, or absolute.
    pub fn with_pin(mut self, path: &str) -> Tracked {
        let oracle = self
            .table
            .entry("oracle")
            .or_insert_with(|| Value::Table(Table::new()));
        oracle
            .as_table_mut()
            .unwrap()
            .insert("pin".to_owned(), Value::from(path));
        self
    }

    /// The file as text, as [`Tracked::write`] writes it.
    pub fn text(&self) -> String {
        toml::to_string(&self.table).unwrap()
    }

    /// The file written at `root`, and the manifest it loads as there.
    pub fn write(&self, root: &Path) -> Manifest {
        fs::write(root.join(manifest::FILE), self.text()).unwrap();
        Manifest::load(root)
            .unwrap_or_else(|err| panic!("the manifest written at {}: {err:#}", root.display()))
    }

    /// The row of the array of tables `array` whose `key` is `name`: an
    /// empty one at the end where the file has none.
    fn row(&mut self, array: &str, key: &str, name: &str) -> &mut Table {
        let rows = self
            .table
            .entry(array)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap();
        let at = rows
            .iter()
            .position(|row| row.get(key).and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| {
                rows.push(Value::Table(Table::new()));
                rows.len() - 1
            });
        rows[at].as_table_mut().unwrap()
    }
}

/// The workspace's oracle pin, read from `oracle.toml` itself and not
/// through the manifest that names it.
pub fn pin() -> Pin {
    Pin::load(&workspace().join("oracle.toml")).unwrap()
}

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}
