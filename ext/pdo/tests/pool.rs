//! PDO's persistent connections past the engine's pool's bound
//! (ADR-0010's amendment of ADR-0005, #1798): an entry leaves the pool
//! only through its extension, which closes it with the request that is
//! ending, never by a drop no request sees. A connector of the test's own
//! writes a line to a file each time PDO calls its `closer`, so the test
//! reads what was closed without a server.

mod support;

use std::ffi::OsStr;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use php_ext::abi::{Dependency, DependencyKind, EngineState, Extension, ExtensionBuilder};
use php_ext::request::Request;
use php_ext::test::{Capture, Engine, Files};
use php_ext_pdo::connector::{Connection, Connector, Failure, Prepared, Source};

/// The connector, `counted:<path>`: each connection appends `closed` to
/// the file at `<path>` as PDO closes it, and one whose path ends in
/// `dead` answers dead to every `check_liveness`.
#[derive(EngineState)]
struct Counted;

static COUNTED: Counted = Counted;

impl Connector for Counted {
    fn name(&self) -> &'static str {
        "counted"
    }

    fn open(&self, req: &mut Request, source: &Source<'_>) -> Result<Box<dyn Connection>, Failure> {
        let _ = req;
        let log = PathBuf::from(OsStr::from_bytes(source.dsn));
        Ok(Box::new(Link { log }))
    }
}

/// One connection of [`Counted`].
struct Link {
    log: PathBuf,
}

impl Connection for Link {
    fn prepare(
        &mut self,
        req: &mut Request,
        sql: &[u8],
        options: Option<php_ext::values::PhpArray>,
    ) -> Result<Prepared, Failure> {
        let _ = (req, sql, options);
        Err(Failure::unsupported())
    }

    fn exec(&mut self, req: &mut Request, sql: &[u8]) -> Result<i64, Failure> {
        let _ = (req, sql);
        Ok(0)
    }

    /// Every attribute taken, `PDO::ATTR_PERSISTENT` among them, which
    /// PDO hands the connector after the factory as php's constructor does.
    fn set_attribute(
        &mut self,
        req: &mut Request,
        attr: i64,
        value: &php_ext::values::Value,
    ) -> Result<(), Failure> {
        let _ = (req, attr, value);
        Ok(())
    }

    fn check_liveness(&mut self, req: &mut Request) -> bool {
        let _ = req;
        !self.log.to_string_lossy().ends_with("dead")
    }

    fn close(&mut self, req: &mut Request) {
        let _ = req;
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
        {
            log.write_all(b"closed\n").ok();
        }
    }
}

/// The extension that registers [`Counted`] with PDO.
fn counted() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![Dependency {
            name: String::from("pdo"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .register::<&'static dyn Connector>(&COUNTED)
        .build("pdo_counted")
}

/// One request holds 1025 persistent connections, one more than the
/// engine's pool keeps (`dbh::POOLED`): as it ends, 1024 wait in the pool
/// and the one past the bound is closed through the connector's `closer`,
/// once, and the next request finds the 1024 kept and closes nothing more
/// until it, too, gives one back past the bound.
#[test]
fn a_persistent_connection_past_the_pool_s_bound_is_closed_with_the_ending_request() {
    let files = Files::new("pdo-pool", &[]);
    let log = files.path().join("closed.log");
    let engine = Engine::builder()
        .extensions(support::extensions())
        .extensions([counted()])
        .build();
    let script = format!(
        "<?php for ($i = 0; $i < 1025; $i++) {{ \
         new PDO('counted:{}', null, null, [PDO::ATTR_PERSISTENT => \"n$i\"]); }} echo 'done';",
        log.display()
    );
    let served = engine.serve_capture(&script, Capture::default());
    assert_eq!(served.stdout_text(), "done");
    let closed = || std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(closed(), "closed\n");

    let served = engine.serve_capture(&script, Capture::default());
    assert_eq!(served.stdout_text(), "done");
    assert_eq!(closed(), "closed\nclosed\n");
}

/// A persistent connection found dead is closed through the connector's
/// `closer` before the one opened in its place replaces it, as
/// `zend_list_close` closes it in php's constructor (#1957): the one the
/// request holds, found dead by the second constructor, and the one the
/// pool kept from the last request, found dead as it is taken out.
#[test]
fn a_persistent_connection_found_dead_is_closed() {
    let files = Files::new("pdo-dead", &[]);
    let log = files.path().join("closed.dead");
    let engine = Engine::builder()
        .extensions(support::extensions())
        .extensions([counted()])
        .build();
    let script = format!(
        "<?php $o = [PDO::ATTR_PERSISTENT => 'p']; $a = new PDO('counted:{0}', null, null, $o); \
         $b = new PDO('counted:{0}', null, null, $o); echo 'done';",
        log.display()
    );
    let closed = || std::fs::read_to_string(&log).unwrap_or_default();
    let served = engine.serve_capture(&script, Capture::default());
    assert_eq!(served.stdout_text(), "done");
    assert_eq!(closed(), "closed\n");

    let served = engine.serve_capture(&script, Capture::default());
    assert_eq!(served.stdout_text(), "done");
    assert_eq!(closed(), "closed\nclosed\nclosed\n");
}

/// A persistent connection waiting in the engine's pool as the engine
/// ends is closed through the connector's `closer` on the shutdown
/// request, once, as php's `zend_shutdown` destroys `EG(persistent_list)`
/// and `php_pdo_pdbh_dtor` reaches `dbh_free` (#1961): nothing is closed
/// while the engine lives, and the one connection is closed by the drop
/// of its last clone.
#[test]
fn a_pooled_persistent_connection_is_closed_as_the_engine_ends() {
    let files = Files::new("pdo-end", &[]);
    let log = files.path().join("closed.log");
    let engine = Engine::builder()
        .extensions(support::extensions())
        .extensions([counted()])
        .build();
    let script = format!(
        "<?php new PDO('counted:{}', null, null, [PDO::ATTR_PERSISTENT => 'p']); echo 'done';",
        log.display()
    );
    let served = engine.serve_capture(&script, Capture::default());
    assert_eq!(served.stdout_text(), "done");
    let closed = || std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(closed(), "");
    drop(engine);
    assert_eq!(closed(), "closed\n");
}
