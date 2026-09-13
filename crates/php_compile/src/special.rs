//! `zend_try_compile_special_func` (design §6.1): the names php's
//! compiler serves itself instead of emitting the call, in the order
//! `zend_try_compile_special_func_ex` tries them, and what each becomes
//! here. php gives such a call one of two shapes: a constant where the
//! arguments are constant, and an opcode of its own otherwise
//! (`ZEND_STRLEN`, `ZEND_TYPE_CHECK`, `ZEND_COUNT`, `ZEND_IN_ARRAY` and
//! their kin). This compiler serves the first shape for the four names
//! whose constant needs no opcode, `strlen`, `chr`, `ord` and `defined`;
//! every other row, and one of the four whose arguments do not fold, is
//! compiled as the ordinary `INIT_FCALL`, `SEND_*` and `DO_ICALL`, never
//! folded to something else, so the value is php's where the op array is
//! not yet. Which names the corpus reaches is `data/special-funcs.md`.
//!
//! Three of php's gates are not read here, because this compiler has
//! nothing to read them from: the function table, which php requires to
//! hold the name as one of its own natives (`fbc->type`, at
//! `zend_try_compile_special_func`), so a disabled and redeclared name
//! keeps its user body; the compiler option that turns the table off
//! (`ZEND_COMPILE_NO_BUILTINS`); and unpacked
//! or named arguments (`zend_args_contain_unpack_or_named`), which the
//! reader does not read. `chr` and `ord` fold only in a call read for its
//! value (`type == BP_VAR_R`), which every call here is.

use php_heap::Zval;

use crate::CompileError;
use crate::emit::Emit;
use crate::read::{BinaryOp, Expr};

/// What php's compiler makes of one name in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Special {
    /// `zend_compile_func_strlen`: the length of a constant string,
    /// `ZEND_STRLEN` over anything else.
    Strlen,
    /// `zend_compile_func_chr`: the one-byte string of a long written as
    /// such, 0 to 255; the call otherwise.
    Chr,
    /// `zend_compile_func_ord`: the byte of a one-byte string written as
    /// such; the call otherwise.
    Ord,
    /// `zend_compile_func_defined`: `true` for a name the compiler
    /// substitutes, `ZEND_DEFINED` over another name, the call for a
    /// name with a namespace or a class in it.
    Defined,
    /// A shape that is an opcode of its own (`ZEND_TYPE_CHECK`,
    /// `ZEND_BOOL`, `ZEND_CAST`, `ZEND_INIT_USER_CALL`, `ZEND_IN_ARRAY`,
    /// `ZEND_COUNT`, `ZEND_GET_CLASS`, `ZEND_GET_CALLED_CLASS`,
    /// `ZEND_GET_TYPE`, `ZEND_FUNC_NUM_ARGS`, `ZEND_FUNC_GET_ARGS`,
    /// `ZEND_ARRAY_KEY_EXISTS`, the ropes of `sprintf`, `ZEND_CLONE`) and
    /// has no kernel here yet: the ordinary call.
    Opcode,
}

/// `zend_try_compile_special_func_ex`'s names, in its order, each with
/// what this compiler makes of it. A row php reaches through two names
/// (`is_int` and `is_integer`, `count` and `sizeof`) is two rows here,
/// in the order the names stand in php's condition.
pub(crate) const TABLE: &[(&[u8], Special)] = &[
    (b"strlen", Special::Strlen),
    (b"is_null", Special::Opcode),
    (b"is_bool", Special::Opcode),
    (b"is_long", Special::Opcode),
    (b"is_int", Special::Opcode),
    (b"is_integer", Special::Opcode),
    (b"is_float", Special::Opcode),
    (b"is_double", Special::Opcode),
    (b"is_string", Special::Opcode),
    (b"is_array", Special::Opcode),
    (b"is_object", Special::Opcode),
    (b"is_resource", Special::Opcode),
    (b"is_scalar", Special::Opcode),
    (b"boolval", Special::Opcode),
    (b"intval", Special::Opcode),
    (b"floatval", Special::Opcode),
    (b"doubleval", Special::Opcode),
    (b"strval", Special::Opcode),
    (b"defined", Special::Defined),
    (b"chr", Special::Chr),
    (b"ord", Special::Ord),
    (b"call_user_func_array", Special::Opcode),
    (b"call_user_func", Special::Opcode),
    (b"in_array", Special::Opcode),
    (b"count", Special::Opcode),
    (b"sizeof", Special::Opcode),
    (b"get_class", Special::Opcode),
    (b"get_called_class", Special::Opcode),
    (b"gettype", Special::Opcode),
    (b"func_num_args", Special::Opcode),
    (b"func_get_args", Special::Opcode),
    (b"array_slice", Special::Opcode),
    (b"array_key_exists", Special::Opcode),
    (b"sprintf", Special::Opcode),
    (b"clone", Special::Opcode),
];

/// The row for a lowered, unqualified name, as
/// `zend_try_compile_special_func_ex` finds it: the first whose name
/// matches, and none for a name the table has not.
pub(crate) fn row(lowered: &[u8]) -> Option<Special> {
    TABLE
        .iter()
        .find(|(name, _)| *name == lowered)
        .map(|(_, shape)| *shape)
}

impl Emit<'_> {
    /// `zend_try_ct_eval_const` for a name with no namespace in it: php's
    /// special constants, `true`, `false` and `null` in any case
    /// (`zend_get_special_const`), then the persistent table.
    fn ct_eval_const(&mut self, name: &[u8]) -> Result<Option<Zval>, CompileError> {
        Ok(match name.to_ascii_lowercase().as_slice() {
            b"true" => Some(Zval::bool(true)),
            b"false" => Some(Zval::bool(false)),
            b"null" => Some(Zval::null()),
            _ => self.persistent(name)?,
        })
    }

    /// `zend_try_compile_special_func`: the constant a call in the table
    /// folds to, and nothing where the name is not in the table or php
    /// would emit an opcode, which is the ordinary call here.
    ///
    /// ```php
    /// var_dump(1 + strlen('a'), strlen('a' . 'b'), chr(65), ord('a'), defined('INF'), defined('true'));
    /// ```
    /// ```expect
    /// int(2)
    /// int(2)
    /// string(1) "A"
    /// int(97)
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn special(
        &mut self,
        lowered: &[u8],
        arguments: &[Expr],
    ) -> Result<Option<Zval>, CompileError> {
        match row(lowered) {
            Some(Special::Strlen) => Ok(self.strlen(arguments)),
            Some(Special::Chr) => self.chr(arguments),
            Some(Special::Ord) => self.ord(arguments),
            Some(Special::Defined) => self.defined(arguments),
            Some(Special::Opcode) | None => Ok(None),
        }
    }

    /// `zend_compile_func_strlen`: one argument, compiled, and a constant
    /// string is its length. php emits `ZEND_STRLEN` over anything else,
    /// a constant that is not a string included (`strlen(1)`), and that
    /// opcode has no kernel here yet, so the call is ordinary.
    fn strlen(&mut self, arguments: &[Expr]) -> Option<Zval> {
        let [argument] = arguments else {
            return None;
        };
        let value = self.constant_expr(argument).ok()?;
        value
            .as_str()
            .map(|s| Zval::long(i64::try_from(s.len()).unwrap_or(i64::MAX)))
    }

    /// `zend_compile_func_chr`: the one-byte string for a long written
    /// as such, 0 to 255 (`ZVAL_CHAR`). A long written any other way
    /// (`chr(1 + 64)`, `chr(-0)`), one outside the byte and anything
    /// else is the call, as php leaves it.
    fn chr(&mut self, arguments: &[Expr]) -> Result<Option<Zval>, CompileError> {
        let [argument] = arguments else {
            return Ok(None);
        };
        let Some(byte) = self
            .written(argument)?
            .and_then(Zval::as_long)
            .and_then(|n| u8::try_from(n).ok())
        else {
            return Ok(None);
        };
        Ok(Some(self.string(&[byte])?))
    }

    /// `zend_compile_func_ord`: the byte of a one-byte string written as
    /// such; a longer or an empty string, or anything else, is the call.
    fn ord(&mut self, arguments: &[Expr]) -> Result<Option<Zval>, CompileError> {
        let [argument] = arguments else {
            return Ok(None);
        };
        let Some(value) = self.written(argument)? else {
            return Ok(None);
        };
        Ok(value.as_str().and_then(|s| match s.bytes() {
            [byte] => Some(Zval::long(i64::from(*byte))),
            _ => None,
        }))
    }

    /// `zend_compile_func_defined`: one argument written as a value, read
    /// as a string (`zval_get_string`). A name with a `\` or a `:` in it
    /// is the call; a name the compiler substitutes where it is written
    /// ([`Emit::ct_eval_const`]) is `true`; and php emits `ZEND_DEFINED` over
    /// the rest, which has no kernel here yet, so that is the call too.
    fn defined(&mut self, arguments: &[Expr]) -> Result<Option<Zval>, CompileError> {
        let [argument] = arguments else {
            return Ok(None);
        };
        let Some(value) = self.written(argument)? else {
            return Ok(None);
        };
        let Some(name) = self.stringified(value)?.and_then(Zval::as_str) else {
            return Ok(None);
        };
        let name = name.bytes();
        if name.contains(&b'\\') || name.contains(&b':') {
            return Ok(None);
        }
        Ok(self.ct_eval_const(name)?.map(|_| Zval::bool(true)))
    }

    /// Whether an argument stands in php's tree as a value,
    /// `ZEND_AST_ZVAL`, and which: a number or a string as written, or a
    /// `.` over two of them, which php's parser folds before the compiler
    /// sees it (`zend_ast_create_concat_op`). `true`, `false`, `null` and
    /// a named constant are `ZEND_AST_CONST` and are not; nor is any
    /// other expression, whether the compiler would fold it or not, which
    /// is why `chr(1 + 64)` is a call where `ord('a' . '')` is a
    /// constant.
    fn written(&mut self, expr: &Expr) -> Result<Option<Zval>, CompileError> {
        match expr {
            Expr::Int(n) => Ok(Some(Zval::long(*n))),
            Expr::Float(f) => Ok(Some(Zval::double(*f))),
            Expr::Str(bytes) => Ok(Some(self.string(bytes)?)),
            Expr::Binary {
                op: BinaryOp::Concat,
                left,
                right,
            } => {
                let (Some(a), Some(b)) = (self.written(left)?, self.written(right)?) else {
                    return Ok(None);
                };
                self.concat_fold(a, b)
            }
            _ => Ok(None),
        }
    }
}
