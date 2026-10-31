//! php's table of natives, generated from `data/func-infos.toml`
//! (ADR-0015, `scripts/extract_func_infos.py`): `zend_func_infos.h`'s
//! `func_infos[]` as one slice sorted by name, each row the `MAY_BE_*`
//! mask the `F1` or `FN` macro makes of it, written as an expression over
//! `php_unit::may_be` so the numbers stay php's. Written to
//! `OUT_DIR/func_infos.rs` and included by `func_info`, never committed
//! (design §10).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    // Read when `build.rs` runs, not when it compiles: cargo runs one
    // compiled `build.rs` for every checkout sharing a target directory
    // (#2628).
    let Some(dir) = env::var_os("CARGO_MANIFEST_DIR") else {
        println!("cargo:warning=CARGO_MANIFEST_DIR is unset; cargo sets it for every `build.rs`");
        std::process::exit(1);
    };
    let Some(out) = env::var_os("OUT_DIR") else {
        println!("cargo:warning=OUT_DIR is unset; cargo sets it for every `build.rs`");
        std::process::exit(1);
    };
    let out = PathBuf::from(out).join("func_infos.rs");
    let data = Path::new(&dir).join("../../data/func-infos.toml");
    println!("cargo:rerun-if-changed={}", data.display());
    let text = fs::read_to_string(&data)
        .map_err(|e| e.to_string())
        .and_then(|text| generate(&text));
    match text {
        Ok(text) => {
            if let Err(e) = fs::write(&out, text) {
                println!("cargo:warning=writing {}: {e}", out.display());
                std::process::exit(1);
            }
        }
        Err(e) => {
            println!("cargo:warning=data/func-infos.toml: {e}");
            std::process::exit(1);
        }
    }
}

/// The `may_be` expression of one `MAY_BE_*` word of the table, its
/// prefix dropped (`Z/Zend/zend_type_info.h`).
fn word(word: &str) -> Result<String, String> {
    const VALUES: [&str; 9] = [
        "NULL", "FALSE", "TRUE", "LONG", "DOUBLE", "STRING", "ARRAY", "OBJECT", "RESOURCE",
    ];
    Ok(match word {
        "BOOL" => "may_be::FALSE | may_be::TRUE".to_owned(),
        "ARRAY_KEY_LONG" => "may_be::PACKED | may_be::NUMERIC_HASH".to_owned(),
        "ARRAY_KEY_STRING" => "may_be::STRING_HASH".to_owned(),
        "ARRAY_OF_ANY" => "may_be::OF_ANY".to_owned(),
        "ARRAY_OF_REF" => "may_be::OF_REF".to_owned(),
        w if VALUES.contains(&w) => format!("may_be::{w}"),
        w => match w.strip_prefix("ARRAY_OF_") {
            Some(of) if VALUES.contains(&of) => format!("(may_be::{of} << may_be::ARRAY_SHIFT)"),
            _ => return Err(format!("MAY_BE_{w} is no word the table is read with")),
        },
    })
}

fn generate(text: &str) -> Result<String, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let functions = table
        .get("function")
        .and_then(toml::Value::as_array)
        .ok_or("no [[function]] rows")?;
    let counted = table.get("rows").and_then(toml::Value::as_integer);
    if counted != i64::try_from(functions.len()).ok() {
        return Err(format!(
            "rows = {counted:?} where the file holds {} [[function]] rows",
            functions.len()
        ));
    }
    let mut rows = Vec::new();
    for row in functions {
        let field = |key: &str| {
            row.get(key)
                .and_then(toml::Value::as_str)
                .ok_or(format!("a row without its {key}"))
        };
        let name = field("name")?;
        let mut mask = vec![match field("count")? {
            "0" => "0".to_owned(),
            "1" => "may_be::RC1".to_owned(),
            "n" => "may_be::RC1 | may_be::RCN".to_owned(),
            other => return Err(format!("{name}: count {other} is none of 0, 1 and n")),
        }];
        for w in row
            .get("types")
            .and_then(toml::Value::as_array)
            .ok_or(format!("{name}: no types"))?
        {
            mask.push(word(
                w.as_str().ok_or(format!("{name}: a type not a string"))?,
            )?);
        }
        rows.push((name.to_owned(), mask.join(" | ")));
    }
    rows.sort();
    let twice = rows.windows(2).find_map(|pair| match pair {
        [a, b] if a.0 == b.0 => Some(&a.0),
        _ => None,
    });
    if let Some(name) = twice {
        return Err(format!("{name} is in the table twice"));
    }
    let mut out = String::from(
        "/// `func_infos[]` by name, sorted: each native's\n\
         /// `MAY_BE_*` mask as `zend_func_infos.h` gives it.\n\
         pub(crate) static FUNC_INFOS: &[(&[u8], u32)] = &[\n",
    );
    for (name, mask) in rows {
        writeln!(out, "    (b\"{name}\", {mask}),").map_err(|e| e.to_string())?;
    }
    out.push_str("];\n");
    Ok(out)
}
