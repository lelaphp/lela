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
/// not: what the diagnostics tests and units.rs's cached unit echo to
/// raise php's warning.
fn declaring_nan(builder: EngineBuilder) -> EngineBuilder {
    use crate::api::__private::NativeConst;
    use php_unit::decl::ConstExpr;
    builder.additional_extension(
        crate::api::abi::ExtensionBuilder::default()
            .constants(&[
                NativeConst {
                    name: b"INF",
                    value: || ConstExpr::Double(f64::INFINITY),
                    attributes: &[],
                },
                NativeConst {
                    name: b"NAN",
                    value: || ConstExpr::Double(f64::NAN),
                    attributes: &[],
                },
            ])
            .build("standard"),
    )
}
