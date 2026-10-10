//! `zend_compile_func_cuf` and `zend_compile_func_cufa`
//! (`Z/Zend/zend_compile.c:4290-4366`), the two rows of
//! `zend_try_compile_special_func`'s table that are a call of their own:
//! `call_user_func(...)` and `call_user_func_array(...)` spelled by a
//! name the compiler resolves to the native push no frame for the native
//! and call the callback straight from the frame that wrote them.
//!
//! The callback is `zend_compile_init_user_func`'s: a string written as
//! such that names a function the table holds is that function's
//! `INIT_FCALL` (`zend_try_compile_ct_bound_init_user_func`), and
//! anything else is `INIT_USER_CALL` over the callback, the native's
//! lowered name in `op1` for the `TypeError` the engine raises over a
//! value that is no callback. `call_user_func`'s remaining arguments are
//! each one `SEND_USER` at its number; `call_user_func_array`'s array is
//! one `SEND_ARRAY`, whose string keys the engine places by name, so a
//! `CHECK_UNDEF_ARGS` follows it and the call op carries
//! `ZEND_FCALL_MAY_HAVE_EXTRA_NAMED_PARAMS`; an array written as
//! `array_slice($a, <offset>, $length)` is that `SEND_ARRAY` over `$a`
//! with the length in `op2` and the offset in `extended_value`, and no
//! name is placed. The call is `DO_FCALL` whatever the callee, at the
//! line the compiler last stood on, as `zend_emit_op` stamps it.
//!
//! ```php
//! function f($a, &$b = null) { return "f$a"; }
//! $args = ['x', 'y', 'z'];
//! var_dump(call_user_func('f', 1), call_user_func('F', 2), call_user_func('strtoupper', 'a'), call_user_func(fn ($x) => $x * 2, 4));
//! var_dump(call_user_func_array('f', [3]), call_user_func_array('f', ['a' => 4]), call_user_func_array('str_pad', ['abc', 'length' => 5, 'pad_string' => '-']));
//! var_dump(call_user_func_array('implode', array_slice([0, ', ', $args], 1, null)), call_user_func_array('max', array_slice([5, 9, 7], 1, 2)));
//! ```
//! ```expect
//! string(2) "f1"
//! string(2) "f2"
//! string(1) "A"
//! int(8)
//! string(2) "f3"
//! string(2) "f4"
//! string(5) "abc--"
//! string(7) "x, y, z"
//! int(9)
//! ```
//!
//! The callee is called from the frame that wrote the call, so a native
//! it names reads that file's `strict_types`, where the native spelled
//! by a variable calls it from its own frame, which is never strict:
//!
//! ```php
//! declare(strict_types=1);
//! try { var_dump(call_user_func('str_repeat', 'a', '3')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
//! $f = 'call_user_func';
//! var_dump($f('str_repeat', 'a', '3'));
//! try { var_dump(call_user_func_array('str_repeat', ['a', '3'])); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
//! ```
//! ```expect
//! str_repeat(): Argument #2 ($times) must be of type int, string given
//! string(3) "aaa"
//! str_repeat(): Argument #2 ($times) must be of type int, string given
//! ```

use php_syntax::tree;
use php_unit::{Opcode, OperandKind, Opline};

use super::{Emit, Source, Where, used_stack};
use crate::CompileError;

/// `ZEND_FCALL_MAY_HAVE_EXTRA_NAMED_PARAMS` on a call op: a name the
/// array holds may land in the callee's variadic.
const MAY_HAVE_EXTRA_NAMED: bool = true;

impl Emit<'_> {
    /// `zend_compile_func_cuf`: a callback and nothing else named, then
    /// each remaining argument compiled as an expression and sent by
    /// `SEND_USER` at its number, and the `DO_FCALL`. No argument at all
    /// is the ordinary call, which the native refuses.
    pub(crate) fn call_user_func(
        &mut self,
        arguments: &[Source],
    ) -> Result<Option<Where>, CompileError> {
        let Some((callback, rest)) = arguments.split_first() else {
            return Ok(None);
        };
        let count = u32::try_from(rest.len()).unwrap_or(u32::MAX);
        self.init_user_func(*callback, count, b"call_user_func")?;
        for (number, argument) in (1u32..).zip(rest) {
            let value = self.source(*argument)?;
            let (op1_type, op1) = self.operand(value);
            self.push(Opline {
                opcode: Opcode::SendUser,
                op1_type,
                op1,
                op2: number,
                out: number - 1,
                ..Opline::default()
            });
        }
        let line = self.line;
        Ok(Some(self.call_op(Opcode::DoFcall, line, false)))
    }

    /// `zend_compile_func_cufa`: exactly a callback and an array, the
    /// callback initialised with no argument counted, then the array
    /// sent whole by `SEND_ARRAY`, or the `array_slice` over it
    /// [`Self::sliced`] reads.
    pub(crate) fn call_user_func_array(
        &mut self,
        arguments: &[Source],
    ) -> Result<Option<Where>, CompileError> {
        let [callback, array] = arguments else {
            return Ok(None);
        };
        self.init_user_func(*callback, 0, b"call_user_func_array")?;
        if let Some((source, offset, length)) = self.sliced(*array)? {
            let source = self.expr(source)?;
            let length = self.expr(length)?;
            let (op1_type, op1) = self.operand(source);
            let (op2_type, op2) = self.operand(length);
            self.push(Opline {
                opcode: Opcode::SendArray,
                op1_type,
                op1,
                op2_type,
                op2,
                extended_value: offset,
                ..Opline::default()
            });
            let line = self.line;
            return Ok(Some(self.call_op(Opcode::DoFcall, line, false)));
        }
        let value = self.source(*array)?;
        let (op1_type, op1) = self.operand(value);
        self.push(Opline {
            opcode: Opcode::SendArray,
            op1_type,
            op1,
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::CheckUndefArgs,
            ..Opline::default()
        });
        let line = self.line;
        Ok(Some(self.call_op(
            Opcode::DoFcall,
            line,
            MAY_HAVE_EXTRA_NAMED,
        )))
    }

    /// `zend_compile_init_user_func`: the `INIT_FCALL` of the function a
    /// string written as such names where the table holds it
    /// (`zend_try_compile_ct_bound_init_user_func`), its name lowered
    /// and nothing resolved against a namespace, so `'\strlen'` is no
    /// such name; and otherwise the callback compiled and
    /// `INIT_USER_CALL` over it, with the native's lowered name in `op1`
    /// and `count` in `extended_value`.
    fn init_user_func(
        &mut self,
        callback: Source,
        count: u32,
        native: &[u8],
    ) -> Result<(), CompileError> {
        if let Some(name) = self.written_of(callback)?
            && let Some(name) = name.as_str().map(|s| s.bytes().to_ascii_lowercase())
        {
            let stack = if self.native(&name) {
                Some(used_stack(count, None))
            } else {
                self.declared_function(&name)
                    .map(|(_, body)| used_stack(count, Some(body)))
            };
            if let Some(stack) = stack {
                let lowered = self.string(&name)?;
                let op2 = self.literal(lowered);
                let out = self.cache_slot();
                self.push(Opline {
                    opcode: Opcode::InitFcall,
                    op1: stack,
                    op2_type: OperandKind::Const,
                    op2,
                    out,
                    extended_value: count,
                    ..Opline::default()
                });
                return Ok(());
            }
        }
        let value = self.source(callback)?;
        let (op2_type, op2) = self.operand(value);
        let native = self.string(native)?;
        let op1 = self.literal(native);
        self.push(Opline {
            opcode: Opcode::InitUserCall,
            op1_type: OperandKind::Const,
            op1,
            op2_type,
            op2,
            extended_value: count,
            ..Opline::default()
        });
        Ok(())
    }

    /// `zend_compile_func_cufa`'s one more arm: an array argument that
    /// is a call of a name resolving to `array_slice`, with three plain
    /// arguments whose second is an integer written as such from 0 to
    /// `0x7fffffff`: the array, the offset and the length. Nothing for
    /// any other array.
    fn sliced(
        &mut self,
        array: Source,
    ) -> Result<Option<(tree::Expr, u32, tree::Expr)>, CompileError> {
        let Some((resolved, args)) = array.tree().and_then(|array| self.named_call(array)) else {
            return Ok(None);
        };
        if resolved != b"array_slice" {
            return Ok(None);
        }
        let list: Option<Vec<tree::Expr>> = args
            .iter()
            .map(|arg| match arg {
                tree::Arg::Expr(expr) => Some(*expr),
                _ => None,
            })
            .collect();
        let Some([source, offset, length]) = list.as_deref() else {
            return Ok(None);
        };
        let offset = self
            .written(*offset)?
            .and_then(php_heap::Zval::as_long)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n <= 0x7fff_ffff);
        Ok(offset.map(|offset| (*source, offset, *length)))
    }
}
