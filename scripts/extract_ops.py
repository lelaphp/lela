#!/usr/bin/env python3
"""extract_ops.py -- derive ops.toml from a php-src checkout.

Usage:  python3 scripts/extract_ops.py /path/to/php-src > ops.toml

Reads (all relative to the php-src root given in argv[1]):
  main/php_version.h                 the php version, printed as the tag
  Zend/zend_vm_opcodes.h             the opcode numbers (the enum is generated from these)
  Zend/zend_vm_opcodes.c             zend_vm_opcodes_names / zend_vm_opcodes_flags (cross-check + ext)
  Zend/zend_vm_gen.php               the operand / extended_value shorthand tables
  Zend/zend_vm_def.h                 the handler and helper definitions and their bodies
  Zend/Optimizer/zend_inference.c    zend_may_throw_ex, the source of the `throw` effect
  Zend/zend_compile.c                the emit calls for the six safepoint opcodes (result cross-check)

Everything below the hand tables is derived from those files. The hand tables are
the only judgments in this file; each one says why it cannot be derived.
"""

import os
import re
import sys

# ---------------------------------------------------------------------------
# Hand tables. Each carries the reason it is not derived.
# ---------------------------------------------------------------------------

# The opcodes specialised on their operand kinds, ops.toml's `specialise`, which
# third-engine-design.md section 5.8 renders. Not a property of php-src: an opcode
# php's zend_vm_gen.php specialises joins in the commit that measures its run-time
# operand decoding (section 5.8, #2997). php's own SPEC(...) clause is emitted
# beside it in `spec` so the two can be compared.
HOT_LIST = {
    "ASSIGN", "ADD", "SUB", "MUL", "IS_SMALLER", "IS_EQUAL", "IS_IDENTICAL",
    "JMPZ", "JMPNZ", "FETCH_DIM_R", "FETCH_DIM_W", "ASSIGN_DIM", "FETCH_OBJ_R",
    "FETCH_OBJ_W", "ASSIGN_OBJ", "SEND_VAR", "SEND_VAL", "INIT_FCALL", "DO_UCALL",
    "DO_ICALL", "RETURN", "PRE_INC", "CONCAT", "FAST_CONCAT", "ROPE_INIT",
    "ROPE_ADD", "ROPE_END", "FE_FETCH_R", "COUNT", "ISSET_ISEMPTY_DIM_OBJ",
    "FETCH_DIM_FUNC_ARG", "ASSIGN_DIM_OP", "FETCH_DIM_RW", "PRE_DEC",
}

# Safepoint candidates (design section 5.7 / jit-design section 3.1) and the
# opline field the design wants to carry the safepoint number. A design choice;
# the script only *verifies* that the field is free (see verify_safepoints).
SAFEPOINT_FIELD = {
    "JMP": "result", "JMPZ": "result", "JMPNZ": "result",
    "RETURN": "result", "RETURN_BY_REF": "result", "GENERATOR_RETURN": "result",
}

# `call` effect. Criterion: the opcode's purpose is to transfer control into a
# callee frame or to run user code (a PHP function, an included file, a
# generator/fiber switch, a registered callable). "Purpose" is not a text
# property of a handler body: FETCH_OBJ_R may run __get and CAST may run
# __toString, but neither exists to call anything, while DO_ICALL's body has no
# ZEND_VM_ENTER at all (it calls the C handler directly). So this is a hand
# list. INIT_* and NEW set up a frame but do not enter it (NEW's constructor is
# a separate DO_FCALL), so they are not `call`. TICKS is in because its purpose
# is to run the register_tick_function callables. USER_OPCODE runs an
# extension's C handler, not user code, and is out. The script prints the
# mechanical call-site pattern hits to stderr so this list can be re-checked.
CALL_OPCODES = {
    "DO_FCALL", "DO_ICALL", "DO_UCALL", "DO_FCALL_BY_NAME", "INCLUDE_OR_EVAL",
    "YIELD", "YIELD_FROM", "CALL_TRAMPOLINE", "TICKS",
    "FRAMELESS_ICALL_0", "FRAMELESS_ICALL_1", "FRAMELESS_ICALL_2", "FRAMELESS_ICALL_3",
}

# `write_globals` exclusions. The text patterns (WRITE_GLOBALS_PATTERNS) hit the
# shared zend_fetch_var_address_helper, which serves every FETCH_* fetch type; in
# that helper the symbol-table inserts (zend_hash_add_new / zend_hash_update)
# are under `type == BP_VAR_W` and `type == BP_VAR_RW` only, and the `type`
# argument is a per-caller parameter the text scan cannot follow. Likewise
# ISSET_ISEMPTY_VAR and FETCH_GLOBALS only read the table. These are read-only
# users of the pattern and are excluded by hand, with the source line of the
# read-only path noted.
WRITE_GLOBALS_EXCLUDE = {
    "FETCH_R":          "helper: BP_VAR_R path warns, never inserts",
    "FETCH_IS":         "helper: BP_VAR_IS path returns uninitialized_zval, never inserts",
    "FETCH_UNSET":      "helper: BP_VAR_UNSET path returns uninitialized_zval, never inserts",
    "ISSET_ISEMPTY_VAR": "zend_hash_find_ex on the target table only",
    "FETCH_GLOBALS":    "reads EG(symbol_table) into a $GLOBALS array copy",
}

# ---------------------------------------------------------------------------
# Text patterns used for the body-derived effects. Stated here verbatim so the
# header of ops.toml can quote them.
# ---------------------------------------------------------------------------

# `alloc`: the transitive handler body (handler + every helper/handler it
# dispatches to) matches one of these. Exception construction is not counted
# (that is the `throw` effect). Binary/unary arithmetic kernels are counted
# because array union, string increment etc. allocate.
ALLOC_PATTERNS = [
    r"\bemalloc\b", r"\becalloc\b", r"\berealloc\b", r"\bsafe_emalloc\b",
    r"\bzend_string_alloc\b", r"\bzend_string_safe_alloc\b", r"\bzend_string_init\b",
    r"\bzend_string_extend\b", r"\bzend_string_realloc\b", r"\bzend_string_concat\d\b",
    r"\bzend_string_concat\b", r"\bzend_strpprintf\b", r"\bzend_long_to_str\b",
    r"\bzend_double_to_str\b", r"\bzval_get_string\w*\b", r"\bzval_try_get_string\w*\b",
    r"\bzend_new_array\b", r"\bzend_new_pair\b", r"\barray_init\b", r"\bzend_array_dup\b",
    r"\bzend_array_to_list\b", r"\bzend_hash_(?:next_index_)?(?:add|update|insert)\w*\b",
    r"\bzend_hash_real_init\w*\b", r"\bzend_hash_extend\b", r"\bzend_hash_packed_grow\b",
    r"\bSEPARATE_ARRAY\b", r"\bSEPARATE_ZVAL\w*\b", r"\bZVAL_ARR\s*\(\s*\w+\s*,\s*zend_array_dup\b",
    r"\bZVAL_NEW_REF\b", r"\bZVAL_MAKE_REF\w*\b", r"\bZVAL_NEW_EMPTY_REF\b", r"\bzend_make_ref\b",
    r"\bobject_init\w*\b", r"\bzend_objects_new\b", r"\bzend_object_alloc\b", r"\bzend_objects_clone_obj\b",
    r"\bzend_create_closure\w*\b", r"\bzend_closure_from_frame\b", r"\bzend_create_fake_closure\b",
    r"\bzend_generator_create_zval\b", r"\bzend_vm_stack_push_call_frame\w*\b",
    r"\bzend_vm_stack_extend\w*\b", r"\bzend_vm_stack_copy_call_frame\b",
    r"\b(?:add|sub|mul|div|mod|pow|concat|shift_left|shift_right|bitwise_or|bitwise_and|bitwise_xor|bitwise_not|increment|decrement)_function\b",
    r"\bzend_binary_op\b", r"\bZEND_TRY_BINARY_OP\w*\b", r"\bzend_fetch_dimension_address\w*\b",
    r"\bzend_fetch_property_address\b", r"\bzend_assign_to_string_offset\b",
    r"\bzend_concat_function\b", r"\bzend_copy_extra_args\b",
    r"\bzend_rope_\w+\b", r"\bzend_frameless_observed_call_copy\b",
]

# `write_globals`: the transitive body matches one of these outside a
# ZEND_ASSERT( line. zend_get_target_symbol_table() is how the var-fetch
# helper, UNSET_VAR and ISSET_ISEMPTY_VAR reach EG(symbol_table) for the global
# fetch type; BIND_GLOBAL and FETCH_GLOBALS name EG(symbol_table) directly;
# static_variables_ptr is the static-variable binding table (BIND_STATIC,
# BIND_INIT_STATIC_OR_JMP).
WRITE_GLOBALS_PATTERNS = [
    r"zend_get_target_symbol_table\s*\(", r"EG\(symbol_table\)", r"static_variables_ptr",
]

# Cross-check patterns for `call` (stderr only).
CALL_SITE_PATTERNS = [
    r"ZEND_VM_ENTER_EX\(\)", r"ZEND_VM_ENTER\(\)", r"internal_function\.handler\(",
    r"frameless_function", r"zend_include_or_eval\(", r"zend_generator_yield_from\(",
    r"ZEND_VM_RETURN\(\)", r"zend_call_function\(", r"zend_execute_ex\(",
]

# `result`: the transitive body references the result operand through one of
# these. RETURN_VALUE_USED(opline) expands to `opline->result_type != IS_UNUSED`
# and ZEND_VM_SMART_BRANCH writes EX_VAR(opline->result.var) in its NONE form.
# OPLINE_CC passes the opline to an out-of-line zend_execute.c function (e.g.
# zend_fetch_dimension_address_W, zend_fetch_this_var), which is how the FETCH_*
# W/RW/IS/UNSET family writes EX_VAR(opline->result.var); a call that hands the
# opline out is presumed to address the result.
RESULT_PATTERNS = [r"opline->result", r"RETURN_VALUE_USED", r"ZEND_VM_SMART_BRANCH", r"OPLINE_CC"]

# Operand shorthand expansion, from $vm_op_decode in zend_vm_gen.php: the
# entries carrying ZEND_VM_OP_SPEC name zval operand kinds; the others (NUM,
# JMP_ADDR, TRY_CATCH, LOOP_END, THIS, NEXT, CLASS_FETCH, CONSTRUCTOR,
# CONST_FETCH, CACHE_SLOT) mean the operand field is not a zval and the opline's
# opN_type is IS_UNUSED (parse_operand_spec turns them into "ANY" for
# specialisation). ANY is every kind.
ATOMS = ["CONST", "TMP", "VAR", "CV", "UNUSED"]
SHORTHAND = {
    "CONST": ["CONST"], "TMP": ["TMP"], "VAR": ["VAR"], "CV": ["CV"], "UNUSED": ["UNUSED"],
    "TMPVAR": ["TMP", "VAR"], "TMPVARCV": ["TMP", "VAR", "CV"], "ANY": list(ATOMS),
}

# Name -> phrase, for the `doc` of handlers whose body does not open with a
# comment. Longest token sequence wins; unknown tokens are lowercased.
NAME_WORDS = {
    ("ISSET", "ISEMPTY"): "isset/empty of", ("FUNC", "ARG"): "for function argument",
    ("DIM", "IS"): "dimension for isset", ("OBJ", "IS"): "property for isset", ("PROP", "IS"): "property for isset",
    ("FETCH", "IS"): "fetch for isset", ("DIM", "UNSET"): "dimension for unset", ("OBJ", "UNSET"): "property for unset",
    ("PROP", "UNSET"): "property for unset", ("FETCH", "UNSET"): "fetch for unset", ("CASE", "STRICT"): "strict case compare",
    ("PROP", "OBJ"): "property", ("DIM", "OBJ"): "dimension or property", ("VAR", "EX"): "variable, callee unknown",
    ("VAL", "EX"): "value, callee unknown", ("NO", "REF", "EX"): "without reference, callee unknown",
    ("FUNC", "NUM", "ARGS"): "func_num_args()", ("FUNC", "GET", "ARGS"): "func_get_args()",
    ("JMP", "SET"): "jump if truthy, keeping the value", ("JMP", "NULL"): "nullsafe jump if null",
    ("JMP", "FRAMELESS"): "jump over a frameless call when the function is redefined",
    ("RECV", "INIT"): "receive argument with default", ("FE", "FREE"): "free foreach iterator",
    ("QM", "ASSIGN"): "assign to temporary", ("COPY", "TMP"): "copy temporary",
    ("BY", "NAME"): "by name", ("BY", "REF"): "by reference", ("NO", "REF"): "without reference",
    ("STATIC", "PROP"): "static property", ("FAST", "CALL"): "call finally block",
    ("FAST", "RET"): "return from finally block", ("OR", "JMP"): "or jump",
    ("NOP",): "no-op", ("ADD",): "add", ("SUB",): "subtract", ("MUL",): "multiply", ("DIV",): "divide",
    ("MOD",): "modulo", ("SL",): "shift left", ("SR",): "shift right", ("CONCAT",): "concatenate",
    ("BW",): "bitwise", ("POW",): "power", ("BOOL",): "boolean", ("NOT",): "not", ("XOR",): "xor",
    ("IS",): "is", ("IDENTICAL",): "identical", ("EQUAL",): "equal", ("SMALLER",): "smaller",
    ("OR",): "or", ("AND",): "and", ("ASSIGN",): "assign", ("DIM",): "dimension", ("OBJ",): "property",
    ("OP",): "with operator", ("REF",): "reference", ("QM",): "ternary", ("PRE",): "pre",
    ("POST",): "post", ("INC",): "increment", ("DEC",): "decrement", ("JMP",): "jump",
    ("JMPZ",): "jump if false", ("JMPNZ",): "jump if true", ("EX",): "keeping the value",
    ("CASE",): "loose case compare", ("CHECK",): "check", ("VAR",): "variable", ("CAST",): "cast",
    ("FAST",): "fast", ("ROPE",): "rope", ("INIT",): "init", ("END",): "end", ("BEGIN",): "begin",
    ("SILENCE",): "silence", ("FCALL",): "function call", ("DO",): "perform", ("RETURN",): "return",
    ("RECV",): "receive argument", ("SEND",): "send argument", ("VAL",): "value", ("NEW",): "new object",
    ("NS",): "namespaced", ("FREE",): "free", ("ARRAY",): "array", ("ELEMENT",): "element",
    ("INCLUDE",): "include", ("EVAL",): "eval", ("UNSET",): "unset", ("FE",): "foreach",
    ("RESET",): "reset", ("FETCH",): "fetch", ("R",): "for read", ("W",): "for write",
    ("RW",): "for read-write", ("LIST",): "list()", ("CONSTANT",): "constant", ("EXT",): "extension hook",
    ("STMT",): "statement", ("TICKS",): "ticks", ("CATCH",): "catch", ("THROW",): "throw",
    ("CLASS",): "class", ("CLONE",): "clone", ("METHOD",): "method", ("CALL",): "call",
    ("USER",): "user", ("STRLEN",): "string length", ("DEFINED",): "defined", ("TYPE",): "type",
    ("VERIFY",): "verify", ("DYNAMIC",): "dynamic", ("ICALL",): "internal call", ("UCALL",): "user call",
    ("ECHO",): "echo", ("DATA",): "data", ("INSTANCEOF",): "instanceof", ("GENERATOR",): "generator",
    ("CREATE",): "create", ("MAKE",): "make", ("DECLARE",): "declare", ("FUNCTION",): "function",
    ("LAMBDA",): "closure", ("CONST",): "constant", ("DELAYED",): "delayed", ("ANON",): "anonymous",
    ("UNPACK",): "unpack", ("HANDLE",): "handle", ("EXCEPTION",): "exception", ("OPCODE",): "opcode",
    ("ASSERT",): "assert", ("SET",): "set", ("CV",): "compiled variable", ("SEPARATE",): "separate",
    ("NAME",): "name", ("TRAMPOLINE",): "trampoline", ("DISCARD",): "discard", ("YIELD",): "yield",
    ("FROM",): "from", ("VARIADIC",): "variadic", ("COPY",): "copy", ("TMP",): "temporary",
    ("BIND",): "bind", ("GLOBAL",): "global", ("COALESCE",): "coalesce", ("SPACESHIP",): "spaceship",
    ("NUM",): "number of", ("GET",): "get", ("ARGS",): "arguments", ("LEXICAL",): "lexical",
    ("THIS",): "$this", ("SWITCH",): "switch on", ("LONG",): "integer", ("STRING",): "string",
    ("IN",): "in", ("COUNT",): "count", ("CALLED",): "called", ("KEY",): "key", ("EXISTS",): "exists",
    ("MATCH",): "match", ("STRICT",): "strict", ("ERROR",): "error", ("NULL",): "null",
    ("UNDEF",): "undefined", ("GLOBALS",): "$GLOBALS", ("NEVER",): "never", ("CALLABLE",): "callable",
    ("CONVERT",): "convert", ("STATIC",): "static", ("FRAMELESS",): "frameless", ("PARENT",): "parent",
    ("PROPERTY",): "property", ("HOOK",): "hook", ("ATTRIBUTED",): "attributed", ("PROP",): "property",
}

# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------

def read(root, rel):
    with open(os.path.join(root, rel), encoding="utf-8", errors="replace") as f:
        return f.read()


def die(msg):
    sys.stderr.write("extract_ops: " + msg + "\n")
    sys.exit(1)


def note(msg):
    sys.stderr.write("extract_ops: " + msg + "\n")


def php_version(root):
    m = re.search(r'#define PHP_VERSION "([^"]+)"', read(root, "main/php_version.h"))
    return m.group(1) if m else "unknown"


def opcode_numbers(root):
    """num -> name from zend_vm_opcodes.h; plus the ZEND_VM_* flag defines."""
    text = read(root, "Zend/zend_vm_opcodes.h")
    nums, flags = {}, {}
    for m in re.finditer(r"^#define ZEND_([A-Z0-9_]+)\s+(0x[0-9a-fA-F]+|\d+)\s*$", text, re.M):
        name, val = m.group(1), m.group(2)
        if name.startswith("VM_"):
            flags[name] = int(val, 0)
        elif not val.startswith("0x"):
            nums[int(val)] = name
    last = flags.get("VM_LAST_OPCODE")
    if last is None or max(nums) != last:
        die("ZEND_VM_LAST_OPCODE does not match the highest opcode define")
    return nums, flags


def opcode_tables_c(root):
    """names[] and flags[] arrays from zend_vm_opcodes.c, indexed by number."""
    text = read(root, "Zend/zend_vm_opcodes.c")
    m = re.search(r"zend_vm_opcodes_names\[\d+\] = \{(.*?)\};", text, re.S)
    names = [s or None for s, _null in re.findall(r'"([^"]*)"|(NULL)', m.group(1))]
    m = re.search(r"zend_vm_opcodes_flags\[\d+\] = \{(.*?)\};", text, re.S)
    flags = [int(x, 16) for x in re.findall(r"0x[0-9a-fA-F]+", m.group(1))]
    return names, flags


def gen_tables(root):
    """$vm_op_flags, $vm_op_decode, $vm_ext_decode from zend_vm_gen.php."""
    text = read(root, "Zend/zend_vm_gen.php")

    def block(name):
        m = re.search(r"\$" + name + r"\s*=\s*array\((.*?)\);", text, re.S)
        if not m:
            die("cannot find $" + name + " in zend_vm_gen.php")
        return m.group(1)

    op_flags = {}
    for m in re.finditer(r'"(ZEND_VM_[A-Z_]+)"\s*=>\s*([^,\n]+),', block("vm_op_flags")):
        expr = m.group(2).strip()
        op_flags[m.group(1)] = eval(expr, {"__builtins__": {}}, {})  # literals like 1<<3, 0x20 only

    def decode(name):
        out = {}
        for m in re.finditer(r'"([A-Z_]+)"\s*=>\s*([^,\n]+),', block(name)):
            expr = m.group(2).strip()
            out[m.group(1)] = eval(expr, {"__builtins__": {}}, dict(op_flags))
        return out

    return op_flags, decode("vm_op_decode"), decode("vm_ext_decode")


HANDLER_RE = re.compile(
    r"^ZEND_VM_(HOT_|INLINE_|HOT_OBJ_|HOT_SEND_|HOT_NOCONST_|HOT_NOCONSTCONST_|COLD_|COLD_CONST_|COLD_CONSTCONST_)?HANDLER\("
    r"\s*([0-9]+)\s*,\s*([A-Z_][A-Z0-9_]*)\s*,\s*([A-Z_|]+)\s*,\s*([A-Z_|]+)\s*"
    r"(?:,\s*([A-Z_|]+)\s*)?(?:,\s*SPEC\(([A-Z_|=,]+)\)\s*)?\)\s*$")
TYPE_SPEC_RE = re.compile(
    r"^ZEND_VM_(HOT_|INLINE_|HOT_OBJ_|HOT_SEND_|HOT_NOCONST_|HOT_NOCONSTCONST_)?TYPE_SPEC_HANDLER\("
    r"\s*([A-Z_|]+)\s*,\s*(.*?),\s*([A-Za-z_]+)\s*,\s*([A-Z_|]+)\s*,\s*([A-Z_|]+)\s*"
    r"(?:,\s*([A-Z_|]+)\s*)?(?:,\s*SPEC\(([A-Z_|=,]+)\)\s*)?\)\s*$")
HELPER_RE = re.compile(
    r"^ZEND_VM(_INLINE|_COLD|_HOT)?_HELPER\(\s*([A-Za-z_]+)\s*,\s*([A-Z_|]+)\s*,\s*([A-Z_|]+)\s*"
    r"(?:,\s*SPEC\(([A-Z_|=,]+)\)\s*)?(?:,\s*([^)]*)\s*)?\)\s*$")
DEFINE_OP_RE = re.compile(r"^ZEND_VM_DEFINE_OP\(\s*([0-9]+)\s*,\s*([A-Z_]+)\s*\);")


def parse_vm_def(root):
    """Handlers, type-spec handlers, helpers, define-ops, each with its body text."""
    lines = read(root, "Zend/zend_vm_def.h").split("\n")
    items = []  # (kind, dict, start_line)
    for i, line in enumerate(lines, 1):
        if line.startswith("ZEND_VM_") and ("HANDLER(" in line or "HELPER(" in line or "DEFINE_OP(" in line):
            m = HANDLER_RE.match(line)
            if m:
                items.append(("handler", {
                    "flavour": (m.group(1) or "").rstrip("_") or "PLAIN", "num": int(m.group(2)),
                    "name": m.group(3), "op1_raw": m.group(4), "op2_raw": m.group(5),
                    "ext_raw": m.group(6), "spec": m.group(7) or "", "line": i}, i))
                continue
            m = TYPE_SPEC_RE.match(line)
            if m:
                items.append(("type_spec", {
                    "flavour": (m.group(1) or "").rstrip("_") or "PLAIN", "parents": m.group(2).split("|"),
                    "cond": m.group(3), "name": m.group(4), "op1_raw": m.group(5), "op2_raw": m.group(6),
                    "ext_raw": m.group(7), "spec": m.group(8) or "", "line": i}, i))
                continue
            m = HELPER_RE.match(line)
            if m:
                items.append(("helper", {
                    "flavour": (m.group(1) or "").lstrip("_") or "PLAIN", "name": m.group(2),
                    "op1_raw": m.group(3), "op2_raw": m.group(4), "spec": m.group(5) or "",
                    "params": (m.group(6) or "").strip(), "line": i}, i))
                continue
            m = DEFINE_OP_RE.match(line)
            if m:
                items.append(("define_op", {"num": int(m.group(1)), "name": m.group(2), "line": i}, i))
                continue
            die("unparsed definition line %d: %s" % (i, line))
    # body = text up to the next definition line
    for idx, (kind, d, start) in enumerate(items):
        end = items[idx + 1][2] - 1 if idx + 1 < len(items) else len(lines)
        d["body"] = "\n".join(lines[start:end])
    return items


def expand_operand(raw, op_decode, op_flags):
    """Atoms accepted for an operand spec like CONST|TMPVARCV, plus its flag bits."""
    flags = 0
    atoms = []
    for part in raw.split("|"):
        if part not in op_decode:
            die("unknown operand shorthand " + part)
        flags |= op_decode[part]
        if part in SHORTHAND:
            for a in SHORTHAND[part]:
                if a not in atoms:
                    atoms.append(a)
    if not (flags & op_decode["TMP"]):  # no ZEND_VM_OP_SPEC bit: NUM/JMP_ADDR/...; the field is not a zval
        atoms = ["UNUSED"]
    return [a for a in ATOMS if a in atoms], flags


def ext_flags(raw, ext_decode):
    f = 0
    if raw:
        for part in raw.split("|"):
            if part not in ext_decode:
                die("unknown extended_value shorthand " + part)
            f |= ext_decode[part]
    return f


def ext_from_flags(flags, ext_decode):
    """Decode the ZEND_VM_EXT_* bits of a zend_vm_opcodes_flags entry back into names."""
    names = []
    mask_val = flags & 0x0f000000
    for name, val in ext_decode.items():
        if val & 0x0f000000:
            if val == mask_val:
                names.append(name)
        elif flags & val:
            names.append(name)
    return "|".join(sorted(names, key=lambda n: ext_decode[n]))


# ---------------------------------------------------------------------------
# zend_may_throw_ex
# ---------------------------------------------------------------------------

def parse_may_throw(root):
    text = read(root, "Zend/Optimizer/zend_inference.c")
    start = text.index("ZEND_API bool zend_may_throw_ex(")
    end = text.index("ZEND_API bool zend_may_throw(", start)
    fn = text[start:end]
    switches = [m.start() for m in re.finditer(r"switch \(opline->opcode\) \{", fn)]
    if len(switches) != 5:
        die("zend_may_throw_ex: expected 5 opcode switches, found %d" % len(switches))
    # The four preambles, in source order, each guarded by a known condition.
    guards = ["opline->op1_type == IS_CV", "opline->op1_type & (IS_TMP_VAR|IS_VAR)",
              "opline->op2_type == IS_CV", "opline->op2_type & (IS_TMP_VAR|IS_VAR)"]
    exempt = []
    for k in range(4):
        pre = fn[:switches[k]]
        if guards[k] not in pre[pre.rfind("if (opline->op"):]:
            die("zend_may_throw_ex: preamble %d is not guarded by %s" % (k, guards[k]))
        seg = fn[switches[k]:switches[k + 1]]
        brk = seg.index("break;")
        exempt.append(set(re.findall(r"case ZEND_([A-Z0-9_]+):", seg[:brk])))
    main = fn[switches[4]:]
    # case groups of the main switch: labels then statements up to the next label
    # case labels at the switch's own indentation, each followed by its statements;
    # consecutive labels share the statements that follow the last of them
    groups, pending = {}, []
    for m in re.finditer(r"(?m)^\t\t(case ZEND_([A-Z0-9_]+):|default:)[ \t]*\n(.*?)(?=^\t\t(?:case ZEND_|default:)|\Z)", main, re.S):
        pending.append(m.group(2) or "*default*")
        body = re.sub(r"/\*.*?\*/", "", m.group(3), flags=re.S)
        body = re.sub(r"\s+", " ", body).strip()
        if body:
            for l in pending:
                groups[l] = body
            pending = []
    if pending or "*default*" not in groups:
        die("zend_may_throw_ex: main switch parse left labels without statements")
    return exempt, groups


def throw_verdict(name, op1, op2, exempt, groups):
    """(throws: bool, condition text or '')."""
    cv_undef1, dtor1, cv_undef2, dtor2 = exempt
    body = groups.get(name, groups["*default*"])
    conds = []
    if body != "return 0;":
        if body in ("return 1;", groups["*default*"]) and name not in groups:
            conds.append("default (zend_may_throw_ex has no case)")
        elif body == "return 1;":
            conds.append("always")
        else:
            conds.append(body if len(body) <= 240 else body[:240] + " ...")
    if "CV" in op1 and name not in cv_undef1:
        conds.append("op1 CV undefined -> warning")
    if ("TMP" in op1 or "VAR" in op1) and name not in dtor1:
        conds.append("op1 TMP/VAR destructor")
    if "CV" in op2 and name not in cv_undef2:
        conds.append("op2 CV undefined -> warning")
    if ("TMP" in op2 or "VAR" in op2) and name not in dtor2:
        conds.append("op2 TMP/VAR destructor")
    return (bool(conds), "; ".join(conds))


# ---------------------------------------------------------------------------
# zend_compile.c cross-check for the safepoint opcodes
# ---------------------------------------------------------------------------

def compile_emit_results(root, names):
    """For each name: list of (line, first-arg text) of zend_emit_op*/cond-jump calls naming it."""
    text = read(root, "Zend/zend_compile.c")
    found = {n: [] for n in names}
    call_re = re.compile(r"zend_emit_op(?:_tmp)?\(\s*([^,]+?)\s*,\s*([^;]*?)\)\s*;", re.S)
    for m in call_re.finditer(text):
        for n in names:
            if re.search(r"\bZEND_" + n + r"\b", m.group(2)):
                found[n].append((text[:m.start()].count("\n") + 1, m.group(1)))
    # jumps go through zend_emit_jump / zend_emit_cond_jump; check their emit call
    for fname, target in (("zend_emit_jump", "JMP"), ("zend_emit_cond_jump", ("JMPZ", "JMPNZ"))):
        s = text.find("uint32_t " + fname + "(")
        seg = text[s:text.find("\n}\n", s)]
        m = re.search(r"zend_emit_op\(\s*([^,]+?)\s*,", seg)
        ln = text[:s].count("\n") + 1 + seg[:m.start()].count("\n")
        for n in (target if isinstance(target, tuple) else (target,)):
            found[n].append((ln, m.group(1) + " (in " + fname + ")"))
    for n in ("JMPZ", "JMPNZ"):  # every caller of zend_emit_cond_jump names one of the two
        for m in re.finditer(r"zend_emit_cond_jump\(\s*ZEND_" + n + r"\b", text):
            found[n].append((text[:m.start()].count("\n") + 1, "NULL (via zend_emit_cond_jump)"))
    return found


# ---------------------------------------------------------------------------
# Doc phrase
# ---------------------------------------------------------------------------

def leading_comment(body):
    m = re.match(r"\s*\{\s*/\*(.*?)\*/", body, re.S)
    if not m:
        return None
    text = re.sub(r"\s+", " ", m.group(1)).strip().rstrip(".")
    return text if len(text) <= 90 else None  # a long opening comment is an implementation note, not a doc


def name_phrase(name):
    toks = name.split("_")
    out, i = [], 0
    while i < len(toks):
        for n in (3, 2, 1):
            key = tuple(toks[i:i + n])
            if key in NAME_WORDS:
                out.append(NAME_WORDS[key])
                i += n
                break
        else:
            out.append(toks[i].lower())
            i += 1
    return " ".join(out)


# ---------------------------------------------------------------------------
# TOML emission (stdlib has no writer; the shapes here are strings, ints,
# bools and string arrays only)
# ---------------------------------------------------------------------------

def q(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def arr(xs):
    return "[" + ", ".join(q(x) for x in xs) + "]"


# ---------------------------------------------------------------------------

def main():
    if len(sys.argv) != 2:
        die("usage: extract_ops.py /path/to/php-src")
    root = sys.argv[1]
    version = php_version(root)
    nums, _hflags = opcode_numbers(root)
    c_names, c_flags = opcode_tables_c(root)
    op_flags, op_decode, ext_decode = gen_tables(root)
    items = parse_vm_def(root)
    exempt, throw_groups = parse_may_throw(root)

    handlers = {d["num"]: d for k, d, _ in items if k == "handler"}
    define_ops = {d["num"]: d for k, d, _ in items if k == "define_op"}
    helpers = {d["name"]: d for k, d, _ in items if k == "helper"}
    type_specs = [d for k, d, _ in items if k == "type_spec"]
    by_name = {d["name"]: d for d in handlers.values()}

    # -- verification: every number in the .h has exactly one definition, names agree
    for num, name in nums.items():
        if (num in handlers) == (num in define_ops):
            die("opcode %d %s: expected exactly one of handler/define-op" % (num, name))
        d = handlers.get(num) or define_ops[num]
        if d["name"] != "ZEND_" + name:
            die("opcode %d: .h says %s, def.h says %s" % (num, name, d["name"]))
        if c_names[num] != "ZEND_" + name:
            die("opcode %d: zend_vm_opcodes.c name mismatch" % num)
    for num in list(handlers) + list(define_ops):
        if num not in nums:
            die("opcode %d defined in zend_vm_def.h but absent from zend_vm_opcodes.h" % num)

    # -- transitive bodies
    body_cache = {}

    def transitive_body(key, seen=None):
        if key in body_cache:
            return body_cache[key]
        seen = seen or set()
        if key in seen:
            return ""
        seen.add(key)
        d = helpers.get(key) or by_name.get(key)
        if d is None:
            return ""
        text = d["body"]
        for h in re.findall(r"ZEND_VM_DISPATCH_TO_HELPER\(\s*(\w+)", text):
            text += "\n" + transitive_body(h, seen)
        for h in re.findall(r"ZEND_VM_DISPATCH_TO_HANDLER\(\s*(\w+)", text):
            text += "\n" + transitive_body(h, seen)
        body_cache[key] = text
        return text

    def hits(text, patterns, skip_assert=False):
        if skip_assert:
            text = "\n".join(l for l in text.split("\n") if "ZEND_ASSERT(" not in l)
        return [p for p in patterns if re.search(p, text)]

    # -- assemble ops
    ops = []
    call_pattern_hits = []
    for num in sorted(nums):
        name = nums[num]
        if num in define_ops:
            ops.append({"num": num, "name": name, "kind": "DEFINE_OP", "line": define_ops[num]["line"],
                        "op1": list(ATOMS), "op1_raw": "ANY", "op2": list(ATOMS), "op2_raw": "ANY",
                        "result": "unused", "ext": "", "effects": [], "throw_cond": "", "spec": "",
                        "specialise": False, "type_spec": [],
                        "doc": "extra operand opline consumed by the preceding opline; no handler"})
            continue
        d = handlers[num]
        op1, f1 = expand_operand(d["op1_raw"], op_decode, op_flags)
        op2, f2 = expand_operand(d["op2_raw"], op_decode, op_flags)
        flags = f1 | (f2 << 8) | ext_flags(d["ext_raw"], ext_decode)
        spec_rules = [s.split("=")[0].strip() for s in d["spec"].split(",") if s]
        if "NO_CONST_CONST" in spec_rules:
            flags |= op_flags["ZEND_VM_NO_CONST_CONST"]
        if "COMMUTATIVE" in spec_rules:
            flags |= op_flags["ZEND_VM_COMMUTATIVE"]
        if flags != c_flags[num]:
            die("opcode %d %s: handler-line flags 0x%08x != zend_vm_opcodes.c 0x%08x" % (num, name, flags, c_flags[num]))
        ext = ext_from_flags(c_flags[num], ext_decode)
        body = transitive_body("ZEND_" + name)
        result = "used" if hits(body, RESULT_PATTERNS) else "unused"
        throws, cond = throw_verdict(name, op1, op2, exempt, throw_groups)
        effects = []
        if throws:
            effects.append("throw")
        if hits(body, ALLOC_PATTERNS):
            effects.append("alloc")
        if name in CALL_OPCODES:
            effects.append("call")
        if hits(body, CALL_SITE_PATTERNS):
            call_pattern_hits.append(name)
        if hits(body, WRITE_GLOBALS_PATTERNS, skip_assert=True) and name not in WRITE_GLOBALS_EXCLUDE:
            effects.append("write_globals")
        doc = leading_comment(d["body"]) or name_phrase(name)
        ops.append({"num": num, "name": name, "kind": d["flavour"], "line": d["line"],
                    "op1": op1, "op1_raw": d["op1_raw"], "op2": op2, "op2_raw": d["op2_raw"],
                    "result": result, "ext": ext, "effects": effects, "throw_cond": cond if throws else "",
                    "spec": d["spec"], "specialise": name in HOT_LIST,
                    "type_spec": [t["name"][5:] for t in type_specs if ("ZEND_" + name) in t["parents"]],
                    "doc": doc})

    # -- safepoint verification: handler leaves result unused AND zend_compile.c emits with NULL result
    emits = compile_emit_results(root, list(SAFEPOINT_FIELD))
    safepoint_ok = {}
    for op in ops:
        n = op["name"]
        if n not in SAFEPOINT_FIELD:
            continue
        problems = []
        if op["result"] != "unused":
            problems.append("handler body references result")
        for ln, arg in emits[n]:
            if not arg.startswith("NULL"):
                problems.append("zend_compile.c:%d emits with result %s" % (ln, arg))
        if n != "GENERATOR_RETURN" and not emits[n]:
            problems.append("no zend_compile.c emit found")
        safepoint_ok[n] = problems
        if problems:
            note("safepoint %s NOT verified: %s" % (n, "; ".join(problems)))
        else:
            note("safepoint %s verified: result unused in handler; zend_compile.c emits at %s"
                 % (n, ", ".join("%d(%s)" % e for e in emits[n]) or "none (rewritten in pass_two)"))

    unlisted = sorted(set(call_pattern_hits) - CALL_OPCODES)
    note("call-site pattern hits not in CALL_OPCODES (review): " + ", ".join(unlisted))
    note("CALL_OPCODES without a call-site pattern hit (review): "
         + ", ".join(sorted(CALL_OPCODES - set(call_pattern_hits))))

    # -- counts
    flav = {}
    for d in handlers.values():
        flav[d["flavour"]] = flav.get(d["flavour"], 0) + 1
    hflav = {}
    for d in helpers.values():
        hflav[d["flavour"]] = hflav.get(d["flavour"], 0) + 1

    out = []
    w = out.append
    w("# ops.toml -- the instruction set of php, extracted from php-src at tag php-%s." % version)
    w("# Generated by: python3 scripts/extract_ops.py <php-src root> > ops.toml")
    w("# Do not edit; re-run the script at a new php tag.")
    w("#")
    w("# num/name: Zend/zend_vm_opcodes.h. op1/op2: the ZEND_VM_HANDLER line, expanded into the")
    w("#   zval kinds CONST/TMP/VAR/CV/UNUSED per $vm_op_decode in zend_vm_gen.php; a shorthand without")
    w("#   the SPEC bit (NUM, JMP_ADDR, TRY_CATCH, LOOP_END, THIS, NEXT, CLASS_FETCH, CONSTRUCTOR,")
    w("#   CONST_FETCH, CACHE_SLOT) means the field is not a zval and opN_type is IS_UNUSED, so it")
    w("#   expands to [UNUSED]; opN_raw keeps php's text. ext: the ZEND_VM_EXT_* bits of")
    w("#   zend_vm_opcodes_flags[] in Zend/zend_vm_opcodes.c (verified equal to the handler line).")
    w("# result: \"used\" when the transitive handler body (the handler plus every helper/handler it")
    w("#   dispatches to) matches %s." % " | ".join(RESULT_PATTERNS))
    w("# effects.throw: from zend_may_throw_ex (Zend/Optimizer/zend_inference.c): absent only when the")
    w("#   main switch returns 0 for the opcode unconditionally AND none of the four preamble checks")
    w("#   (op1/op2 undefined CV warning, op1/op2 TMP/VAR destructor) can apply to its operand kinds.")
    w("#   throw_cond records the condition text otherwise.")
    w("# effects.call: hand list; criterion: the opcode's purpose is to transfer control into a callee")
    w("#   frame or run user code (a PHP function, an included file, a generator switch, a registered")
    w("#   callable). NEW is not (its constructor is a separate DO_FCALL); INIT_* are not.")
    w("# effects.alloc: transitive body matches one of the allocation patterns:")
    for p in ALLOC_PATTERNS:
        w("#   " + p)
    w("# effects.write_globals: transitive body, minus ZEND_ASSERT( lines, matches one of:")
    for p in WRITE_GLOBALS_PATTERNS:
        w("#   " + p)
    w("#   minus the read-only users excluded by hand: " + ", ".join("%s (%s)" % kv for kv in sorted(WRITE_GLOBALS_EXCLUDE.items())))
    w("# spec: php's SPEC(...) clause verbatim; specialise: the design's hot list; type_spec: php's")
    w("#   ZEND_VM_*TYPE_SPEC_HANDLER variants of the opcode.")
    w("# safepoint: the field carrying the safepoint number, on the six site opcodes, emitted only when")
    w("#   the handler leaves result unused and every zend_compile.c emit passes NULL as the result.")
    w("# kind: the ZEND_VM_*HANDLER macro flavour; line: the line of that macro in Zend/zend_vm_def.h.")
    w("")
    w("[meta]")
    w("php_tag = %s" % q("php-" + version))
    w("php_version = %s" % q(version))
    w("opcode_count = %d" % len(nums))
    w("last_opcode = %d" % max(nums))
    w("unused_numbers = [%s]" % ", ".join(str(n) for n in range(max(nums) + 1) if n not in nums))
    w("handler_count = %d" % len(handlers))
    w("handler_count_excluding_inline = %d" % (len(handlers) - flav.get("INLINE", 0)))
    w("type_spec_handler_count = %d" % len(type_specs))
    w("helper_count = %d" % len(helpers))
    w("define_op_count = %d" % len(define_ops))
    w("")
    w("[meta.handlers_by_kind]")
    for k in sorted(flav):
        w("%s = %d" % (k, flav[k]))
    w("")
    w("[meta.helpers_by_kind]")
    for k in sorted(hflav):
        w("%s = %d" % (k, hflav[k]))
    w("")
    w("[meta.may_throw_preamble_exemptions]")
    for k, key in enumerate(("op1_cv_undef", "op1_tmpvar_dtor", "op2_cv_undef", "op2_tmpvar_dtor")):
        w("%s = %s" % (key, arr(sorted(exempt[k]))))
    w("")
    for op in ops:
        w("[[op]]")
        w("num = %d" % op["num"])
        w("name = %s" % q(op["name"]))
        w("kind = %s" % q(op["kind"]))
        w("line = %d" % op["line"])
        w("op1 = %s" % arr(op["op1"]))
        w("op1_raw = %s" % q(op["op1_raw"]))
        w("op2 = %s" % arr(op["op2"]))
        w("op2_raw = %s" % q(op["op2_raw"]))
        w("result = %s" % q(op["result"]))
        if op["ext"]:
            w("ext = %s" % q(op["ext"]))
        w("effects = %s" % arr(op["effects"]))
        if op["throw_cond"]:
            w("throw_cond = %s" % q(op["throw_cond"]))
        w("spec = %s" % q(op["spec"]))
        w("specialise = %s" % ("true" if op["specialise"] else "false"))
        if op["type_spec"]:
            w("type_spec = %s" % arr(op["type_spec"]))
        if op["name"] in SAFEPOINT_FIELD and not safepoint_ok[op["name"]]:
            w("safepoint = %s" % q(SAFEPOINT_FIELD[op["name"]]))
        w("doc = %s" % q(op["doc"]))
        w("")
    for name, d in sorted(helpers.items(), key=lambda kv: kv[1]["line"]):
        w("[[helper]]")
        w("name = %s" % q(name))
        w("kind = %s" % q(d["flavour"]))
        w("line = %d" % d["line"])
        w("op1_raw = %s" % q(d["op1_raw"]))
        w("op2_raw = %s" % q(d["op2_raw"]))
        if d["params"]:
            w("params = %s" % q(d["params"]))
        w("")
    sys.stdout.write("\n".join(out))


if __name__ == "__main__":
    main()
