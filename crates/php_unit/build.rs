//! The opcode enum and its tables, generated from `ops.toml` at the
//! workspace root (design §4.7, §5.7, seam 7): one enum, so the compiler,
//! the dump and the dispatch cannot disagree on a number. Written to
//! `OUT_DIR/opcode.rs` and included by `opcode`, never committed (design
//! §10). The enum keeps php's numbers as discriminants in the one byte
//! `zend_op.opcode` is, including the two `unused_numbers`; its variants
//! are php's names in camel case. Beside it, the handler enum an opline
//! is resolved to where its unit is published, one variant per opcode
//! and per pair of kinds its row specialises (`Handler`).

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
    /// The words of the handler line's `SPEC(...)`: `COMMUTATIVE`, the
    /// `ZEND_VM_COMMUTATIVE` bit of `zend_vm_opcodes_flags`, and
    /// `SMART_BRANCH`, a comparison's handler one over it and the `JMPZ`
    /// or `JMPNZ` after it ([`Op::says`]).
    spec: Vec<String>,
    /// php's `zend_vm_opcodes_flags` word for the opcode, without its
    /// two top bits.
    flags: u32,
    /// Where the design's hot list specialises the opcode's handler
    /// (`specialise`), the kinds of `op1` and of `op2` it is specialised
    /// over, each `None` for an operand whose shape carries no zval kind
    /// with php's `SPEC` bit, `ANY` or a field such as `JMP_ADDR`, which
    /// `zend_vm_gen.php` does not specialise over.
    kinds: Option<[Option<Vec<String>>; 2]>,
}

impl Op {
    /// Whether the handler line's `SPEC(...)` says `word`.
    fn says(&self, word: &str) -> bool {
        self.spec.iter().any(|said| said == word)
    }
}

fn generate(path: &Path) -> Result<String, String> {
    let ops = read(path)?;
    let mut out = render(&ops)?;
    handlers(&ops, &mut out)?;
    Ok(out)
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
        let spec_words = row
            .get("spec")
            .and_then(toml::Value::as_str)
            .map(|spec| {
                spec.split(',')
                    .filter(|word| !word.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let spec = |key: &str| row.get(key).and_then(toml::Value::as_str).unwrap_or("ANY");
        let flags = operand_flags(&name, spec("op1_raw"))?
            | (operand_flags(&name, spec("op2_raw"))? << 8)
            | ext_flags(&name, row.get("ext").and_then(toml::Value::as_str))?;
        let kinds = row
            .get("specialise")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
            .then(|| [specialised(row, "op1"), specialised(row, "op2")]);
        ops.push(Op {
            num,
            variant: camel(&name),
            name,
            effects,
            result_used,
            safepoint,
            spec: spec_words,
            flags,
            kinds,
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
        by_num(&|op| op.says("COMMUTATIVE").to_string(), "false")
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

/// The kinds a row lists for one operand, where its shape, `op1_raw` or
/// `op2_raw`, carries a zval kind with php's `SPEC` bit.
fn specialised(row: &toml::Value, field: &str) -> Option<Vec<String>> {
    let raw = row
        .get(format!("{field}_raw"))
        .and_then(toml::Value::as_str)
        .unwrap_or("ANY");
    raw.split('|')
        .any(|word| {
            matches!(
                word,
                "CONST" | "TMP" | "VAR" | "CV" | "UNUSED" | "TMPVAR" | "TMPVARCV"
            )
        })
        .then(|| {
            row.get(field)
                .and_then(toml::Value::as_array)
                .map(|kinds| {
                    kinds
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        })
}

/// Each dimension one opcode's handler is resolved over, what the
/// `match` reads with each value's pattern and the part of the variant's
/// name it gives, and whether a kind is among them, which leaves the
/// kinds a row does not list to a variant of their own.
fn dimensions(op: &Op) -> Result<(Vec<Dimension>, bool), String> {
    let (op1, op2) = match &op.kinds {
        Some([op1, op2]) => (op1.as_ref(), op2.as_ref()),
        None => (None, None),
    };
    let mut dimensions: Vec<Dimension> = Vec::new();
    for (scrutinee, kinds) in [("op.op1_type", op1), ("op.op2_type", op2)] {
        if let Some(kinds) = kinds {
            let values = kinds
                .iter()
                .map(|k| kind(&op.name, k).map(|k| (format!("OperandKind::{k}"), k.to_owned())))
                .collect::<Result<Vec<_>, String>>()?;
            dimensions.push((scrutinee, values));
        }
    }
    // `SPEC(OP_DATA=...)` on an opcode the hot list specialises: the
    // kind of the `OP_DATA` opline's `op1` after it, as php's
    // `_OP_DATA_CV` handlers are.
    if op.kinds.is_some()
        && let Some(listed) = op
            .spec
            .iter()
            .find_map(|word| word.strip_prefix("OP_DATA="))
    {
        let values = listed
            .split('|')
            .map(|k| {
                kind(&op.name, k).map(|k| (format!("Some(OperandKind::{k})"), format!("OpData{k}")))
            })
            .collect::<Result<Vec<_>, String>>()?;
        dimensions.push(("data_kind(next)", values));
    }
    let listed = !dimensions.is_empty();
    // `SPEC(RETVAL)` on an opcode the hot list specialises by kind:
    // whether the opline's result is read, `RETURN_VALUE_USED`, as php's
    // `_RETVAL_UNUSED` and `_RETVAL_USED` handlers are. Read by the
    // kinds' own `match`, so it adds no `Unlisted` of its own.
    if listed && op.says("RETVAL") {
        dimensions.push((
            "op.out_type == OperandKind::Unused",
            vec![
                ("true".to_owned(), "RetvalUnused".to_owned()),
                ("false".to_owned(), "RetvalUsed".to_owned()),
            ],
        ));
    }
    if op.says("SMART_BRANCH") {
        dimensions.push((
            "Fused::of(op, next, at)",
            ["No", "Jmpz", "Jmpnz", "JmpzBack", "JmpnzBack"]
                .iter()
                .map(|way| {
                    let name = if *way == "No" { "" } else { way };
                    (format!("Fused::{way}"), name.to_owned())
                })
                .collect(),
        ));
    }
    if ASSIGNED.contains(&op.name.as_str()) {
        dimensions.push((
            "Assigned::of(op, next)",
            vec![
                ("Assigned::No".to_owned(), String::new()),
                ("Assigned::Cv".to_owned(), "AssignCv".to_owned()),
            ],
        ));
    }
    Ok((dimensions, listed))
}

/// The arithmetic whose handler is resolved over whether the opline
/// after it is the `ASSIGN` of its temporary to a CV (`Assigned`), so
/// that the loop serves both oplines with one dispatch, as a comparison
/// whose line says `SMART_BRANCH` is resolved over the jump after it
/// (design §5.8). php fuses no such pair; the opcodes and the dump are
/// php's, and the handler alone tells the pair apart. `php_engine`'s
/// `build.rs` names the same variants from the same list.
const ASSIGNED: [&str; 4] = ["ADD", "SUB", "MUL", "DIV"];

/// What a `match` reads, and each value's pattern with the part of the
/// variant's name it gives.
type Dimension = (&'static str, Vec<(String, String)>);

/// The handler enum and its resolution: one variant for an opcode
/// `ops.toml` does not specialise, and for one it does a variant for
/// every pair of the kinds it lists for the operands it specialises,
/// `op1`'s then `op2`'s, and one for the kinds it does not list, as
/// `zend_vm_gen.php` emits a handler per `SPEC` pair and
/// `ZEND_NULL_HANDLER` for the rest; for an opcode whose line says
/// `RETVAL`, each kind's twice over, its `out` unread and read, as
/// php's `_RETVAL_UNUSED` and `_RETVAL_USED` handlers are; for one whose
/// handler line says `SMART_BRANCH`, each of those five times over,
/// unfused and fused to the `JMPZ` or the `JMPNZ` after it, as php's
/// `_JMPZ` and `_JMPNZ` handlers are, with the jump forward or back,
/// which php does not tell apart where it takes the jump and this
/// resolves with the handler; and for the arithmetic of [`ASSIGNED`],
/// each of those twice over, alone and fused to the `ASSIGN` of its
/// temporary to a CV after it. `Unresolved`, the default, is what an
/// opline holds until its unit is published, and what no opline of a
/// `PublishedUnit` holds.
fn handlers(ops: &[Op], out: &mut String) -> Result<(), String> {
    let mut variants = vec!["Unresolved".to_owned()];
    let mut arms = String::new();
    for op in ops {
        let (dimensions, listed) = dimensions(op)?;
        if dimensions.is_empty() {
            variants.push(op.variant.clone());
            writeln!(arms, "            Opcode::{0} => Handler::{0},", op.variant).ok();
            continue;
        }
        let scrutinee = dimensions.iter().map(|(read, _)| *read).collect::<Vec<_>>();
        let scrutinee = match scrutinee.as_slice() {
            [one] => (*one).to_owned(),
            many => format!("({})", many.join(", ")),
        };
        writeln!(
            arms,
            "            Opcode::{} => match {scrutinee} {{",
            op.variant
        )
        .ok();
        let mut rows: Vec<(Vec<String>, String)> = vec![(Vec::new(), op.variant.clone())];
        for (_, values) in &dimensions {
            rows = rows
                .iter()
                .flat_map(|(patterns, name)| {
                    values.iter().map(move |(pattern, part)| {
                        let mut patterns = patterns.clone();
                        patterns.push(pattern.clone());
                        (patterns, format!("{name}{part}"))
                    })
                })
                .collect();
        }
        for (patterns, variant) in rows {
            let pattern = match patterns.as_slice() {
                [one] => one.clone(),
                many => format!("({})", many.join(", ")),
            };
            writeln!(arms, "                {pattern} => Handler::{variant},").ok();
            variants.push(variant);
        }
        if listed {
            let unlisted = format!("{}Unlisted", op.variant);
            writeln!(arms, "                _ => Handler::{unlisted},").ok();
            variants.push(unlisted);
        }
        writeln!(arms, "            }},").ok();
    }
    if variants.len() > usize::from(u16::MAX) {
        return Err(format!(
            "{} handlers do not fit the u16 an opline holds",
            variants.len()
        ));
    }
    writeln!(
        out,
        "/// The handler an opline is served by, as `zend_vm_set_opcode_handler`\n\
         /// picks `zend_op.handler` (`Z/Zend/zend_vm_execute.h`): its opcode, at\n\
         /// the kinds of its operands where `ops.toml` specialises the opcode\n\
         /// (`specialise`), one variant per pair of the kinds the row lists for\n\
         /// the operands whose shape has php's `SPEC` bit, `AddCvConst` for an\n\
         /// `ADD` of a CV and a constant and `JmpzCv` for a `JMPZ` over a CV,\n\
         /// whose `op2` is a jump address, and `AddUnlisted` for the kinds it\n\
         /// does not list, php's `ZEND_NULL_HANDLER`. Resolved once, where the\n\
         /// unit is published ([`crate::Unit::published`]), so the dispatch\n\
         /// reads one field and jumps once; `Unresolved` until then, which no\n\
         /// opline of a [`crate::PublishedUnit`] holds.\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]\n\
         #[repr(u16)]\n\
         pub enum Handler {{\n\
         \x20   #[default]"
    )
    .ok();
    for variant in &variants {
        writeln!(out, "    {variant},").ok();
    }
    writeln!(
        out,
        "}}\n\
         \n\
         impl Handler {{\n\
         \x20   /// The handler `op`, the opline at `at`, is served by.\n\
         \x20   // One arm per opcode: the length is the number of opcodes, and a\n\
         \x20   // `_` is the kinds the row does not list, however few are left.\n\
         \x20   #[allow(clippy::too_many_lines, clippy::match_wildcard_for_single_variants)]\n\
         \x20   #[must_use]\n\
         \x20   pub(crate) fn of(op: &Opline, next: Option<&Opline>, at: usize) -> Handler {{\n\
         \x20       match op.opcode {{\n\
         {arms}\
         \x20       }}\n\
         \x20   }}\n\
         }}"
    )
    .ok();
    Ok(())
}

/// The variant of `OperandKind` a kind `ops.toml` lists names.
fn kind(op: &str, kind: &str) -> Result<&'static str, String> {
    Ok(match kind {
        "CONST" => "Const",
        "TMP" => "Tmp",
        "VAR" => "Var",
        "CV" => "Cv",
        "UNUSED" => "Unused",
        other => {
            return Err(format!(
                "{op}: operand kind {other} is not one an opline holds"
            ));
        }
    })
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
