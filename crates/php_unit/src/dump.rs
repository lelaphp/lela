//! An op array in php's own dump format (design §6.1): what
//! `zend_dump_op_array` (`Z/Zend/Optimizer/zend_dump.c`) prints under
//! `opcache.opt_debug_level=0x10000`, before the optimizer runs, and
//! what `compile-identity` holds the compiler to byte for byte. The
//! header is `zend_dump_op_array`'s four lines; an opline is
//! `zend_dump_op_line` and `zend_dump_op`: the operand `result` names
//! and ` = ` first, `T1 = ADD ...`, when its kind is `TMP`, `VAR` or
//! `CV`, then
//! the opcode's name, its `extended_value` by the shape `ops.toml` gives
//! the field, and its two operands: a constant as `zend_dump_const`
//! prints one, a variable as `zend_dump_var` numbers it, and an unused
//! field as `zend_dump_unused_op` reads it, a jump target, a number,
//! `THIS`, `NEXT` or how a class is fetched. The `try`/`catch` table
//! follows the oplines as php's does.
//!
//! **What is not printed.** php prints the jump table of a
//! `SWITCH_LONG`, `SWITCH_STRING` or `MATCH`, the name of a
//! `FRAMELESS_ICALL_*` and the live-range table; each waits on the unit
//! holding the thing, and is printed as it lands (#228).
//!
//! **Numbers.** php prints a temporary by its frame slot, counted past
//! the CVs (`EX_VAR_TO_NUM`); an opline holds the temporary's own
//! number, and the slot is `num_cvs` past it, which is what the engine's
//! `temporary_slot` computes. A jump target is the opline's index, as
//! the compiler stores it and as php prints it after subtracting the
//! op array's base.

use std::fmt::Write as _;

use php_heap::{Zval, ZvalType};

use crate::decl::FunctionDecl;
use crate::op::{OpArray, OperandKind, Opline};
use crate::unit::Unit;
use crate::{Opcode, opcode};

impl Opcode {
    /// The operand and `extended_value` bits of php's
    /// `zend_vm_opcodes_flags` word for the opcode.
    pub(crate) fn flags(self) -> u32 {
        opcode::FLAGS
            .get(usize::from(self.num()))
            .copied()
            .unwrap_or(0)
    }
}

/// `ZEND_VM_OP_MASK` and its values: the shape of an operand field that
/// holds no zval.
mod op {
    pub const MASK: u32 = 0x00f0;
    pub const NUM: u32 = 0x0010;
    pub const JMP_ADDR: u32 = 0x0020;
    pub const TRY_CATCH: u32 = 0x0030;
    pub const LOOP_END: u32 = 0x0040;
    pub const THIS: u32 = 0x0050;
    pub const NEXT: u32 = 0x0060;
    pub const CLASS_FETCH: u32 = 0x0070;
    pub const CONSTRUCTOR: u32 = 0x0080;
    pub const CONST_FETCH: u32 = 0x0090;
}

/// `ZEND_VM_EXT_*`: the seven bits read one by one and, under
/// `ZEND_VM_EXT_MASK`, the shape of `extended_value`.
mod ext {
    pub const VAR_FETCH: u32 = 0x0001_0000;
    pub const ISSET: u32 = 0x0002_0000;
    pub const ARRAY_INIT: u32 = 0x0008_0000;
    pub const REF: u32 = 0x0010_0000;
    pub const FETCH_REF: u32 = 0x0020_0000;
    pub const DIM_WRITE: u32 = 0x0040_0000;
    pub const MASK: u32 = 0x0f00_0000;
    pub const NUM: u32 = 0x0100_0000;
    pub const JMP_ADDR: u32 = 0x0300_0000;
    pub const OP: u32 = 0x0400_0000;
    pub const TYPE: u32 = 0x0700_0000;
    pub const EVAL: u32 = 0x0800_0000;
    pub const TYPE_MASK: u32 = 0x0900_0000;
    pub const SRC: u32 = 0x0b00_0000;
}

/// `ZEND_LAST_CATCH`, on a `CATCH`'s `extended_value`.
const LAST_CATCH: u32 = 1;
/// `ZEND_FREE_ON_RETURN`, on a `FREE`'s `extended_value`.
const FREE_ON_RETURN: u32 = 1;
/// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE`, on a `FETCH_CONSTANT`'s `op1`.
const UNQUALIFIED_IN_NAMESPACE: u32 = 0x800;

/// Every op array of the unit, one block each, in php's order: the main
/// op array, then the functions as declared.
pub(crate) fn unit(unit: &Unit) -> String {
    let mut out = String::new();
    op_array(&mut out, "$_main", 0, &unit.main);
    for (decl, body) in &unit.functions {
        op_array(
            &mut out,
            &String::from_utf8_lossy(&decl.name.0),
            num_args(decl),
            body,
        );
    }
    out
}

/// `op_array->num_args`: the declared parameters less the variadic one.
fn num_args(decl: &FunctionDecl) -> usize {
    decl.parameters.iter().filter(|p| !p.variadic).count()
}

/// `zend_dump_op_array` with `ZEND_DUMP_LIVE_RANGES` and the message
/// `before optimizer`: a blank line, the name, the counts, the message,
/// the file with the line range, and the return line the zeroed
/// `zend_func_info` of every op array prints before the optimizer has
/// inferred anything; then the oplines and the `try`/`catch` table.
fn op_array(out: &mut String, name: &str, args: usize, body: &OpArray) {
    let _ = writeln!(
        out,
        "\n{name}:\n     ; (lines={}, args={args}, vars={}, tmps={})",
        body.oplines.len(),
        body.num_cvs,
        body.num_tmps
    );
    let _ = writeln!(out, "     ; (before optimizer)");
    let _ = writeln!(
        out,
        "     ; {}:{}-{}",
        String::from_utf8_lossy(&body.filename.0),
        body.line_start,
        body.line_end
    );
    let _ = writeln!(out, "     ; return  [] RANGE[0..0]");
    for (at, op) in body.oplines.iter().enumerate() {
        let _ = write!(out, "{at:04} ");
        opline(out, body, op);
        out.push('\n');
    }
    if !body.try_catch.is_empty() {
        let _ = writeln!(out, "EXCEPTION TABLE:");
        for range in &body.try_catch {
            let _ = write!(out, "     {:04}", range.try_op.0);
            for at in [range.catch_op.0, range.finally_op.0, range.finally_end.0] {
                if at == 0 {
                    out.push_str(", -");
                } else {
                    let _ = write!(out, ", {at:04}");
                }
            }
            out.push('\n');
        }
    }
}

/// `zend_dump_op` with no CFG and no SSA: the operand `result` names
/// and ` = ` when its kind is `TMP`, `VAR` or `CV`, the name, the
/// `extended_value` by its shape, `op1`, `op2`, and the target an
/// `extended_value` of the `JMP_ADDR` shape holds. A `result` of kind
/// `UNUSED` opens nothing, whatever the number the field holds: a
/// site's safepoint number, or the temporary php still numbered where
/// `zend_do_free` set the mark.
fn opline(out: &mut String, body: &OpArray, op: &Opline) {
    let flags = op.opcode.flags();
    let name = op.opcode.name();
    if op.out_type != OperandKind::Unused && op.out_type != OperandKind::Const {
        var(out, body, op.out_type, op.out);
        out.push_str(" = ");
    }
    if name.is_empty() {
        let _ = write!(out, "OP_{}", op.opcode.num());
    } else {
        out.push_str(name.trim_start_matches("ZEND_"));
    }
    extended_value(out, op, flags);
    operand(out, body, op, op.op1_type, op.op1, flags & 0xff, true);
    operand(
        out,
        body,
        op,
        op.op2_type,
        op.op2,
        (flags >> 8) & 0xff,
        false,
    );
    if flags & ext::MASK == ext::JMP_ADDR {
        let _ = write!(out, " {:04}", op.extended_value);
    }
    // A constant `result`, which no opline of `zend_compile.c` has, is
    // printed last where php prints it.
    if op.out_type == OperandKind::Const {
        constant(out, body.literals.get(op.out as usize).copied());
    }
}

/// One operand: the constant, the variable, or what an unused field says.
fn operand(
    out: &mut String,
    body: &OpArray,
    op: &Opline,
    kind: OperandKind,
    field: u32,
    flags: u32,
    first: bool,
) {
    match kind {
        OperandKind::Const => {
            constant(out, body.literals.get(field as usize).copied());
        }
        OperandKind::Tmp | OperandKind::Var | OperandKind::Cv => {
            out.push(' ');
            var(out, body, kind, field);
        }
        OperandKind::Unused => {
            if flags & op::MASK == op::JMP_ADDR {
                // A `CATCH` that is the last of its try prints no target
                // in `op2`, where php's compiler left none.
                if first || op.opcode != Opcode::Catch || op.extended_value & LAST_CATCH == 0 {
                    let _ = write!(out, " {field:04}");
                }
            } else {
                unused(out, op, field, flags);
            }
        }
    }
}

/// `zend_dump_const`: the constant by type, a string escaped as
/// `smart_str_append_escaped` escapes it with `"` escaped besides, a
/// double as C's `%g` prints it.
fn constant(out: &mut String, value: Option<Zval>) {
    let Some(value) = value else {
        out.push_str(" zval(type=?)");
        return;
    };
    match value.kind() {
        ZvalType::Null => out.push_str(" null"),
        ZvalType::False => out.push_str(" bool(false)"),
        ZvalType::True => out.push_str(" bool(true)"),
        ZvalType::Long => {
            let _ = write!(out, " int({})", value.as_long().unwrap_or(0));
        }
        ZvalType::Double => {
            let _ = write!(
                out,
                " float({})",
                percent_g(value.as_double().unwrap_or(0.0))
            );
        }
        ZvalType::String => {
            out.push_str(" string(\"");
            if let Some(s) = value.as_str() {
                escaped(out, s.bytes());
            }
            out.push_str("\")");
        }
        ZvalType::Array => out.push_str(" array(...)"),
        kind => {
            let _ = write!(out, " zval(type={})", kind as u8);
        }
    }
}

/// `zend_dump_var`: a CV by its number and name, a `VAR` or `TMP` by
/// its slot past the CVs, and `X` for a CV number the op array has no
/// name for.
fn var(out: &mut String, body: &OpArray, kind: OperandKind, number: u32) {
    match kind {
        OperandKind::Cv if number < body.num_cvs => {
            let name = body
                .vars
                .get(number as usize)
                .map(|n| &*n.0)
                .unwrap_or_default();
            let _ = write!(out, "CV{number}(${})", String::from_utf8_lossy(name));
        }
        OperandKind::Var => {
            let _ = write!(out, "V{}", body.num_cvs.saturating_add(number));
        }
        OperandKind::Tmp => {
            let _ = write!(out, "T{}", body.num_cvs.saturating_add(number));
        }
        _ => {
            let _ = write!(out, "X{number}");
        }
    }
}

/// `zend_dump_unused_op`: what a field that holds no zval says, by the
/// shape its opcode gives it.
fn unused(out: &mut String, op: &Opline, field: u32, flags: u32) {
    match flags & op::MASK {
        op::NUM => {
            let _ = write!(out, " {field}");
        }
        op::TRY_CATCH if field != u32::MAX => {
            let _ = write!(out, " try-catch({field})");
        }
        op::LOOP_END if op.extended_value & FREE_ON_RETURN != 0 => {
            let _ = write!(out, " loop-end(+{field})");
        }
        op::THIS => out.push_str(" THIS"),
        op::NEXT => out.push_str(" NEXT"),
        op::CLASS_FETCH => class_fetch_type(out, field),
        op::CONSTRUCTOR => out.push_str(" CONSTRUCTOR"),
        op::CONST_FETCH if field & UNQUALIFIED_IN_NAMESPACE != 0 => {
            out.push_str(" (unqualified-in-namespace)");
        }
        _ => {}
    }
}

/// `zend_dump_class_fetch_type`: which `ZEND_FETCH_CLASS_*` the fetch
/// is, and the three bits beside it.
fn class_fetch_type(out: &mut String, fetch_type: u32) {
    out.push_str(match fetch_type & 0x0f {
        1 => " (self)",
        2 => " (parent)",
        3 => " (static)",
        4 => " (auto)",
        5 => " (interface)",
        6 => " (trait)",
        _ => "",
    });
    if fetch_type & 0x80 != 0 {
        out.push_str(" (no-autoload)");
    }
    if fetch_type & 0x100 != 0 {
        out.push_str(" (silent)");
    }
    if fetch_type & 0x200 != 0 {
        out.push_str(" (exception)");
    }
}

/// The `extended_value` arm of `zend_dump_op`: one shape under
/// `ZEND_VM_EXT_MASK`, or the bits beside the mask read one by one.
fn extended_value(out: &mut String, op: &Opline, flags: u32) {
    let value = op.extended_value;
    match flags & ext::MASK {
        ext::NUM => {
            let _ = write!(out, " {value}");
        }
        ext::OP => {
            let name = u8::try_from(value)
                .ok()
                .and_then(Opcode::from_u8)
                .map_or("", Opcode::name);
            let _ = write!(out, " ({})", name.trim_start_matches("ZEND_"));
        }
        ext::TYPE => out.push_str(match value {
            1 => " (null)",
            2 => " (false)",
            3 => " (true)",
            4 => " (long)",
            5 => " (double)",
            6 => " (string)",
            7 => " (array)",
            8 => " (object)",
            9 => " (resource)",
            12 => " (callable)",
            14 => " (void)",
            17 => " (never)",
            18 => " (bool)",
            _ => " (???)",
        }),
        ext::TYPE_MASK => match value {
            0x002 => out.push_str(" (null)"),
            0x004 => out.push_str(" (false)"),
            0x008 => out.push_str(" (true)"),
            0x010 => out.push_str(" (long)"),
            0x020 => out.push_str(" (double)"),
            0x040 => out.push_str(" (string)"),
            0x080 => out.push_str(" (array)"),
            0x100 => out.push_str(" (object)"),
            0x200 => out.push_str(" (resource)"),
            0x00c => out.push_str(" (bool)"),
            mask => {
                out.push_str(" TYPE");
                type_info(out, mask);
            }
        },
        ext::EVAL => out.push_str(match value {
            1 => " (eval)",
            2 => " (include)",
            4 => " (include_once)",
            8 => " (require)",
            16 => " (require_once)",
            _ => " (???)",
        }),
        ext::SRC => {
            if value == 2 {
                out.push_str(" (value)");
            } else if value & 1 != 0 {
                out.push_str(" (function)");
            }
        }
        _ => {
            if flags & ext::VAR_FETCH != 0 {
                if value & 2 != 0 {
                    out.push_str(" (global)");
                } else if value & 4 != 0 {
                    out.push_str(" (local)");
                } else if value & 8 != 0 {
                    out.push_str(" (global+lock)");
                }
            }
            if flags & ext::ISSET != 0 {
                out.push_str(if value & 1 == 0 {
                    " (isset)"
                } else {
                    " (empty)"
                });
            }
            if flags & ext::ARRAY_INIT != 0 {
                let _ = write!(out, " {}", value >> 2);
                if value & 2 == 0 {
                    out.push_str(" (packed)");
                }
            }
            if flags & ext::REF != 0 && value & 1 != 0 {
                out.push_str(" (ref)");
            }
            if flags & (ext::DIM_WRITE | ext::FETCH_REF) != 0 {
                match value & 3 {
                    1 => out.push_str(" (ref)"),
                    2 => out.push_str(" (dim write)"),
                    _ => {}
                }
            }
        }
    }
}

/// `zend_dump_type_info` over the bits a `TYPE_CHECK` mask can hold, the
/// `MAY_BE_*` of the nine value types: `[bool, long, double, string]` for
/// `is_scalar`. The array, class and reference detail of an inferred
/// type never reaches an `extended_value`.
fn type_info(out: &mut String, info: u32) {
    const ANY: u32 = 0x3fe;
    let mut words: Vec<&str> = Vec::new();
    if info & ANY == ANY {
        words.push("any");
    } else {
        if info & 0x002 != 0 {
            words.push("null");
        }
        match (info & 0x004 != 0, info & 0x008 != 0) {
            (true, true) => words.push("bool"),
            (true, false) => words.push("false"),
            (false, true) => words.push("true"),
            (false, false) => {}
        }
        for (bit, word) in [
            (0x010, "long"),
            (0x020, "double"),
            (0x040, "string"),
            (0x080, "array"),
            (0x100, "object"),
            (0x200, "resource"),
        ] {
            if info & bit != 0 {
                words.push(word);
            }
        }
    }
    let _ = write!(out, " [{}]", words.join(", "));
}

/// `smart_str_append_escaped` with `"` escaped besides, as
/// `zend_dump_const` prints a string: the seven C escapes by letter, and
/// every other byte outside printable ASCII as `\x` and two upper-case
/// hex digits.
fn escaped(out: &mut String, bytes: &[u8]) {
    for &c in bytes {
        match c {
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0c => out.push_str("\\f"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            0x1b => out.push_str("\\e"),
            c if !(32..=126).contains(&c) => {
                let _ = write!(out, "\\x{c:02X}");
            }
            c => out.push(char::from(c)),
        }
    }
}

/// C's `%g` at its default precision, six significant digits: fixed
/// notation while the exponent is in `-4..6` and `%e`'s otherwise, the
/// trailing zeros of the fraction dropped and the point with them, and
/// the exponent signed and at least two digits. `inf`, `-inf` and `nan`
/// as glibc and libSystem spell them.
fn percent_g(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return (if value > 0.0 { "inf" } else { "-inf" }).to_owned();
    }
    let rendered = format!("{value:.5e}");
    let (mantissa, exponent) = rendered.split_once('e').unwrap_or((rendered.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let mut text = if (-4..6).contains(&exponent) {
        format!("{value:.*}", usize::try_from(5 - exponent).unwrap_or(0))
    } else {
        format!(
            "{mantissa}e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
    };
    let (digits, tail) = match text.find('e') {
        Some(at) => (text[..at].to_owned(), text[at..].to_owned()),
        None => (text.clone(), String::new()),
    };
    if digits.contains('.') {
        let trimmed = digits.trim_end_matches('0').trim_end_matches('.');
        text = format!("{trimmed}{tail}");
    }
    text
}

#[cfg(test)]
mod tests {
    use php_heap::Intern as _;

    use super::*;
    use crate::decl::{Name, Parameter};
    use crate::op::{OplineIdx, TryCatch};

    /// A constant string interned into a fresh table.
    fn text(strings: &mut php_heap::Strings, bytes: &[u8]) -> Zval {
        strings.intern(bytes).map_or(Zval::UNDEF, Zval::string)
    }

    fn op(opcode: Opcode) -> Opline {
        Opline {
            opcode,
            ..Opline::default()
        }
    }

    fn with(op: Opline, kind: OperandKind, field: u32, second: OperandKind, other: u32) -> Opline {
        Opline {
            op1_type: kind,
            op1: field,
            op2_type: second,
            op2: other,
            ..op
        }
    }

    fn body(oplines: Vec<Opline>, literals: Vec<Zval>) -> OpArray {
        OpArray {
            oplines: oplines.into_boxed_slice(),
            literals: literals.into_boxed_slice(),
            filename: Name::from("a.php"),
            line_start: 1,
            line_end: 1,
            ..OpArray::default()
        }
    }

    /// One opline printed on its own, without the header.
    fn line(body: &OpArray, at: usize) -> String {
        let mut out = String::new();
        if let Some(op) = body.oplines.get(at) {
            opline(&mut out, body, op);
        }
        out
    }

    #[test]
    fn the_header_is_php_s_four_lines_and_a_blank_line_before_the_name() {
        let unit = Unit {
            main: body(
                vec![with(
                    op(Opcode::Return),
                    OperandKind::Const,
                    0,
                    OperandKind::Unused,
                    0,
                )],
                vec![Zval::long(1)],
            ),
            ..Unit::default()
        };
        assert_eq!(
            unit.dump(),
            "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n     ; (before optimizer)\n     ; a.php:1-1\n     ; return  [] RANGE[0..0]\n0000 RETURN int(1)\n"
        );
    }

    #[test]
    fn a_function_prints_its_name_and_its_parameters_less_the_variadic_one() {
        let parameter = |variadic: bool| Parameter {
            name: Name::from("x"),
            ty: None,
            by_ref: false,
            variadic,
            default: None,
            attributes: Vec::new(),
        };
        let decl = FunctionDecl {
            name: Name::from("f"),
            parameters: vec![parameter(false), parameter(false), parameter(true)],
            return_type: None,
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name::from("a.php"),
            line: 4,
        };
        let function = OpArray {
            line_start: 4,
            line_end: 6,
            ..body(vec![], vec![])
        };
        let unit = Unit {
            main: body(vec![], vec![]),
            functions: vec![(decl, function)],
            ..Unit::default()
        };
        assert!(unit.dump().ends_with(
            "\nf:\n     ; (lines=0, args=2, vars=0, tmps=0)\n     ; (before optimizer)\n     ; a.php:4-6\n     ; return  [] RANGE[0..0]\n"
        ));
    }

    /// `zend_dump_const`: each type of constant printed as the value and
    /// never as the slot.
    #[test]
    fn a_constant_operand_is_the_literal_by_type() {
        let mut strings = php_heap::Strings::new();
        let literals = vec![
            Zval::null(),
            Zval::bool(false),
            Zval::bool(true),
            Zval::long(-7),
            Zval::double(1.5),
            text(&mut strings, b"x\"y\n\\\x01\xC3\xA9\x1b\x7f"),
        ];
        let echo = |at| {
            with(
                op(Opcode::Echo),
                OperandKind::Const,
                at,
                OperandKind::Unused,
                0,
            )
        };
        let body = body((0..6).map(echo).collect(), literals);
        assert_eq!(line(&body, 0), "ECHO null");
        assert_eq!(line(&body, 1), "ECHO bool(false)");
        assert_eq!(line(&body, 2), "ECHO bool(true)");
        assert_eq!(line(&body, 3), "ECHO int(-7)");
        assert_eq!(line(&body, 4), "ECHO float(1.5)");
        assert_eq!(
            line(&body, 5),
            "ECHO string(\"x\\\"y\\n\\\\\\x01\\xC3\\xA9\\e\\x7F\")"
        );
    }

    /// The oracle over `$a = 1.5; $c = 1e25; $d = 0.00001; $e = -0.0;
    /// $f = 100000.0; $g = 1000000.0; $h = 3.14159265; $i = 123456.789;`
    /// prints `float(1.5)`, `float(1e+25)`, `float(1e-05)`, `float(-0)`,
    /// `float(100000)`, `float(1e+06)`, `float(3.14159)` and
    /// `float(123457)`.
    #[test]
    fn a_double_is_printed_as_c_s_percent_g_prints_it() {
        for (value, text) in [
            (1.5, "1.5"),
            (1e25, "1e+25"),
            (0.00001, "1e-05"),
            (-0.0, "-0"),
            (100_000.0, "100000"),
            (1_000_000.0, "1e+06"),
            (1.234_567_89, "1.23457"),
            (123_456.789, "123457"),
            (0.0001, "0.0001"),
            (1.0, "1"),
            (0.5, "0.5"),
            (1e100, "1e+100"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ] {
            assert_eq!(percent_g(value), text, "{value}");
        }
    }

    /// `zend_dump_var`: a CV by number and name, a temporary by its slot
    /// past the CVs, and an `X` for a CV the op array has no name for.
    #[test]
    fn a_variable_operand_is_numbered_as_php_numbers_the_frame() {
        let mut body = body(
            vec![
                with(op(Opcode::Add), OperandKind::Cv, 0, OperandKind::Tmp, 0),
                with(
                    op(Opcode::Echo),
                    OperandKind::Var,
                    1,
                    OperandKind::Unused,
                    0,
                ),
                with(op(Opcode::Echo), OperandKind::Cv, 5, OperandKind::Unused, 0),
            ],
            vec![],
        );
        body.vars = vec![Name::from("a"), Name::from("b")].into_boxed_slice();
        body.num_cvs = 2;
        body.num_tmps = 2;
        assert_eq!(line(&body, 0), "ADD CV0($a) T2");
        assert_eq!(line(&body, 1), "ECHO V3");
        assert_eq!(line(&body, 2), "ECHO X5");
    }

    /// The oracle over `function f($x) { $x++; return $x; } $a = f(1);
    /// 1 + $a;` prints `CV0($x) = RECV 1`, `V1 = DO_UCALL` and
    /// `T3 = ADD int(1) CV0($a)`: the operand `result` names opens the line, a
    /// temporary numbered past the CVs as `zend_dump_var` numbers it.
    #[test]
    fn an_out_of_kind_tmp_var_or_cv_opens_the_line() {
        let writing = |op: Opline, out_type, out| Opline {
            out_type,
            out,
            ..op
        };
        let mut body = body(
            vec![
                writing(
                    with(
                        op(Opcode::Recv),
                        OperandKind::Unused,
                        1,
                        OperandKind::Unused,
                        0,
                    ),
                    OperandKind::Cv,
                    0,
                ),
                writing(op(Opcode::DoUcall), OperandKind::Var, 0),
                writing(
                    with(op(Opcode::Add), OperandKind::Const, 0, OperandKind::Cv, 0),
                    OperandKind::Tmp,
                    2,
                ),
            ],
            vec![Zval::long(1)],
        );
        body.vars = vec![Name::from("x")].into_boxed_slice();
        body.num_cvs = 1;
        body.num_tmps = 3;
        assert_eq!(line(&body, 0), "CV0($x) = RECV 1");
        assert_eq!(line(&body, 1), "V1 = DO_UCALL");
        assert_eq!(line(&body, 2), "T3 = ADD int(1) CV0($x)");
    }

    /// The same oracle prints `ASSIGN CV0($a) V1` and `PRE_INC CV0($x)`
    /// bare, the temporaries `zend_do_free` marked `UNUSED` and still
    /// numbered, and `JMP 0004` bare, a site whose `result` is its
    /// safepoint number: a `result` of kind `UNUSED` opens nothing,
    /// whatever the number in the field.
    #[test]
    fn an_out_of_kind_unused_prints_nothing_whatever_its_number() {
        let mut body = body(
            vec![
                with(op(Opcode::Assign), OperandKind::Cv, 0, OperandKind::Var, 1),
                Opline {
                    out: 2,
                    ..with(
                        op(Opcode::PreInc),
                        OperandKind::Cv,
                        0,
                        OperandKind::Unused,
                        0,
                    )
                },
                Opline {
                    out: 1,
                    ..with(
                        op(Opcode::Jmp),
                        OperandKind::Unused,
                        4,
                        OperandKind::Unused,
                        0,
                    )
                },
            ],
            vec![],
        );
        body.vars = vec![Name::from("a")].into_boxed_slice();
        body.num_cvs = 1;
        body.num_tmps = 3;
        assert_eq!(line(&body, 0), "ASSIGN CV0($a) V2");
        assert_eq!(line(&body, 1), "PRE_INC CV0($a)");
        assert_eq!(line(&body, 2), "JMP 0004");
    }

    /// The oracle over `set_time_limit(1); for (;;) {}` prints
    /// `JMP 0004` and `JMPNZ bool(true) 0004`: a target in `op1` and one
    /// in `op2`, each the opline's index.
    #[test]
    fn a_jump_target_is_printed_from_the_field_that_holds_it() {
        let body = body(
            vec![
                with(
                    op(Opcode::Jmp),
                    OperandKind::Unused,
                    4,
                    OperandKind::Unused,
                    0,
                ),
                with(
                    op(Opcode::Jmpnz),
                    OperandKind::Const,
                    0,
                    OperandKind::Unused,
                    4,
                ),
                Opline {
                    extended_value: 7,
                    ..with(
                        op(Opcode::FeFetchR),
                        OperandKind::Var,
                        0,
                        OperandKind::Unused,
                        0,
                    )
                },
            ],
            vec![Zval::bool(true)],
        );
        assert_eq!(line(&body, 0), "JMP 0004");
        assert_eq!(line(&body, 1), "JMPNZ bool(true) 0004");
        assert_eq!(line(&body, 2), "FE_FETCH_R V0 0007");
    }

    /// The oracle over `set_time_limit(1)` prints
    /// `INIT_FCALL 1 96 string("set_time_limit")`, `SEND_VAL int(1) 1`
    /// and `DO_ICALL`: the argument count from `extended_value`, the used
    /// stack from `op1`, the argument's number from `op2`, and nothing
    /// for a field whose shape prints nothing.
    #[test]
    fn a_number_in_an_unused_field_is_printed_where_its_shape_says_so() {
        let mut strings = php_heap::Strings::new();
        let name = text(&mut strings, b"set_time_limit");
        let body = body(
            vec![
                Opline {
                    extended_value: 1,
                    ..with(
                        op(Opcode::InitFcall),
                        OperandKind::Unused,
                        96,
                        OperandKind::Const,
                        0,
                    )
                },
                with(
                    op(Opcode::SendVal),
                    OperandKind::Const,
                    1,
                    OperandKind::Unused,
                    1,
                ),
                with(
                    op(Opcode::DoIcall),
                    OperandKind::Unused,
                    0,
                    OperandKind::Unused,
                    0,
                ),
                with(
                    op(Opcode::Recv),
                    OperandKind::Unused,
                    1,
                    OperandKind::Unused,
                    0,
                ),
            ],
            vec![name, Zval::long(1)],
        );
        assert_eq!(line(&body, 0), "INIT_FCALL 1 96 string(\"set_time_limit\")");
        assert_eq!(line(&body, 1), "SEND_VAL int(1) 1");
        assert_eq!(line(&body, 2), "DO_ICALL");
        assert_eq!(line(&body, 3), "RECV 1");
    }

    /// The oracle over `catch (Exception $e)` as the last catch prints
    /// `CATCH string("Exception")` and no target; `FETCH_CLASS`,
    /// `FETCH_CONSTANT`, `FREE` and `FAST_RET` print what
    /// `zend_dump_unused_op` reads from the field.
    #[test]
    fn an_unused_field_prints_what_its_shape_says() {
        let mut strings = php_heap::Strings::new();
        let name = text(&mut strings, b"Exception");
        let body = body(
            vec![
                Opline {
                    extended_value: LAST_CATCH,
                    ..with(
                        op(Opcode::Catch),
                        OperandKind::Const,
                        0,
                        OperandKind::Unused,
                        9,
                    )
                },
                with(
                    op(Opcode::Catch),
                    OperandKind::Const,
                    0,
                    OperandKind::Unused,
                    9,
                ),
                with(
                    op(Opcode::FetchClass),
                    OperandKind::Unused,
                    0x181,
                    OperandKind::Const,
                    0,
                ),
                with(
                    op(Opcode::FetchConstant),
                    OperandKind::Unused,
                    0x800,
                    OperandKind::Const,
                    0,
                ),
                Opline {
                    extended_value: FREE_ON_RETURN,
                    ..with(
                        op(Opcode::Free),
                        OperandKind::Tmp,
                        0,
                        OperandKind::Unused,
                        3,
                    )
                },
                with(
                    op(Opcode::FastRet),
                    OperandKind::Tmp,
                    0,
                    OperandKind::Unused,
                    u32::MAX,
                ),
                with(
                    op(Opcode::FastRet),
                    OperandKind::Tmp,
                    0,
                    OperandKind::Unused,
                    2,
                ),
                with(
                    op(Opcode::FetchThis),
                    OperandKind::Unused,
                    0,
                    OperandKind::Unused,
                    0,
                ),
                with(
                    op(Opcode::InitMethodCall),
                    OperandKind::Unused,
                    0,
                    OperandKind::Const,
                    0,
                ),
            ],
            vec![name],
        );
        assert_eq!(line(&body, 0), "CATCH string(\"Exception\")");
        assert_eq!(line(&body, 1), "CATCH string(\"Exception\") 0009");
        assert_eq!(
            line(&body, 2),
            "FETCH_CLASS (self) (no-autoload) (silent) string(\"Exception\")"
        );
        assert_eq!(
            line(&body, 3),
            "FETCH_CONSTANT (unqualified-in-namespace) string(\"Exception\")"
        );
        assert_eq!(line(&body, 4), "FREE T0 loop-end(+3)");
        assert_eq!(line(&body, 5), "FAST_RET T0");
        assert_eq!(line(&body, 6), "FAST_RET T0 try-catch(2)");
        assert_eq!(line(&body, 7), "FETCH_THIS");
        assert_eq!(
            line(&body, 8),
            "INIT_METHOD_CALL 0 THIS string(\"Exception\")"
        );
    }

    /// The oracle over `is_scalar($a)` and `is_int($a)` prints
    /// `TYPE_CHECK TYPE [bool, long, double, string] CV0($a)` and
    /// `TYPE_CHECK (long) CV0($a)`; the other shapes of
    /// `extended_value` print as `zend_dump_op` prints them.
    #[test]
    fn extended_value_is_printed_by_its_shape() {
        let rows: [(Opcode, u32, OperandKind, &str); 11] = [
            (
                Opcode::TypeCheck,
                0x07c,
                OperandKind::Unused,
                "TYPE_CHECK TYPE [bool, long, double, string] CV0($a)",
            ),
            (
                Opcode::TypeCheck,
                0x010,
                OperandKind::Unused,
                "TYPE_CHECK (long) CV0($a)",
            ),
            (Opcode::Cast, 4, OperandKind::Unused, "CAST (long) CV0($a)"),
            (
                Opcode::AssignOp,
                1,
                OperandKind::Cv,
                "ASSIGN_OP (ADD) CV0($a) CV0($a)",
            ),
            (
                Opcode::IncludeOrEval,
                2,
                OperandKind::Unused,
                "INCLUDE_OR_EVAL (include) CV0($a)",
            ),
            (
                Opcode::FetchR,
                2,
                OperandKind::Unused,
                "FETCH_R (global) CV0($a)",
            ),
            (
                Opcode::IssetIsemptyVar,
                1,
                OperandKind::Unused,
                "ISSET_ISEMPTY_VAR (empty) CV0($a)",
            ),
            (
                Opcode::InitArray,
                (3 << 2) | 2,
                OperandKind::Unused,
                "INIT_ARRAY 3 CV0($a) NEXT",
            ),
            (
                Opcode::FetchObjW,
                2,
                OperandKind::Cv,
                "FETCH_OBJ_W (dim write) CV0($a) CV0($a)",
            ),
            (Opcode::Return, 2, OperandKind::Unused, "RETURN CV0($a)"),
            (
                Opcode::ReturnByRef,
                2,
                OperandKind::Unused,
                "RETURN_BY_REF (value) CV0($a)",
            ),
        ];
        let oplines = rows
            .iter()
            .map(|&(opcode, extended_value, second, _)| Opline {
                extended_value,
                ..with(op(opcode), OperandKind::Cv, 0, second, 0)
            })
            .collect();
        let body = OpArray {
            vars: Box::new([Name::from("a")]),
            num_cvs: 1,
            ..body(oplines, vec![])
        };
        for (at, row) in rows.iter().enumerate() {
            assert_eq!(line(&body, at), row.3, "{:?}", row.0);
        }
    }

    /// The oracle prints `EXCEPTION TABLE:` and `     0033, 0037, 0040,
    /// 0041` after the oplines, a `-` for an index php left zero.
    #[test]
    fn the_exception_table_follows_the_oplines() {
        let ranges = [
            TryCatch {
                try_op: OplineIdx(33),
                catch_op: OplineIdx(37),
                finally_op: OplineIdx(40),
                finally_end: OplineIdx(41),
            },
            TryCatch {
                try_op: OplineIdx(2),
                catch_op: OplineIdx(0),
                finally_op: OplineIdx(0),
                finally_end: OplineIdx(0),
            },
        ];
        let unit = Unit {
            main: OpArray {
                try_catch: Box::new(ranges),
                ..body(vec![], vec![])
            },
            ..Unit::default()
        };
        assert!(
            unit.dump()
                .ends_with("EXCEPTION TABLE:\n     0033, 0037, 0040, 0041\n     0002, -, -, -\n")
        );
    }
}
