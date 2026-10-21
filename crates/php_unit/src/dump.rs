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
//! `THIS`, `NEXT` or how a class is fetched. A `FRAMELESS_ICALL_*`
//! carries its function's name in parentheses after the opcode, read
//! from the table the caller hands in, `zend_flf_functions` by name.
//! The live-range table follows the oplines under
//! `ZEND_DUMP_LIVE_RANGES`, which the `before optimizer` dump passes,
//! and the `try`/`catch` table follows it, as php's do.
//!
//! **A jump table.** The `op2` of a `SWITCH_LONG`, `SWITCH_STRING` or
//! `MATCH` is the constant array `zend_compile_switch` and
//! `zend_compile_match` lay down, and `zend_dump_op` walks it in place
//! of `zend_dump_const`: one ` key: target,` per bucket, the string key
//! in `"` and unescaped as `%s` prints it, then ` default:` before the
//! `extended_value` target the `JMP_ADDR` shape prints.
//!
//! **Numbers.** php prints a temporary by its frame slot, counted past
//! the CVs (`EX_VAR_TO_NUM`); an opline holds the temporary's own
//! number, and the slot is `num_cvs` past it, which is what the engine's
//! `temporary_slot` computes. A jump target is the opline's index, as
//! the compiler stores it and as php prints it after subtracting the
//! op array's base.

use std::fmt::Write as _;

use php_heap::{Zval, ZvalType};

use crate::decl::{FunctionDecl, Name};
use crate::op::{LiveKind, OpArray, OperandKind, Opline};
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
pub(crate) mod op {
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
pub(crate) mod ext {
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
pub(crate) const LAST_CATCH: u32 = 1;
/// `ZEND_FREE_ON_RETURN`, on a `FREE`'s `extended_value`.
const FREE_ON_RETURN: u32 = 1;
/// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE`, on a `FETCH_CONSTANT`'s `op1`.
const UNQUALIFIED_IN_NAMESPACE: u32 = 0x800;

/// The dump as it is written: bytes and not text, since php's
/// `fprintf` prints a name, a jump table's key and the file name as the
/// bytes they are (`%s`, up to a NUL), whatever encoding they are in; only the words
/// around them are php's ASCII.
#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn push(&mut self, c: char) {
        self.push_str(c.encode_utf8(&mut [0; 4]));
    }

    fn push_str(&mut self, words: &str) {
        self.0.extend_from_slice(words.as_bytes());
    }

    /// A name, a key or a file name as `%s` prints a C string: byte for
    /// byte, up to its first NUL.
    fn c_string(&mut self, bytes: &[u8]) {
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        self.0.extend_from_slice(&bytes[..end]);
    }
}

impl std::fmt::Write for Out {
    fn write_str(&mut self, words: &str) -> std::fmt::Result {
        self.push_str(words);
        Ok(())
    }
}

/// Every op array of the unit, one block each, in php's order
/// (`zend_foreach_op_array`): the main op array, then the functions as
/// declared, then each class's methods as declared, named
/// `Class::method`, and its properties' hooks, `Class::$name::get`
/// before `Class::$name::set`, and after each of those the functions
/// written inside it that a `DECLARE_FUNCTION` binds, its
/// `dynamic_func_defs` in order, each followed by its own. `frameless`
/// is `zend_flf_functions` by name, what a `FRAMELESS_ICALL_*`'s
/// `extended_value` indexes.
pub(crate) fn unit(unit: &Unit, frameless: &[Name]) -> Vec<u8> {
    let mut out = Out::default();
    op_array(&mut out, b"$_main", 0, &unit.main, frameless);
    dynamic_defs(&mut out, unit, &unit.main, frameless, 0);
    for (decl, body) in &unit.functions {
        op_array(&mut out, &decl.name.0, num_args(decl), body, frameless);
        dynamic_defs(&mut out, unit, body, frameless, 0);
    }
    // `CG(class_table)`'s order, as each declaration completed
    // (`Unit::completed`); a unit built by hand without one is read in
    // declaration order.
    let declared = 0..u32::try_from(unit.classes.len()).unwrap_or(u32::MAX);
    let order: Vec<u32> = if unit.completed.is_empty() {
        declared.collect()
    } else {
        unit.completed.clone()
    };
    for at in order {
        let Some(class) = unit.classes.get(at as usize) else {
            continue;
        };
        let methods = unit.methods.iter().filter(|method| method.class == at);
        let mut hooks: Vec<_> = unit.hooks.iter().filter(|hook| hook.class == at).collect();
        hooks.sort_by_key(|hook| (hook.property, hook.kind == crate::decl::HookKind::Set));
        // `!(op_array->fn_flags & ZEND_ACC_ABSTRACT)` holds the function
        // table alone: an abstract method's body, its `RECV` prologue
        // alone, is no op array `zend_foreach_op_array` walks, while a
        // property's hooks are walked by scope whether abstract or not,
        // so an interface's `get;` prints its `RETURN null`.
        let bodies = methods
            .filter_map(|method| {
                class
                    .methods
                    .get(method.method as usize)
                    .filter(|decl| !decl.is_abstract)
                    .map(|decl| (decl, &method.body))
            })
            .chain(hooks.into_iter().filter_map(|hook| {
                class
                    .properties
                    .get(hook.property as usize)
                    .and_then(|property| property.hooks.as_ref())
                    .and_then(|hooks| hooks.of(hook.kind))
                    .map(|decl| (decl, &hook.body))
            }));
        for (decl, body) in bodies {
            let name = [
                crate::decl::shown_bytes(&class.name.0),
                b"::",
                &decl.function.name.0,
            ]
            .concat();
            op_array(&mut out, &name, num_args(&decl.function), body, frameless);
            dynamic_defs(&mut out, unit, body, frameless, 0);
        }
    }
    out.0
}

/// `zend_foreach_op_array_helper`'s recursion: the op arrays of
/// `body`'s `dynamic_func_defs`, in order, each one followed by its
/// own. `depth` cuts a table that names an op array already on the way
/// down, which no compiler here writes and which would otherwise never
/// end.
fn dynamic_defs(out: &mut Out, unit: &Unit, body: &OpArray, frameless: &[Name], depth: u32) {
    if depth > 64 {
        return;
    }
    for at in &body.dynamic_func_defs {
        let Some((decl, nested)) = unit.dynamic.get(*at as usize) else {
            continue;
        };
        op_array(out, &decl.name.0, num_args(decl), nested, frameless);
        dynamic_defs(out, unit, nested, frameless, depth.saturating_add(1));
    }
}

/// `op_array->num_args`: the declared parameters less the variadic one.
fn num_args(decl: &FunctionDecl) -> usize {
    decl.parameters.iter().filter(|p| !p.variadic).count()
}

/// The two shapes of `zend_dump_op_array` this crate prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// opcache's `before optimizer` dump: `ZEND_DUMP_LIVE_RANGES`, the
    /// message, and the return line of the zeroed `zend_func_info`.
    BeforeOptimizer,
    /// phpdbg's, `ZEND_DUMP_LINE_NUMBERS` alone: each opline opened by
    /// `L` and its line, and no message, return line or live ranges.
    LineNumbers,
}

/// `zend_dump_op_array` with `ZEND_DUMP_LIVE_RANGES` and the message
/// `before optimizer`: a blank line, the name, the counts, the message,
/// the file with the line range, and the return line the zeroed
/// `zend_func_info` of every op array prints before the optimizer has
/// inferred anything; then the oplines, the live-range table and the
/// `try`/`catch` table.
fn op_array(out: &mut Out, name: &[u8], args: usize, body: &OpArray, frameless: &[Name]) {
    shaped(out, Shape::BeforeOptimizer, (name, args), body, frameless);
}

/// `zend_dump_op_array(op_array, ZEND_DUMP_LINE_NUMBERS, NULL, NULL)`,
/// what phpdbg's `print` writes for a user function.
pub(crate) fn lines(name: &[u8], args: usize, body: &OpArray, frameless: &[Name]) -> Vec<u8> {
    let mut out = Out::default();
    shaped(&mut out, Shape::LineNumbers, (name, args), body, frameless);
    out.0
}

/// One op array in `shape`.
fn shaped(
    out: &mut Out,
    shape: Shape,
    (name, args): (&[u8], usize),
    body: &OpArray,
    frameless: &[Name],
) {
    out.push('\n');
    out.c_string(name);
    writeln!(
        out,
        ":\n     ; (lines={}, args={args}, vars={}, tmps={})",
        body.oplines.len(),
        body.num_cvs,
        body.num_tmps
    )
    .ok();
    if shape == Shape::BeforeOptimizer {
        writeln!(out, "     ; (before optimizer)").ok();
    }
    out.push_str("     ; ");
    out.c_string(&body.filename.0);
    writeln!(out, ":{}-{}", body.line_start, body.line_end).ok();
    if shape == Shape::BeforeOptimizer {
        writeln!(out, "     ; return  [] RANGE[0..0]").ok();
    }
    for (at, op) in body.oplines.iter().enumerate() {
        if shape == Shape::LineNumbers {
            write!(out, "L{:04} ", op.lineno).ok();
        }
        write!(out, "{at:04} ").ok();
        opline(out, body, op, frameless);
        out.push('\n');
    }
    if shape == Shape::BeforeOptimizer && !body.live_ranges.is_empty() {
        writeln!(out, "LIVE RANGES:").ok();
        for range in &body.live_ranges {
            writeln!(
                out,
                "     {}: {:04} - {:04} ({})",
                body.num_cvs.saturating_add(range.var),
                range.start.0,
                range.end.0,
                live_kind(range.kind)
            )
            .ok();
        }
    }
    if !body.try_catch.is_empty() {
        writeln!(out, "EXCEPTION TABLE:").ok();
        for range in &body.try_catch {
            write!(out, "     {:04}", range.try_op.0).ok();
            for at in [range.catch_op.0, range.finally_op.0, range.finally_end.0] {
                if at == 0 {
                    out.push_str(", -");
                } else {
                    write!(out, ", {at:04}").ok();
                }
            }
            out.push('\n');
        }
    }
}

/// `zend_dump_op` with no CFG and no SSA: the operand `result` names
/// and ` = ` when its kind is `TMP`, `VAR` or `CV`, the name, a
/// frameless call's function in parentheses, the `extended_value` by
/// its shape, `op1`, `op2`, and the target an `extended_value` of the
/// `JMP_ADDR` shape holds. A `result` of kind `UNUSED` opens nothing,
/// whatever the number the field holds: a site's safepoint number, or
/// the temporary php still numbered where `zend_do_free` set the mark.
fn opline(out: &mut Out, body: &OpArray, op: &Opline, frameless: &[Name]) {
    let flags = op.opcode.flags();
    let name = op.opcode.name();
    if op.out_type != OperandKind::Unused && op.out_type != OperandKind::Const {
        var(out, body, op.out_type, op.out);
        out.push_str(" = ");
    }
    if name.is_empty() {
        write!(out, "OP_{}", op.opcode.num()).ok();
    } else {
        out.push_str(name.trim_start_matches("ZEND_"));
    }
    if op.opcode.is_frameless_icall() {
        frameless_name(out, op, frameless);
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
        write!(out, " {:04}", op.extended_value).ok();
    }
    // A constant `result`, which no opline of `zend_compile.c` has, is
    // printed last where php prints it.
    if op.out_type == OperandKind::Const {
        constant(out, body.literals.get(op.out as usize).copied());
    }
}

/// `ZEND_FLF_FUNC(opline)->common.function_name` in parentheses, as
/// `zend_dump_op` prints it right after the opcode, `(str_contains)`.
/// An index the table has no name for prints `(?)`, where php would read
/// past its table.
fn frameless_name(out: &mut Out, op: &Opline, frameless: &[Name]) {
    out.push('(');
    match frameless.get(op.extended_value as usize) {
        Some(name) => out.c_string(&name.0),
        None => out.push('?'),
    }
    out.push(')');
}

/// The word `zend_dump_op_array` prints after a live range, by its kind.
fn live_kind(kind: LiveKind) -> &'static str {
    match kind {
        LiveKind::TmpVar => "tmp/var",
        LiveKind::Loop => "loop",
        LiveKind::Silence => "silence",
        LiveKind::Rope => "rope",
        LiveKind::New => "new",
    }
}

/// One operand: the constant, the variable, or what an unused field says.
fn operand(
    out: &mut Out,
    body: &OpArray,
    op: &Opline,
    kind: OperandKind,
    field: u32,
    flags: u32,
    first: bool,
) {
    match kind {
        OperandKind::Const => {
            let value = body.literals.get(field as usize).copied();
            if !first
                && matches!(
                    op.opcode,
                    Opcode::SwitchLong | Opcode::SwitchString | Opcode::Match
                )
            {
                jump_table(out, value);
            } else {
                constant(out, value);
            }
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
                    write!(out, " {field:04}").ok();
                }
            } else {
                unused(out, op, field, flags);
            }
        }
    }
}

/// The jump table of a `SWITCH_LONG`, `SWITCH_STRING` or `MATCH`, as
/// `zend_dump_op` prints `op2`'s array: ` key: target,` per bucket in
/// the table's order, a string key in `"` up to its first NUL as `%s`
/// reads it, then ` default:`, whose target is the `extended_value`
/// printed after it. A constant that is no array prints as one.
fn jump_table(out: &mut Out, value: Option<Zval>) {
    let Some(table) = value.and_then(php_heap::Zval::as_arr) else {
        constant(out, value);
        return;
    };
    for (key, target) in &table {
        match key {
            php_heap::Key::Long(n) => {
                write!(out, " {n}:").ok();
            }
            php_heap::Key::Str(s) => {
                out.push_str(" \"");
                out.c_string(s.bytes());
                out.push_str("\":");
            }
        }
        write!(out, " {:04},", target.as_long().unwrap_or(0)).ok();
    }
    out.push_str(" default:");
}

/// `zend_dump_const`: the constant by type, a string escaped as
/// `smart_str_append_escaped` escapes it with `"` escaped besides, a
/// double as C's `%g` prints it.
fn constant(out: &mut Out, value: Option<Zval>) {
    let Some(value) = value else {
        out.push_str(" zval(type=?)");
        return;
    };
    match value.kind() {
        ZvalType::Null => out.push_str(" null"),
        ZvalType::False => out.push_str(" bool(false)"),
        ZvalType::True => out.push_str(" bool(true)"),
        ZvalType::Long => {
            write!(out, " int({})", value.as_long().unwrap_or(0)).ok();
        }
        ZvalType::Double => {
            write!(
                out,
                " float({})",
                percent_g(value.as_double().unwrap_or(0.0))
            )
            .ok();
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
            write!(out, " zval(type={})", kind as u8).ok();
        }
    }
}

/// `zend_dump_var`: a CV by its number and name, a `VAR` or `TMP` by
/// its slot past the CVs, and `X` for a CV number the op array has no
/// name for.
fn var(out: &mut Out, body: &OpArray, kind: OperandKind, number: u32) {
    match kind {
        OperandKind::Cv if number < body.num_cvs => {
            let name = body
                .vars
                .get(number as usize)
                .map(|n| &*n.0)
                .unwrap_or_default();
            write!(out, "CV{number}($").ok();
            out.c_string(name);
            out.push(')');
        }
        OperandKind::Var => {
            write!(out, "V{}", body.num_cvs.saturating_add(number)).ok();
        }
        OperandKind::Tmp => {
            write!(out, "T{}", body.num_cvs.saturating_add(number)).ok();
        }
        _ => {
            write!(out, "X{number}").ok();
        }
    }
}

/// `zend_dump_unused_op`: what a field that holds no zval says, by the
/// shape its opcode gives it.
fn unused(out: &mut Out, op: &Opline, field: u32, flags: u32) {
    match flags & op::MASK {
        op::NUM => {
            write!(out, " {field}").ok();
        }
        op::TRY_CATCH if field != u32::MAX => {
            write!(out, " try-catch({field})").ok();
        }
        op::LOOP_END if op.extended_value & FREE_ON_RETURN != 0 => {
            write!(out, " loop-end(+{field})").ok();
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
fn class_fetch_type(out: &mut Out, fetch_type: u32) {
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
fn extended_value(out: &mut Out, op: &Opline, flags: u32) {
    let value = op.extended_value;
    match flags & ext::MASK {
        ext::NUM => {
            write!(out, " {value}").ok();
        }
        ext::OP => {
            let name = u8::try_from(value)
                .ok()
                .and_then(Opcode::from_u8)
                .map_or("", Opcode::name);
            write!(out, " ({})", name.trim_start_matches("ZEND_")).ok();
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
                write!(out, " {}", value >> 2).ok();
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
fn type_info(out: &mut Out, info: u32) {
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
    write!(out, " [{}]", words.join(", ")).ok();
}

/// `smart_str_append_escaped` with `"` escaped besides, as
/// `zend_dump_const` prints a string: the seven C escapes by letter, and
/// every other byte outside printable ASCII as `\x` and two upper-case
/// hex digits.
fn escaped(out: &mut Out, bytes: &[u8]) {
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
                write!(out, "\\x{c:02X}").ok();
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
    use crate::decl::{
        ClassDecl, ClassFlags, ClassKind, EnumBacking, MethodDecl, Name, Parameter, Visibility,
    };
    use crate::op::{LiveRange, OplineIdx, TryCatch};
    use crate::unit::MethodBody;

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
            oplines,
            literals,
            filename: Name::from("a.php"),
            line_start: 1,
            line_end: 1,
            ..OpArray::default()
        }
    }

    /// One opline printed on its own, without the header, with no
    /// frameless table.
    fn line(body: &OpArray, at: usize) -> String {
        let mut out = Out::default();
        let op = body.oplines.get(at).expect("an opline at the index asked");
        opline(&mut out, body, op, &[]);
        utf8(out.0)
    }

    /// A dump the tests below hold to text, every byte of it ASCII.
    fn utf8(dump: Vec<u8>) -> String {
        String::from_utf8(dump).expect("a dump of UTF-8")
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
            utf8(unit.dump(&[])),
            "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n     ; (before optimizer)\n     ; a.php:1-1\n     ; return  [] RANGE[0..0]\n0000 RETURN int(1)\n"
        );
    }

    #[test]
    fn a_function_prints_its_name_and_its_parameters_less_the_variadic_one() {
        let parameter = |variadic: bool| Parameter {
            name: Name::from("x"),
            ty: None,
            by_ref: false,
            prefer_ref: false,
            promoted: false,
            variadic,
            default: crate::decl::ParameterDefault::None,
            attributes: Vec::new(),
        };
        let decl = FunctionDecl {
            name: Name::from("f"),
            parameters: vec![parameter(false), parameter(false), parameter(true)],
            return_type: None,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name::from("a.php"),
            line: 4,
            end_line: 6,
            frameless: Vec::new(),
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
        assert!(utf8(unit.dump(&[])).ends_with(
            "\nf:\n     ; (lines=0, args=2, vars=0, tmps=0)\n     ; (before optimizer)\n     ; a.php:4-6\n     ; return  [] RANGE[0..0]\n"
        ));
    }

    /// `zend_foreach_op_array` reaches a class's methods after the
    /// functions, and each is named `Class::method` with its own
    /// parameter count; a body naming a class or a method the unit has
    /// not is nobody's and prints nothing.
    #[test]
    fn a_method_prints_after_the_functions_under_its_class_s_name() {
        let function = |name: &str, parameters: usize| FunctionDecl {
            name: Name::from(name),
            parameters: (0..parameters)
                .map(|_| Parameter {
                    name: Name::from("x"),
                    ty: None,
                    by_ref: false,
                    prefer_ref: false,
                    promoted: false,
                    variadic: false,
                    default: crate::decl::ParameterDefault::None,
                    attributes: Vec::new(),
                })
                .collect(),
            return_type: None,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name::from("a.php"),
            line: 4,
            end_line: 0,
            frameless: Vec::new(),
        };
        let method = |name: &str, parameters: usize| MethodDecl {
            function: function(name, parameters),
            visibility: Visibility::Public,
            is_instance: true,
            is_abstract: false,
            is_final: false,
            tentative_return: false,
        };
        let class = ClassDecl {
            name: Name::from("C"),
            kind: ClassKind::Class,
            flags: ClassFlags::default(),
            parent: None,
            interfaces: Vec::new(),
            implementors: Vec::new(),
            aliases: Vec::new(),
            traits: Vec::new(),
            adaptations: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: vec![method("m", 1), method("n", 0)],
            attributes: Vec::new(),
            doc_comment: None,
            file: Name::from("a.php"),
            line: 3,
            end_line: 9,
            object_size: 0,
            object_beside: Box::default(),
            attribute_validator: None,
        };
        let at = |line_start: u32| OpArray {
            line_start,
            line_end: line_start,
            ..body(vec![], vec![])
        };
        let unit = Unit {
            main: body(vec![], vec![]),
            functions: vec![(function("f", 0), at(2))],
            classes: vec![class],
            methods: vec![
                MethodBody {
                    class: 0,
                    method: 0,
                    body: at(4),
                },
                MethodBody {
                    class: 0,
                    method: 1,
                    body: at(6),
                },
                MethodBody {
                    class: 1,
                    method: 0,
                    body: at(8),
                },
            ],
            ..Unit::default()
        };
        let dump = utf8(unit.dump(&[]));
        let header = |name: &str, args: usize, line: u32| {
            format!(
                "\n{name}:\n     ; (lines=0, args={args}, vars=0, tmps=0)\n     ; (before optimizer)\n     ; a.php:{line}-{line}\n     ; return  [] RANGE[0..0]\n"
            )
        };
        assert!(dump.ends_with(&format!(
            "{}{}{}",
            header("f", 0, 2),
            header("C::m", 1, 4),
            header("C::n", 0, 6)
        )));
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
        body.vars = vec![Name::from("a"), Name::from("b")];
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
        body.vars = vec![Name::from("x")];
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
        body.vars = vec![Name::from("a")];
        body.num_cvs = 1;
        body.num_tmps = 3;
        assert_eq!(line(&body, 0), "ASSIGN CV0($a) V2");
        assert_eq!(line(&body, 1), "PRE_INC CV0($a)");
        assert_eq!(line(&body, 2), "JMP 0004");
    }

    /// The oracle over `$a = 1; echo match ($a) { 1 => "x", "s" => "y",
    /// default => "z" };` prints
    /// `MATCH CV0($a) 1: 0012, "s": 0014, default: 0016`: the table's
    /// buckets in order, then the `extended_value` target.
    #[test]
    fn a_jump_table_prints_its_buckets_and_the_default_target() {
        use php_heap::{Intern, Key, Strings};
        let mut strings = Strings::new();
        let s = strings.intern(b"s").expect("intern \"s\"");
        let table = strings
            .constant_array(&[
                (Key::Long(1), Zval::long(12)),
                (Key::Str(s), Zval::long(14)),
            ])
            .expect("a constant array");
        let mut body = body(
            vec![Opline {
                extended_value: 16,
                ..with(op(Opcode::Match), OperandKind::Cv, 0, OperandKind::Const, 0)
            }],
            vec![Zval::array(table)],
        );
        body.vars = vec![Name::from("a")];
        body.num_cvs = 1;
        assert_eq!(
            line(&body, 0),
            "MATCH CV0($a) 1: 0012, \"s\": 0014, default: 0016"
        );
        drop(strings);
    }

    /// The oracle over `ext/intl/tests/uconverter_oop_callback_return.phpt`
    /// prints `SWITCH_STRING CV2($codeUnits) "\x80": 0015, ...` with the
    /// key's own byte where UTF-8 has no character, as `%s` prints it;
    /// a function's name, a CV's and the file's are bytes alike, each up
    /// to its first NUL, as `zend_dump_var` prints the CV of
    /// `Zend/tests/heredoc_nowdoc/warning_during_heredoc_scan_ahead.phpt`.
    #[test]
    fn a_name_a_key_and_the_file_print_as_their_bytes() {
        use php_heap::{Intern, Key, Strings};
        let mut strings = Strings::new();
        let key = strings.intern(b"\x80").expect("intern a byte above 0x7f");
        let table = strings
            .constant_array(&[(Key::Str(key), Zval::long(1))])
            .expect("a constant array");
        let decl = FunctionDecl {
            name: Name(b"f\xff"[..].into()),
            parameters: Vec::new(),
            return_type: None,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(b"\xfe.php"[..].into()),
            line: 1,
            end_line: 1,
            frameless: Vec::new(),
        };
        let mut function = body(
            vec![Opline {
                extended_value: 1,
                ..with(
                    op(Opcode::SwitchString),
                    OperandKind::Cv,
                    0,
                    OperandKind::Const,
                    0,
                )
            }],
            vec![Zval::array(table)],
        );
        function.filename = Name(b"\xfe.php"[..].into());
        function.vars = vec![Name(b"\xe9\0after"[..].into())];
        function.num_cvs = 1;
        let unit = Unit {
            main: body(vec![], vec![]),
            functions: vec![(decl, function)],
            ..Unit::default()
        };
        let dump = unit.dump(&[]);
        let expected: &[u8] = b"\nf\xff:\n     ; (lines=1, args=0, vars=1, tmps=0)\n     ; (before optimizer)\n     ; \xfe.php:1-1\n     ; return  [] RANGE[0..0]\n0000 SWITCH_STRING CV0($\xe9) \"\x80\": 0001, default: 0001\n";
        assert!(dump.ends_with(expected), "{}", dump.escape_ascii());
        drop(strings);
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
            vars: vec![Name::from("a")],
            num_cvs: 1,
            ..body(oplines, vec![])
        };
        for (at, row) in rows.iter().enumerate() {
            assert_eq!(line(&body, at), row.3, "{:?}", row.0);
        }
    }

    /// The oracle over `$a = "x"; $b = str_contains($a, "y"); $t =
    /// trim($a);` prints `0001 T4 = FRAMELESS_ICALL_2(str_contains)
    /// CV0($a) string("y")` and `0003 T6 = FRAMELESS_ICALL_1(trim)
    /// CV0($a)`: the name in parentheses right after the opcode and
    /// before the operands, and no number for the index itself, since
    /// the four opcodes carry no `ZEND_VM_EXT_MASK` shape. Which index
    /// a name sits at is the order the natives registered their
    /// frameless entries in, so the table here is one of its own; an
    /// index the table has no name for prints `(?)`.
    #[test]
    fn a_frameless_call_prints_its_function_s_name_after_the_opcode() {
        let mut strings = php_heap::Strings::new();
        let needle = text(&mut strings, b"y");
        let call = |opcode, extended_value, out, second, other| Opline {
            extended_value,
            out_type: OperandKind::Tmp,
            out,
            ..with(op(opcode), OperandKind::Cv, 0, second, other)
        };
        let body = OpArray {
            vars: vec![Name::from("a"), Name::from("b"), Name::from("t")],
            num_cvs: 3,
            ..body(
                vec![
                    call(Opcode::FramelessIcall2, 1, 1, OperandKind::Const, 0),
                    call(Opcode::FramelessIcall1, 0, 3, OperandKind::Unused, 0),
                    call(Opcode::FramelessIcall1, 2, 3, OperandKind::Unused, 0),
                ],
                vec![needle],
            )
        };
        let table = [Name::from("trim"), Name::from("str_contains")];
        let line = |at: usize| {
            let mut out = Out::default();
            if let Some(op) = body.oplines.get(at) {
                opline(&mut out, &body, op, &table);
            }
            utf8(out.0)
        };
        assert_eq!(
            line(0),
            "T4 = FRAMELESS_ICALL_2(str_contains) CV0($a) string(\"y\")"
        );
        assert_eq!(line(1), "T6 = FRAMELESS_ICALL_1(trim) CV0($a)");
        assert_eq!(line(2), "T6 = FRAMELESS_ICALL_1(?) CV0($a)");
    }

    /// The oracle over eight CVs and the five kinds in one program,
    /// a `foreach`, a `@`, an interpolation, a `new` with an argument
    /// and a three-argument `strstr`, whose `OP_DATA` puts a temporary
    /// past its definition:
    ///
    /// ```text
    /// $a = "a"; $b = "b"; $c = false;
    /// foreach ([1] as $v) { @f($v); }
    /// $s = "$a$b$v";
    /// $o = new Foo(f(1));
    /// $r = strstr("needles", "needle", $c);
    /// try { f(2); } catch (Exception $e) {}
    /// ```
    ///
    /// prints `LIVE RANGES:` after the oplines and one line per range,
    /// the temporary's slot past the CVs, the range, and the kind's
    /// word, the ranges in the order of their starts; the
    /// `try`/`catch` table follows it.
    ///
    /// ```text
    /// LIVE RANGES:
    ///      11: 0004 - 0011 (loop)
    ///      12: 0006 - 0009 (silence)
    ///      15: 0012 - 0014 (rope)
    ///      18: 0017 - 0022 (new)
    ///      22: 0024 - 0025 (tmp/var)
    /// EXCEPTION TABLE:
    ///      0026, 0030, -, -
    /// ```
    #[test]
    fn the_live_range_table_follows_the_oplines_with_php_s_five_kinds() {
        let range = |var, kind, start, end| LiveRange {
            var,
            kind,
            start: OplineIdx(start),
            end: OplineIdx(end),
        };
        let unit = Unit {
            main: OpArray {
                num_cvs: 8,
                live_ranges: vec![
                    range(3, LiveKind::Loop, 4, 11),
                    range(4, LiveKind::Silence, 6, 9),
                    range(7, LiveKind::Rope, 12, 14),
                    range(10, LiveKind::New, 17, 22),
                    range(14, LiveKind::TmpVar, 24, 25),
                ],
                try_catch: vec![TryCatch {
                    try_op: OplineIdx(26),
                    catch_op: OplineIdx(30),
                    ..TryCatch::default()
                }],
                ..body(vec![], vec![])
            },
            ..Unit::default()
        };
        assert!(utf8(unit.dump(&[])).ends_with(
            "     ; return  [] RANGE[0..0]\nLIVE RANGES:\n     11: 0004 - 0011 (loop)\n     12: 0006 - 0009 (silence)\n     15: 0012 - 0014 (rope)\n     18: 0017 - 0022 (new)\n     22: 0024 - 0025 (tmp/var)\nEXCEPTION TABLE:\n     0026, 0030, -, -\n"
        ));
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
                try_catch: ranges.to_vec(),
                ..body(vec![], vec![])
            },
            ..Unit::default()
        };
        assert!(
            utf8(unit.dump(&[]))
                .ends_with("EXCEPTION TABLE:\n     0033, 0037, 0040, 0041\n     0002, -, -, -\n")
        );
    }
}
