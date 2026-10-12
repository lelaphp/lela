//! The opcode enum and its tables, generated from `ops.toml` at the
//! workspace root (design §4.7, §5.7, seam 7): one enum, so the compiler,
//! the dump and the dispatch cannot disagree on a number. Written to
//! `OUT_DIR/opcode.rs` and included by `opcode`, never committed (design
//! §10). The enum keeps php's numbers as discriminants in the one byte
//! `zend_op.opcode` is, including the two `unused_numbers`; its variants
//! are php's names in camel case.

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
    let root = Path::new(&dir).join("../../ops.toml");
    println!("cargo:rerun-if-changed={}", root.display());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap_or_default()).join("opcode.rs");
    match generate(&root) {
        Ok(text) => {
            if let Err(e) = fs::write(&out, text) {
                println!("cargo:warning=writing {}: {e}", out.display());
                std::process::exit(1);
            }
        }
        Err(e) => {
            println!("cargo:warning=ops.toml: {e}");
            std::process::exit(1);
        }
    }
}

struct Op {
    num: u8,
    name: String,
    variant: String,
    effects: Vec<String>,
    result_used: bool,
    safepoint: bool,
    /// `COMMUTATIVE` in the handler line's `SPEC(...)`, the
    /// `ZEND_VM_COMMUTATIVE` bit of `zend_vm_opcodes_flags`.
    commutative: bool,
    /// php's `zend_vm_opcodes_flags` word for the opcode, without its
    /// two top bits.
    flags: u32,
}

fn generate(path: &Path) -> Result<String, String> {
    let ops = read(path)?;
    render(&ops)
}

/// The `[[op]]` rows, in number order.
fn read(path: &Path) -> Result<Vec<Op>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let table: toml::Table = toml::from_str(&text).map_err(|e| e.to_string())?;
    let rows = table
        .get("op")
        .and_then(toml::Value::as_array)
        .ok_or("no [[op]] rows")?;
    let mut ops = Vec::new();
    for row in rows {
        let num = row
            .get("num")
            .and_then(toml::Value::as_integer)
            .and_then(|n| u8::try_from(n).ok())
            .ok_or("an [[op]] row without a num that fits php's byte")?;
        let name = row
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or("an [[op]] row without a name")?
            .to_owned();
        let effects = row
            .get("effects")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let result_used = row.get("result").and_then(toml::Value::as_str) == Some("used");
        let safepoint = row.get("safepoint").is_some();
        let commutative = row
            .get("spec")
            .and_then(toml::Value::as_str)
            .is_some_and(|spec| spec.split(',').any(|word| word == "COMMUTATIVE"));
        let spec = |key: &str| row.get(key).and_then(toml::Value::as_str).unwrap_or("ANY");
        let flags = operand_flags(&name, spec("op1_raw"))?
            | (operand_flags(&name, spec("op2_raw"))? << 8)
            | ext_flags(&name, row.get("ext").and_then(toml::Value::as_str))?;
        ops.push(Op {
            num,
            variant: camel(&name),
            name,
            effects,
            result_used,
            safepoint,
            commutative,
            flags,
        });
    }
    ops.sort_by_key(|op| op.num);
    Ok(ops)
}

/// The enum and its tables as Rust source.
fn render(ops: &[Op]) -> Result<String, String> {
    let last = ops.last().map_or(0, |op| usize::from(op.num));
    let by_num = |f: &dyn Fn(&Op) -> String, none: &str| -> String {
        let mut cells = vec![none.to_owned(); last + 1];
        for op in ops {
            cells[usize::from(op.num)] = f(op);
        }
        cells.join(", ")
    };
    let mut out = String::new();
    writeln!(
        out,
        "/// One of php's opcodes, numbered as `Zend/zend_vm_opcodes.h` numbers it."
    )
    .ok();
    writeln!(
        out,
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]\n#[repr(u8)]\npub enum Opcode {{"
    ).ok();
    for op in ops {
        writeln!(out, "    /// `ZEND_{}`", op.name).ok();
        writeln!(out, "    {} = {},", op.variant, op.num).ok();
    }
    writeln!(out, "}}").ok();
    writeln!(out, "pub(crate) const COUNT: usize = {};", ops.len()).ok();
    writeln!(out, "pub(crate) const LAST: usize = {last};").ok();
    writeln!(
        out,
        "pub(crate) static BY_NUM: [Option<Opcode>; LAST + 1] = [{}];",
        by_num(&|op| format!("Some(Opcode::{})", op.variant), "None")
    )
    .ok();
    writeln!(
        out,
        "pub(crate) static NAMES: [&str; LAST + 1] = [{}];",
        by_num(&|op| format!("\"ZEND_{}\"", op.name), "\"\"")
    )
    .ok();
    let effect_bits = |op: &Op| -> Result<String, String> {
        let mut bits = Vec::new();
        for effect in &op.effects {
            bits.push(match effect.as_str() {
                "throw" => "Effects::THROW.0",
                "alloc" => "Effects::ALLOC.0",
                "call" => "Effects::CALL.0",
                "write_globals" => "Effects::GLOBAL_WRITE.0",
                other => {
                    return Err(format!(
                        "{}: effect {other} is not one the enum knows",
                        op.name
                    ));
                }
            });
        }
        Ok(if bits.is_empty() {
            "0".to_owned()
        } else {
            bits.join(" | ")
        })
    };
    let mut effects = Vec::new();
    for op in ops {
        effects.push(effect_bits(op)?);
    }
    writeln!(
        out,
        "pub(crate) static EFFECTS: [u8; LAST + 1] = [{}];",
        by_num(
            &|op| effects[ops.iter().position(|o| o.num == op.num).unwrap_or(0)].clone(),
            "0"
        )
    )
    .ok();
    writeln!(
        out,
        "pub(crate) static RESULT_USED: [bool; LAST + 1] = [{}];",
        by_num(&|op| op.result_used.to_string(), "false")
    )
    .ok();
    writeln!(
        out,
        "pub(crate) static SAFEPOINT: [bool; LAST + 1] = [{}];",
        by_num(&|op| op.safepoint.to_string(), "false")
    )
    .ok();
    writeln!(
        out,
        "pub(crate) static COMMUTATIVE: [bool; LAST + 1] = [{}];",
        by_num(&|op| op.commutative.to_string(), "false")
    )
    .ok();
    // php's `zend_vm_opcodes_flags` without its two top bits, which
    // name a specialisation and not a field: every other bit agrees
    // with the table in `Zend/zend_vm_opcodes.c` at the tag.
    writeln!(
        out,
        "pub(crate) static FLAGS: [u32; LAST + 1] = [{}];",
        by_num(
            &|op| format!("0x{:04x}_{:04x}", op.flags >> 16, op.flags & 0xffff),
            "0"
        )
    )
    .ok();
    Ok(out)
}

/// One operand's byte of `zend_vm_opcodes_flags`, decoded from the
/// handler line's `op1_raw` or `op2_raw` as `zend_vm_gen.php`'s
/// `$vm_op_decode` decodes it: the `SPEC` bit and the two zval-kind bits
/// in the low nibble, and in the high nibble the one shape the field
/// has when it is not a zval, `ZEND_VM_OP_NUM` through
/// `ZEND_VM_OP_CACHE_SLOT`.
fn operand_flags(op: &str, spec: &str) -> Result<u32, String> {
    let mut flags = 0;
    for word in spec.split('|') {
        flags |= match word {
            "ANY" => 0x00,
            "CONST" => 0x03,
            "TMP" | "VAR" | "UNUSED" | "CV" => 0x01,
            "TMPVAR" => 0x05,
            "TMPVARCV" => 0x09,
            "NUM" => 0x10,
            "JMP_ADDR" => 0x20,
            "TRY_CATCH" => 0x30,
            "LOOP_END" => 0x40,
            "THIS" => 0x50,
            "NEXT" => 0x60,
            "CLASS_FETCH" => 0x70,
            "CONSTRUCTOR" => 0x80,
            "CONST_FETCH" => 0x90,
            "CACHE_SLOT" => 0xa0,
            other => {
                return Err(format!(
                    "{op}: operand shape {other} is not one zend_vm_gen.php knows"
                ));
            }
        };
    }
    Ok(flags)
}

/// The `extended_value` bits of `zend_vm_opcodes_flags`, decoded from
/// the handler line's `ext` as `$vm_ext_decode` decodes it: seven bits
/// read one by one and, in `ZEND_VM_EXT_MASK`, the one shape the field
/// has.
fn ext_flags(op: &str, spec: Option<&str>) -> Result<u32, String> {
    let mut flags = 0;
    for word in spec.into_iter().flat_map(|s| s.split('|')) {
        flags |= match word {
            "VAR_FETCH" => 0x0001_0000,
            "ISSET" => 0x0002_0000,
            "CACHE_SLOT" => 0x0004_0000,
            "ARRAY_INIT" => 0x0008_0000,
            "REF" => 0x0010_0000,
            "FETCH_REF" => 0x0020_0000,
            "DIM_WRITE" => 0x0040_0000,
            "NUM" => 0x0100_0000,
            "LAST_CATCH" => 0x0200_0000,
            "JMP_ADDR" => 0x0300_0000,
            "OP" => 0x0400_0000,
            "TYPE" => 0x0700_0000,
            "EVAL" => 0x0800_0000,
            "TYPE_MASK" => 0x0900_0000,
            "SRC" => 0x0b00_0000,
            other => {
                return Err(format!(
                    "{op}: extended_value shape {other} is not one zend_vm_gen.php knows"
                ));
            }
        };
    }
    Ok(flags)
}

/// `ASSIGN_DIM_OP` as `AssignDimOp`.
fn camel(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first
                    .to_uppercase()
                    .chain(chars.map(|c| c.to_ascii_lowercase()))
                    .collect(),
                None => String::new(),
            }
        })
        .collect()
}
