//! The kind enum, generated from `php.ungram` beside this file (design
//! §4.9, ADR-0003): one `SyntaxKind` per token and per node of the tree,
//! so the lexer, the parser and the compiler cannot disagree on a kind.
//! The schema is read by `codegen::ungram`, validated, and emitted by
//! `codegen::kind` into `OUT_DIR/syntax_kind.rs`, which `lib.rs` includes
//! and nothing commits (design §10). A schema that does not parse, or a
//! symbol the punctuation table does not name, is a build error that names
//! its line.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// One body, two readers: `tests/schema.rs` includes the same module for
// the counts and the spot checks, and the typed-node emitter that follows
// this one reads `punct_variant` and `snake_case`; what this build does
// not call is not dead.
#[allow(dead_code)]
#[path = "codegen/mod.rs"]
mod codegen;

use codegen::ungram::Error;

fn main() {
    let schema = Path::new(env!("CARGO_MANIFEST_DIR")).join("php.ungram");
    println!("cargo:rerun-if-changed={}", schema.display());
    println!("cargo:rerun-if-changed=codegen");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap_or_default()).join("syntax_kind.rs");
    match generate(&schema) {
        Ok(text) => {
            if let Err(e) = fs::write(&out, text) {
                println!("cargo:warning=writing {}: {e}", out.display());
                std::process::exit(1);
            }
        }
        Err(errors) => {
            for e in errors {
                println!("cargo:warning=php.ungram: {e}");
            }
            std::process::exit(1);
        }
    }
}

/// The text of `syntax_kind.rs` for the schema at `path`, or every error
/// the schema has, each with its line where it has one.
fn generate(path: &Path) -> Result<String, Vec<Error>> {
    let text = fs::read_to_string(path)
        .map_err(|e| vec![Error::global(format!("reading {}: {e}", path.display()))])?;
    let grammar = codegen::ungram::parse(&text)?;
    let errors = codegen::ungram::validate(&grammar);
    if !errors.is_empty() {
        return Err(errors);
    }
    codegen::kind::generate(&grammar)
}
