//! The kind enum, the typed nodes and the display table, generated from
//! `php.ungram` beside this file (design §4.9, ADR-0003): one `SyntaxKind`
//! per token and per node of the tree, so the lexer, the parser and the
//! compiler cannot disagree on a kind; one struct per node kind with named
//! child fields, so a read of the tree is a field read; and a display name
//! per token kind for a diagnostic. The schema is read by `codegen::ungram`,
//! validated, and emitted by `codegen::kind` into `OUT_DIR/syntax_kind.rs`
//! and by `codegen::tree` into `OUT_DIR/tree.rs` and `OUT_DIR/display.rs`,
//! which `lib.rs` and `tree.rs` include and nothing commits (design §10).
//! A schema that does not parse, a symbol the punctuation table does not
//! name, or a same-typed sibling no anchor token precedes, is a build
//! error that names its line.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// One body, two readers: `tests/schema.rs` includes the same module for
// the counts and the spot checks; what this build does not call is not
// dead.
#[allow(dead_code)]
#[path = "codegen/mod.rs"]
mod codegen;

use codegen::ungram::Error;

fn main() {
    // Read when `build.rs` runs, not when it compiles: cargo runs one
    // compiled `build.rs` for every checkout sharing a target directory
    // (#2628).
    let Some(dir) = env::var_os("CARGO_MANIFEST_DIR") else {
        println!("cargo:warning=CARGO_MANIFEST_DIR is unset; cargo sets it for every `build.rs`");
        std::process::exit(1);
    };
    let schema = Path::new(&dir).join("php.ungram");
    println!("cargo:rerun-if-changed={}", schema.display());
    println!("cargo:rerun-if-changed=codegen");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap_or_default());
    match generate(&schema) {
        Ok(files) => {
            for (name, text) in files {
                let path = out.join(name);
                if let Err(e) = fs::write(&path, text) {
                    println!("cargo:warning=writing {}: {e}", path.display());
                    std::process::exit(1);
                }
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

/// The text of `syntax_kind.rs`, `tree.rs` and `display.rs` for the
/// schema at `path`, each with its file name, or every error the schema
/// has, each with its line where it has one.
fn generate(path: &Path) -> Result<[(&'static str, String); 3], Vec<Error>> {
    let text = fs::read_to_string(path)
        .map_err(|e| vec![Error::global(format!("reading {}: {e}", path.display()))])?;
    let grammar = codegen::ungram::parse(&text)?;
    let errors = codegen::ungram::validate(&grammar);
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok([
        ("syntax_kind.rs", codegen::kind::generate(&grammar)?),
        ("tree.rs", codegen::tree::generate(&grammar)?),
        ("display.rs", codegen::tree::generate_display(&grammar)?),
    ])
}
