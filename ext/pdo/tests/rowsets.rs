//! `closeCursor()`'s emulation for a connector with no `cursor_closer`,
//! which no bundled connector reaches: `pdo_mysql` and `pdo_sqlite` both have
//! one. A stub connector, `rowsets:`, has `next_rowset` and no closer,
//! and its statement holds three rowsets of two rows each. php's loop
//! (`ext/pdo/pdo_stmt.c:1923-1938` at php-8.5.12) fetches every row of
//! every rowset through `pdo_stmt_do_next_rowset` and clears no error,
//! because its `PDO_STMT_CLEAR_ERR` comes after the emulation (#3391).

mod support;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder};
use php_ext::request::Request;
use php_ext::test::{Capture, Engine};
use php_ext::values::{Owner, PhpArray, PhpString, Value};
use php_ext_pdo::connector::{
    BindKind, Column, Connection, Connector, Executed, Failure, Orientation, Placeholders,
    Prepared, Rowsets, Source, Statement,
};

/// The connector, `rowsets:`.
struct Stub;

static STUB: Stub = Stub;

impl Connector for Stub {
    fn name(&self) -> &'static str {
        "rowsets"
    }

    fn open(&self, req: &mut Request, source: &Source<'_>) -> Result<Box<dyn Connection>, Failure> {
        let _ = (req, source);
        Ok(Box::new(Open))
    }
}

/// One connection of [`Stub`].
struct Open;

impl Connection for Open {
    fn prepare(
        &mut self,
        req: &mut Request,
        sql: &[u8],
        options: Option<PhpArray>,
    ) -> Result<Prepared, Failure> {
        let _ = (req, sql, options);
        Ok(Prepared {
            statement: Box::new(Three::default()),
            placeholders: Placeholders::Native {
                named: true,
                positional: true,
            },
            parameter_map: None,
        })
    }

    fn exec(&mut self, req: &mut Request, sql: &[u8]) -> Result<i64, Failure> {
        let _ = (req, sql);
        Ok(0)
    }

    fn set_attribute(
        &mut self,
        req: &mut Request,
        attr: i64,
        value: &Value,
    ) -> Result<(), Failure> {
        let _ = (req, attr, value);
        Ok(())
    }
}

/// Rowsets of a statement of [`Stub`].
const ROWSETS: usize = 3;
/// Rows of each rowset.
const ROWS: usize = 2;

/// A statement of three rowsets of two rows; the value of a row is its
/// rowset times ten plus its row, counted from one.
#[derive(Default)]
struct Three {
    rowset: usize,
    row: usize,
}

impl Statement for Three {
    fn execute(
        &mut self,
        req: &mut Request,
        executed: bool,
        query: &[u8],
    ) -> Result<Executed, Failure> {
        let _ = (req, executed, query);
        (self.rowset, self.row) = (0, 0);
        Ok(Executed {
            columns: 1,
            rows: None,
        })
    }

    fn fetch(
        &mut self,
        req: &mut Request,
        orientation: Orientation,
        offset: i64,
    ) -> Result<bool, Failure> {
        let _ = (req, orientation, offset);
        if self.row == ROWS {
            return Ok(false);
        }
        self.row += 1;
        Ok(true)
    }

    fn describe(&mut self, req: &mut Request, column: usize) -> Result<Column, Failure> {
        let _ = column;
        Ok(Column {
            name: PhpString::new(req, b"c")?,
            max_len: 0,
            precision: 0,
        })
    }

    fn value(
        &mut self,
        req: &mut Request,
        column: usize,
        ty: Option<BindKind>,
    ) -> Result<Owner<Value>, Failure> {
        let _ = (req, column, ty);
        let value = self.rowset * 10 + self.row;
        Ok(Owner::int(i64::try_from(value).unwrap_or(0)))
    }

    fn rowsets(&mut self) -> Option<&mut dyn Rowsets> {
        Some(self)
    }
}

impl Rowsets for Three {
    fn next_rowset(&mut self, req: &mut Request) -> Result<Executed, Failure> {
        let _ = req;
        // No rowset after the last, and the error code left as it was,
        // as a connector slot answering `0` with no error function leaves it.
        if self.rowset + 1 == ROWSETS {
            return Err(Failure::silent());
        }
        (self.rowset, self.row) = (self.rowset + 1, 0);
        Ok(Executed {
            columns: 1,
            rows: None,
        })
    }
}

/// The extension that registers [`Stub`] with PDO.
fn stub() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![Dependency {
            name: String::from("pdo"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .register::<&'static dyn Connector>(&STUB)
        .build("pdo_rowsets")
}

/// What `source` prints under an engine with [`stub`].
fn served(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(support::extensions())
        .extensions([stub()])
        .build();
    engine
        .serve_capture(source, Capture::default())
        .stdout_text()
}

#[test]
fn close_cursor_without_a_closer_walks_every_rowset_and_keeps_the_error() {
    // No ext/standard here, so no var_dump: `b()` prints a bool.
    let stdout = served(concat!(
        "<?php\n",
        "function b($v) { return $v ? 'true' : 'false'; }\n",
        "$p = new PDO('rowsets:x', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]);\n",
        "$s = $p->prepare('x'); $s->execute();\n",
        "echo $s->fetchColumn(), ' ', b($s->nextRowset()), ' ', $s->fetchColumn(), \"\\n\";\n",
        "echo b(@$s->getAttribute(PDO::ATTR_CURSOR_NAME)), ' ', $s->errorCode(), \"\\n\";\n",
        "echo b($s->closeCursor()), ' ', $s->errorCode(), \"\\n\";\n",
        "echo b($s->nextRowset()), ' ', $s->errorCode(), \"\\n\";\n",
        "$s->execute(); $s->closeCursor(); $s->execute();\n",
        "echo $s->fetchColumn(), \"\\n\";\n",
    ));
    assert_eq!(
        stdout,
        concat!(
            "1 true 11\n",
            "false IM001\n",
            "true IM001\n",
            "false 00000\n",
            "1\n",
        ),
        "{stdout}"
    );
}
