//! What php compiles beside the program under `ZEND_COMPILE_EXTENDED_INFO`,
//! which a host's `-e` sets (#1780, [`crate::Options::extended_info`]):
//! the oplines a profiler or a debugger is told at, each emitted where
//! php's compiler emits it and nowhere while the engine compiles without
//! it.
//!
//! - `EXT_STMT` before each statement `zend_compile_stmt` compiles but
//!   a block, a label and the class members php leaves unticked
//!   (`zend_is_unticked_stmt`), so a property group and an enum case
//!   leave one in the op array the class is declared in; before the
//!   condition of each `elseif`; after a `for`'s condition; at the
//!   pipe's operand, which it carries in `op1`; and before a function's
//!   implicit return (`zend_do_extended_stmt`,
//!   `Z/Zend/zend_compile.c:1925`).
//! - `EXT_FCALL_BEGIN` and `EXT_FCALL_END` around the call op of every
//!   call `zend_compile_call_common` makes, after its arguments, and
//!   around an `include` or an `eval`, its operand inside them
//!   (`zend_do_extended_fcall_begin`, `zend_do_extended_fcall_end`,
//!   `Z/Zend/zend_compile.c:1945-1972`). `call_user_func` and its array
//!   form, which `zend_compile_func_cuf` and `zend_compile_func_cufa`
//!   call themselves, and a call served by an opcode of its own, carry
//!   none.
//! - `pass_two`'s `zend_update_extended_stmts`
//!   (`Z/Zend/zend_opcode.c:657`), [`update_extended_stmts`].
//!
//! `EXT_NOP` is php's opcode too, and no compile of php's emits it, so
//! none does here.

use php_unit::{Opcode, OperandKind, Opline};

use super::{Emit, Where};

impl Emit<'_> {
    /// `zend_do_extended_stmt(NULL)`: an `EXT_STMT` at the line the
    /// emitter stands on.
    pub(super) fn ext_stmt(&mut self) {
        self.ext_stmt_over(None);
    }

    /// `zend_do_extended_stmt(result)`: an `EXT_STMT`, carrying `value`
    /// in `op1` where one is given, which the opline reads and does not
    /// free (`zend_op_is_ext_stmt` in `Z/Zend/zend_opcode.c:911` keeps
    /// the live range open past it); a constant is a slot of its own, as
    /// `SET_NODE` adds one.
    pub(super) fn ext_stmt_over(&mut self, value: Option<Where>) {
        if !self.extended_info {
            return;
        }
        let (op1_type, op1) = match value {
            Some(value) => self.operand(value),
            None => (OperandKind::Unused, 0),
        };
        self.push(Opline {
            opcode: Opcode::ExtStmt,
            op1_type,
            op1,
            ..Opline::default()
        });
    }

    /// `zend_do_extended_fcall_begin` or `zend_do_extended_fcall_end`.
    pub(super) fn ext_fcall(&mut self, opcode: Opcode) {
        if !self.extended_info {
            return;
        }
        self.push(Opline {
            opcode,
            ..Opline::default()
        });
    }

    /// The call op of a call `zend_compile_call_common` makes
    /// ([`Self::call_op`]), with its `EXT_FCALL_BEGIN` before and its
    /// `EXT_FCALL_END` after. The end stands at the line the emitter
    /// stood on before the call op, as php's `opline->lineno = lineno`
    /// writes the call op's line alone and leaves `CG(zend_lineno)`.
    pub(super) fn extended_call(&mut self, opcode: Opcode, line: u32, extra_named: bool) -> Where {
        self.ext_fcall(Opcode::ExtFcallBegin);
        let before = self.line;
        let served = self.call_op(opcode, line, extra_named);
        let after = std::mem::replace(&mut self.line, before);
        self.ext_fcall(Opcode::ExtFcallEnd);
        self.line = after;
        served
    }
}

/// `zend_update_extended_stmts`, the first thing `pass_two` does where
/// the compile carries extended info: of two `EXT_STMT`s in a row the
/// first becomes a `NOP`, one standing last becomes a `NOP`, and every
/// other takes the line of the opline after it, which is where a
/// statement handler is told the statement stands.
pub(super) fn update_extended_stmts(oplines: &mut [Opline]) {
    for at in 0..oplines.len() {
        let next = oplines.get(at + 1).map(|next| (next.opcode, next.lineno));
        let Some(op) = oplines
            .get_mut(at)
            .filter(|op| op.opcode == Opcode::ExtStmt)
        else {
            continue;
        };
        match next {
            Some((Opcode::ExtStmt, _)) | None => op.opcode = Opcode::Nop,
            Some((_, lineno)) => op.lineno = lineno,
        }
    }
}

#[cfg(test)]
mod tests {
    use php_unit::{Opcode, OperandKind, Opline};

    use crate::Options;

    /// The source's main op array and its function's, compiled with
    /// extended info or without it.
    fn compiled(source: &[u8], extended_info: bool) -> (Vec<Opline>, Vec<Opline>) {
        let mut strings = php_heap::Strings::new();
        let options = Options {
            extended_info,
            ..Options::default()
        };
        let unit = crate::compile_source(
            &mut php_syntax::Arena::new(),
            source,
            &options,
            &mut strings,
        )
        .unwrap_or_default();
        let function = unit
            .functions
            .first()
            .map(|(_, body)| body.oplines.clone())
            .unwrap_or_default();
        (unit.main.oplines.clone(), function)
    }

    fn opcodes(oplines: &[Opline]) -> Vec<Opcode> {
        oplines.iter().map(|op| op.opcode).collect()
    }

    const SOURCE: &[u8] = b"<?php\nfunction f($a) { return $a; }\n\
        if ($x) { f(1); } elseif ($y) { echo 2; }\nfor (;$i;) {}\ninclude \"x\";\n\
        1 |> f(...);\necho 1, 2;\nclass C { public $p; }\n";

    /// Where php puts each extended opline, opcode for opcode as the
    /// oracle dumps `SOURCE` under `-e` at 8.5.9: before each statement,
    /// each `echo` element and each `elseif` condition, after the `for`'s
    /// condition, around each call and the `include`, at the pipe's
    /// operand, and for the property group, whose `EXT_STMT` and the
    /// `include` statement's own become `NOP`s where another follows.
    #[test]
    fn extended_info_is_php_s_oplines_where_php_emits_them() {
        use Opcode::{
            DoUcall, Echo, ExtFcallBegin, ExtFcallEnd, ExtStmt, IncludeOrEval, InitFcall, Jmp,
            Jmpnz, Jmpz, Nop, Recv, Return, SendVal,
        };
        let (main, function) = compiled(SOURCE, true);
        // One line of the source a row, as the oracle's dump stands.
        #[rustfmt::skip]
        let expected = [
            ExtStmt, Jmpz, ExtStmt, InitFcall, SendVal, ExtFcallBegin, DoUcall, ExtFcallEnd, Jmp,
            ExtStmt, Jmpz, ExtStmt, Echo,
            ExtStmt, Jmp, ExtStmt, Jmpnz,
            ExtStmt, ExtFcallBegin, IncludeOrEval, ExtFcallEnd,
            Nop, ExtStmt, InitFcall, SendVal, ExtFcallBegin, DoUcall, ExtFcallEnd,
            ExtStmt, Echo, ExtStmt, Echo,
            ExtStmt, Return,
        ];
        assert_eq!(opcodes(&main), expected);
        assert_eq!(opcodes(&function), [Recv, ExtStmt, Return, ExtStmt, Return]);
        // The pipe's `EXT_STMT` carries the operand the left side left.
        assert_eq!(main[22].op1_type, OperandKind::Const);
        // Each `EXT_STMT` stands at the line of the opline after it.
        for pair in main.windows(2) {
            if pair[0].opcode == ExtStmt {
                assert_eq!(pair[0].lineno, pair[1].lineno, "{pair:?}");
            }
        }
    }

    /// A property group is a statement of `zend_compile_stmt`'s in the op
    /// array the class is declared in: its `EXT_STMT` and its `TICKS`
    /// stand before the declaration, each `TICKS` kept where an `EXT_STMT`
    /// parts it from the one before, and swallowed by it where none does,
    /// as the oracle dumps the program under `declare(ticks=1)` with and
    /// without `-e`.
    #[test]
    fn a_class_member_is_ticked_where_php_ticks_it() {
        use Opcode::{
            Assign, DeclareAnonClass, DoFcall, ExtFcallBegin, ExtFcallEnd, ExtStmt, New, Nop,
            Return, Ticks,
        };
        let source =
            b"<?php\ndeclare(ticks=1);\n$o = new class { public $p; public $q; };\n".as_slice();
        #[rustfmt::skip]
        let extended = [
            ExtStmt, Ticks, Nop, ExtStmt, Ticks, ExtStmt, Ticks,
            DeclareAnonClass, New, ExtFcallBegin, DoFcall, ExtFcallEnd, Assign, Ticks, Return,
        ];
        assert_eq!(opcodes(&compiled(source, true).0), extended);
        let plain = [Ticks, DeclareAnonClass, New, DoFcall, Assign, Ticks, Return];
        assert_eq!(opcodes(&compiled(source, false).0), plain);
    }

    /// Without extended info nothing of it is emitted, and the plain
    /// compile is the one it was.
    #[test]
    fn without_extended_info_none_is_emitted() {
        let (main, function) = compiled(SOURCE, false);
        let extended = |op: &Opline| {
            matches!(
                op.opcode,
                Opcode::ExtStmt | Opcode::ExtFcallBegin | Opcode::ExtFcallEnd | Opcode::ExtNop
            )
        };
        assert!(!main.iter().chain(&function).any(extended));
        assert_eq!(main.len(), 16);
    }
}
