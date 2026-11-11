//! A connector's calls are served as the extension that registered it
//! (#2864): a block its factory allocates and drops is named after that
//! extension, not ext/pdo, whose native made the call. The sensor is a
//! debug build's alone.
#![cfg(debug_assertions)]

mod support;

use php_ext::abi::{Dependency, DependencyKind, EngineState, Extension, ExtensionBuilder};
use php_ext::request::Request;
use php_ext::test::{Capture, Engine};
use php_ext::values::PhpString;
use php_ext_pdo::connector::{Connection, Connector, Failure, Prepared, Source};

/// The connector, `leaky:`, whose factory makes a string and drops its
/// owner, faulty on purpose, then opens a connection that does nothing.
#[derive(EngineState)]
struct Leaky;

static LEAKY: Leaky = Leaky;

impl Connector for Leaky {
    fn name(&self) -> &'static str {
        "leaky"
    }

    fn open(&self, req: &mut Request, source: &Source<'_>) -> Result<Box<dyn Connection>, Failure> {
        let _ = source;
        if let Ok(made) = PhpString::new(req, b"dropped") {
            // The fault itself: the owner neither handed on nor given up.
            #[allow(clippy::mem_forget)]
            std::mem::forget(made);
        }
        Ok(Box::new(Quiet))
    }
}

/// One connection of [`Leaky`].
struct Quiet;

impl Connection for Quiet {
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
        true
    }

    fn close(&mut self, req: &mut Request) {
        let _ = req;
    }
}

/// The extension that registers [`Leaky`] with PDO.
fn leaky() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![Dependency {
            name: String::from("pdo"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .register::<&'static dyn Connector>(&LEAKY)
        .build("pdo_leaky")
}

#[test]
fn a_leak_from_a_connector_s_factory_names_its_extension() {
    let engine = Engine::builder()
        .extensions(support::extensions())
        .extensions([leaky()])
        .build();
    let served = engine.serve_capture(
        "<?php $p = new PDO('leaky:x'); echo 'made';",
        Capture::default(),
    );
    let stdout = served.stdout_text();
    assert!(stdout.contains(", allocated by pdo_leaky in "), "{stdout}");
}
