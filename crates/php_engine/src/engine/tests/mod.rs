//! engine.rs's tests, one file per concern, and the fixtures they
//! share: the CLI's request description of a source, a request served
//! into a kept sink, and a builder declaring `INF` and `NAN`.

use super::*;

use crate::api::request::Kept;
use crate::test::Files;

mod diagnostics;
mod execution;
mod hosts;
mod includes;
mod interning;
mod startup;
mod units;

fn description(source: &str) -> RequestDescription {
    RequestDescription {
        sapi_name: String::from("cli"),
        script_name: Some(b"witness.php".to_vec()),
        source: Some(source.as_bytes().to_vec()),
        ..RequestDescription::default()
    }
}

fn served(engine: &Engine, source: &str) -> (String, Observables) {
    let mut sink = Kept::default();
    let observables = engine.serve_into(description(source), &mut sink);
    (sink.body(), observables)
}

/// `builder` with `INF` and `NAN` among its persistent constants, as
/// ext/standard declares them, which the bundle of these tests has
/// not: what the diagnostics tests echo to raise php's warning.
fn declaring_nan(builder: EngineBuilder) -> EngineBuilder {
    let constant = |name: &str, value: f64| php_unit::decl::ConstDecl {
        name: Name::from(name),
        value: php_unit::decl::ConstExpr::Double(value),
        attributes: Vec::new(),
    };
    builder.additional_extension(
        crate::api::abi::ExtensionBuilder::default()
            .constants(vec![
                constant("INF", f64::INFINITY),
                constant("NAN", f64::NAN),
            ])
            .build("standard"),
    )
}
