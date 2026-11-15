//! `xtask new-sapi <name>` (ADR-0008; design §4.1): one host crate laid
//! down at `sapi/<name>` in the topology the CLI and CGI hosts have, as
//! `new-ext` lays an extension down, so the shape is made by the harness and
//! not copied by hand. The crate reaches the engine through the facade
//! alone and the enabled set through the bundle, the `[[edge]]` `sapi/*`
//! permits; its one entry is `main`, which the binary calls; the template
//! host serves the file its line names on one engine of the bundle's
//! enabled set, the request description and the sink on the process's
//! streams, until the host's own replace them, with the bundle's
//! `configure` switches, which its version line and phpinfo read beside
//! the date the engine stamps (`php_sapi::version`); a test runs the
//! binary; the lints the manifest denies under `sapi/*` stand at its root.
//! `sapi/*` is a workspace member by its glob, and the crate joins the
//! members where the root lists them one by one.
//!
//! What the template is exercised on is the fixture at
//! `xtask/fixtures/new-sapi`, whose own `seams.toml` carries the
//! `[[crate]]` row for `sapi/demo`: `cargo test -p xtask --test new_sapi`
//! lays the crate down there and holds it to `budget`, `unsafe-audit`,
//! `api-check` and `clippy`, the last against the fixture's stand-ins for
//! the facade and the bundle, the shipped manifest carrying no row for a
//! name nobody ships.
//!
//! Four refusals, each before anything is written: a name with no
//! `[[crate]]` row (the ceiling is the manifest's, and the row is a
//! person's), a workspace without `crates/php_sapi` (the facade) or without
//! `crates/php_bundle` (the enabled set), and a directory already at
//! `sapi/<name>` (the crate is laid down once and never overwritten).

use std::path::Path;

use anyhow::{Result, bail};

use crate::layout;
use crate::manifest::Manifest;
use crate::new_ext::{BUNDLE, Laid, lint_attributes, subsets, unrefused};
use crate::reading::listed;

/// The facade every host depends on (ADR-0008), relative to the root.
const FACADE: &str = "crates/php_sapi";

/// Lay `sapi/<name>` down with its binary `binary`, `php-<name>` where the
/// line names none, or refuse and write nothing.
pub fn run(root: &Path, manifest: &Manifest, name: &str, binary: Option<&str>) -> Result<Laid> {
    if name.is_empty()
        || !name.starts_with(|c: char| c.is_ascii_lowercase())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        bail!("{name}: a host's name is lowercase ascii letters and digits, as php-src spells it");
    }
    let bin = binary.map_or_else(|| format!("php-{name}"), str::to_owned);
    if bin.is_empty()
        || !bin
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("{bin}: a binary's name is ascii letters, digits, `-` and `_`");
    }
    let row = format!("sapi/{name}");
    let (krate, dir) = unrefused(root, manifest, &row, FACADE, "new-sapi", "host (ADR-0008)")?;
    let package = layout::package(&row);
    // The lints the manifest denies under the row, at both crate roots.
    let lints = lint_attributes(manifest, &row);

    let files = [
        ("Cargo.toml", cargo_toml(name, &package, &bin)),
        ("src/lib.rs", lib_rs(&lints, name, &bin)),
        ("src/main.rs", main_rs(&lints, &package, &bin)),
        ("tests/serve.rs", serve_rs(name, &bin)),
    ];
    let mut laid = Laid::files(root, dir, &row, files)?;

    let owned = subsets(krate);
    laid.notes.push(if owned.is_empty() {
        format!("phpt subset: none; the [[crate]] row for {row} names none")
    } else {
        format!("phpt subset: {} at the pinned tag", listed(&owned))
    });
    laid.notes.push(format!(
        "left: the host itself; {bin} serves the file its line names until the host's own request description and sink replace the template's"
    ));
    Ok(laid)
}

/// The crate's manifest: the facade and the bundle, and the one binary.
fn cargo_toml(name: &str, package: &str, bin: &str) -> String {
    format!(
        "# sapi/{name}: php-src/sapi/{name}, one crate under `sapi/` (ADR-0008), laid\n\
         # down by `xtask new-sapi`: the binary `{bin}`. Two edges, php_bundle and\n\
         # php_sapi; the engine is named through the facade alone.\n\
         [package]\n\
         name = \"{package}\"\n\
         version = \"0.0.0\"\n\
         edition.workspace = true\n\
         license.workspace = true\n\
         rust-version.workspace = true\n\
         publish = false\n\
         description = \"php's {name} host, the binary {bin}\"\n\
         \n\
         [lints]\n\
         workspace = true\n\
         \n\
         [[bin]]\n\
         name = \"{bin}\"\n\
         path = \"src/main.rs\"\n\
         \n\
         [dependencies]\n\
         php_bundle = {{ path = \"../../{BUNDLE}\" }}\n\
         php_sapi = {{ path = \"../../{FACADE}\" }}\n"
    )
}

/// The crate root: the host's name, its `main` and the template's sink.
fn lib_rs(lints: &str, name: &str, bin: &str) -> String {
    format!(
        "//! php's `{name}` host, `php-src/sapi/{name}`, as one crate under `sapi/`\n\
         //! (ADR-0008): the binary `{bin}`, reaching the engine through `php_sapi`\n\
         //! alone. A host builds a request description from what it was given,\n\
         //! supplies a sink for what the request writes, and crosses `serve`\n\
         //! between them (design §4.3 seam 4).\n\
         #![forbid(unsafe_code)]\n\
         {lints}\
         \n\
         use std::io::Write;\n\
         use std::process::ExitCode;\n\
         \n\
         use php_sapi::{{Engine, HostServes, OutputSink, RequestDescription, ResponseHead}};\n\
         \n\
         /// `sapi_module.name`: what `php_sapi_name()` answers.\n\
         pub const NAME: &str = \"{name}\";\n\
         \n\
         /// The host's `main`: the file the line names served on one engine of\n\
         /// the bundle's enabled set with the bundle's tier bound, and the\n\
         /// process ending with the code the request ended with.\n\
         #[must_use]\n\
         pub fn main() -> ExitCode {{\n\
         \x20   let Some(file) = std::env::args_os().nth(1) else {{\n\
         \x20       std::io::stderr()\n\
         \x20           .lock()\n\
         \x20           .write_all(b\"Usage: {bin} <file>\\n\")\n\
         \x20           .ok();\n\
         \x20       return php_sapi::exit_code(1);\n\
         \x20   }};\n\
         \x20   let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();\n\
         \x20   let builder = Engine::builder()\n\
         \x20       .sapi_name(NAME)\n\
         \x20       .extensions(&extensions)\n\
         \x20       .configure(php_bundle::CONFIGURE)\n\
         \x20       .tier::<php_bundle::Tier>();\n\
         \x20   let engine = php_sapi::start(builder, &[], |entry| Streams.log(entry));\n\
         \x20   let description = RequestDescription {{\n\
         \x20       argv: vec![file.as_encoded_bytes().to_vec()],\n\
         \x20       cwd: std::env::current_dir().unwrap_or_default(),\n\
         \x20       sapi_name: NAME.to_owned(),\n\
         \x20       script_name: Some(file.as_encoded_bytes().to_vec()),\n\
         \x20       ..RequestDescription::default()\n\
         \x20   }};\n\
         \x20   let (served, _) = engine.serve(description, Box::new(Streams), || HostServes::Program);\n\
         \x20   php_sapi::exit_code(served.exit_code)\n\
         }}\n\
         \n\
         /// The process's two streams: the body on stdout, no header block, and\n\
         /// the log and the error stream on stderr, each log entry with `\\n`\n\
         /// after it.\n\
         struct Streams;\n\
         \n\
         impl OutputSink for Streams {{\n\
         \x20   fn write(&mut self, bytes: &[u8]) -> Result<(), php_sapi::Aborted> {{\n\
         \x20       std::io::stdout().lock().write_all(bytes).ok();\n\
         \x20       Ok(())\n\
         \x20   }}\n\
         \n\
         \x20   fn flush(&mut self) {{\n\
         \x20       std::io::stdout().lock().flush().ok();\n\
         \x20   }}\n\
         \n\
         \x20   fn send_headers(&mut self, _head: &ResponseHead) -> Result<(), php_sapi::Aborted> {{\n\
         \x20       Ok(())\n\
         \x20   }}\n\
         \n\
         \x20   fn log(&mut self, entry: &[u8]) {{\n\
         \x20       let mut stderr = std::io::stderr().lock();\n\
         \x20       stderr.write_all(entry).ok();\n\
         \x20       stderr.write_all(b\"\\n\").ok();\n\
         \x20   }}\n\
         \n\
         \x20   fn write_stderr(&mut self, bytes: &[u8]) {{\n\
         \x20       std::io::stderr().lock().write_all(bytes).ok();\n\
         \x20   }}\n\
         }}\n"
    )
}

/// The binary: the host's `main`, and nothing else, under the lints the
/// library's root carries too.
fn main_rs(lints: &str, package: &str, bin: &str) -> String {
    format!(
        "//! `{bin}`, the binary (design §4.1; ADR-0008): [`{package}::main`].\n\
         #![forbid(unsafe_code)]\n\
         {lints}\
         \n\
         use std::process::ExitCode;\n\
         \n\
         fn main() -> ExitCode {{\n\
         \x20   {package}::main()\n\
         }}\n"
    )
}

/// The integration test: the binary serves the file its line names,
/// under the host's name.
fn serve_rs(name: &str, bin: &str) -> String {
    format!(
        "//! The binary serves the file its line names, under the host's name.\n\
         \n\
         use std::process::Command;\n\
         \n\
         #[test]\n\
         fn the_file_the_line_names_is_served_under_the_host_s_name() {{\n\
         \x20   let file = std::env::temp_dir().join(format!(\"sapi-{name}-{{}}.php\", std::process::id()));\n\
         \x20   std::fs::write(&file, \"<?php echo PHP_SAPI, \\\"\\\\n\\\";\").unwrap();\n\
         \x20   let out = Command::new(env!(\"CARGO_BIN_EXE_{bin}\"))\n\
         \x20       .arg(&file)\n\
         \x20       .output()\n\
         \x20       .unwrap();\n\
         \x20   std::fs::remove_file(&file).ok();\n\
         \x20   assert_eq!(String::from_utf8_lossy(&out.stdout), \"{name}\\n\");\n\
         \x20   assert!(out.status.success(), \"{{out:?}}\");\n\
         }}\n"
    )
}
