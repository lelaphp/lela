//! `ops.lock`, the one lock rendered from a file rather than extracted
//! from a crate's sources (design §4.5, seam 3). The `ops` seam's producer
//! is `ops.toml`, the instruction set extracted from php-src at the pinned
//! tag, and its lock is one line per opcode: the name, php's number, the
//! operand kinds of `op1` and `op2`, whether the handler uses `result`,
//! the effects, and the `safepoint` column.
//!
//! The lock is the record of what the compiler, the dispatch loop and the
//! compiled tier agree the instruction set is, so a re-extraction at a new
//! php tag lands as a diff a person reads rather than a silent change of
//! an opcode's number or of whether it may throw. `php_unit`'s `build.rs`
//! renders the same rows into the `Opcode` enum and its tables (ADR-0001);
//! the lock sits beside `unit.lock` in that crate because that is the
//! interface it records.
//!
//! The columns are padded to a fixed width so the file reads as a table
//! and a widened cell moves one line rather than all of them; the last
//! column carries no padding, since [`super::lock::Lock`] trims what it
//! reads and a trailing space would diff against itself.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use toml::Value;

/// The seam producer this module renders a lock from; every other
/// `generated` seam is skipped, naming its producer.
pub const PRODUCER: &str = "ops.toml";

/// What one line of the lock names, for the reading's message.
pub const SUBJECT: &str = "opcodes";

/// The column widths, in order; the `safepoint` column is last and is not
/// padded. `name` is php's longest opcode name, the operands the longest
/// `CONST|TMP|VAR|CV|UNUSED`, and `effects` the four of them joined.
const NAME: usize = 30;
const NUM: usize = 3;
const OPERAND: usize = 23;
const RESULT: usize = 6;
const EFFECTS: usize = 25;

/// The cell an absent column carries, so every line has every column.
const NONE: &str = "-";

/// The lock's lines, rendered from the `[[op]]` rows of the file at
/// `path`.
pub fn lines(path: &Path) -> Result<BTreeSet<String>> {
    let text = crate::read(path)?;
    render(&text).with_context(|| format!("{}", path.display()))
}

/// The lock's lines, rendered from the text of an `ops.toml`.
pub fn render(text: &str) -> Result<BTreeSet<String>> {
    let table: toml::Table = toml::from_str(text).context("ops.toml is not TOML")?;
    let rows = table
        .get("op")
        .and_then(Value::as_array)
        .context("ops.toml has no [[op]] rows")?;
    let mut out = BTreeSet::new();
    for row in rows {
        out.insert(line(row)?);
    }
    Ok(out)
}

/// One `[[op]]` row as its line.
fn line(row: &Value) -> Result<String> {
    let name = row
        .get("name")
        .and_then(Value::as_str)
        .context("an [[op]] row without a name")?;
    let num = row
        .get("num")
        .and_then(Value::as_integer)
        .with_context(|| format!("{name}: an [[op]] row without a num"))?;
    let mut out = String::new();
    write!(out, "{name:<NAME$} {num:>NUM$}").ok();
    write!(out, " {:<OPERAND$}", listed(row, "op1", "|", false)).ok();
    write!(out, " {:<OPERAND$}", listed(row, "op2", "|", false)).ok();
    let result = row.get("result").and_then(Value::as_str).unwrap_or(NONE);
    write!(out, " {result:<RESULT$}").ok();
    write!(out, " {:<EFFECTS$}", listed(row, "effects", ",", true)).ok();
    let safepoint = row.get("safepoint").and_then(Value::as_str).unwrap_or(NONE);
    write!(out, " {safepoint}").ok();
    Ok(out)
}

/// The strings of the row's `key`, joined by `sep`, sorted first where
/// `sorted`: one operand's zval kinds as php's handler line writes them
/// (`|`), or the effects in a fixed order so a re-extraction that lists
/// them differently is not a diff (`,`); `-` for a row that names none.
fn listed(row: &Value, key: &str, sep: &str, sorted: bool) -> String {
    let mut names: Vec<&str> = (row.get(key).and_then(Value::as_array))
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if sorted {
        names.sort_unstable();
    }
    if names.is_empty() {
        NONE.to_owned()
    } else {
        names.join(sep)
    }
}

#[cfg(test)]
mod tests {
    use super::render;

    const OPS: &str = "\
[[op]]
num = 0
name = \"NOP\"
op1 = [\"UNUSED\"]
op2 = [\"UNUSED\"]
result = \"unused\"
effects = []
[[op]]
num = 42
name = \"JMP\"
op1 = [\"UNUSED\"]
op2 = [\"UNUSED\"]
result = \"unused\"
effects = []
safepoint = \"result\"
[[op]]
num = 60
name = \"DO_FCALL\"
op1 = [\"UNUSED\"]
op2 = [\"UNUSED\"]
result = \"used\"
effects = [\"throw\", \"call\"]
";

    #[test]
    fn a_row_renders_as_its_columns_and_the_lines_sort_by_name() {
        let lines: Vec<String> = render(OPS).expect("the rows render").into_iter().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("DO_FCALL "),
            "sorted by name: {lines:?}"
        );
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            [
                "DO_FCALL",
                "60",
                "UNUSED",
                "UNUSED",
                "used",
                "call,throw",
                "-"
            ],
            "the effects are ordered, and the absent safepoint is a cell"
        );
        assert_eq!(
            lines[1].split_whitespace().collect::<Vec<_>>(),
            ["JMP", "42", "UNUSED", "UNUSED", "unused", "-", "result"]
        );
        assert!(lines[2].starts_with("NOP "), "{:?}", lines[2]);
        for line in &lines {
            assert_eq!(line.trim_end(), line, "no line carries trailing space");
        }
    }

    #[test]
    fn a_row_without_a_name_or_a_num_is_named_in_the_error() {
        let no_name = "[[op]]\nnum = 0\n";
        assert!(
            render(no_name)
                .expect_err("a row without a name")
                .to_string()
                .contains("without a name")
        );
        let no_num = "[[op]]\nname = \"NOP\"\n";
        let error = render(no_num).expect_err("a row without a num").to_string();
        assert!(error.contains("NOP"), "{error}");
    }
}
