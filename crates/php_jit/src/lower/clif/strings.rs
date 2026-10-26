//! The string oplines compiled inline, as php's code generator compiles
//! them: a strict comparison where a string is one side
//! (`zend_jit_identical`, `Z/ext/opcache/jit/zend_jit_ir.c:7292-7532`), a
//! string's length (`zend_jit_strlen`, `:11623-11660`), a concatenation of
//! two strings (`zend_jit_concat`, `:6016-6118`), a rope's string pieces
//! (`zend_jit_rope`, `:11753-11799`) and `.=` of two strings
//! (`zend_jit_assign_op`, `:6120-6290`). A string is read through
//! [`Str::LEN_AT`] alone; its bytes are compared by
//! [`Kernel::Identical`], where php calls `zend_is_identical`, and joined
//! by the kernels over the engine's own rules, where php calls its helpers
//! (`Kernel::Concat`, `RopeInit`, `RopeAdd`, `RopeEnd`, `AssignConcat`).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I32, I64};
use cranelift_codegen::ir::{BlockArg, InstBuilder, Value};
use php_heap::{Str, ZvalType};
use php_unit::{Opcode, OperandKind};

use super::{EX_OPLINE, Emit, Kernel, Scalar, TYPE_AT, imm, mismatch, tag, trusted};
use crate::types::{
    Abort, Check, Constant, Frame, Fused, GuardAt, Home, Leave, Line, OpId, Operand,
};

/// Where a string's length is, `ZSTR_LEN`.
const LEN_AT: i32 = imm(Str::LEN_AT);

impl Emit<'_> {
    /// Whether `operand` of the inline op at `at` is a string: a constant
    /// no scalar is, which an inline comparison reads only as a string
    /// (`types` serves no other inline), or a value its guard before the
    /// op, or its types, hold as one.
    pub(super) fn is_string(&self, at: OpId, operand: Operand) -> bool {
        let tag = match operand {
            Operand::Constant(Constant::Other(_)) => return true,
            Operand::Constant(_) | Operand::Unused => return false,
            Operand::Value(value) => self
                .ir
                .guards
                .iter()
                .find_map(|g| match g.check {
                    Check::Type { value: v, tag } if v == value && g.at == GuardAt::Before(at) => {
                        Some(tag)
                    }
                    _ => None,
                })
                .or_else(|| self.value(value).ok().and_then(|held| held.types.tag())),
        };
        tag == Some(ZvalType::String)
    }

    /// A strict comparison where one side is a string, as php's
    /// `zend_jit_identical` compiles it: a string and a scalar are never
    /// identical, so the answer is a constant; two strings are identical
    /// where they are one string, and otherwise where their lengths and
    /// then their bytes are equal, the bytes compared by
    /// [`Kernel::Identical`], php's call of `zend_is_identical`. A
    /// temporary either side is is given up after, `FREE_OP1` and
    /// `FREE_OP2`, before the jump fused to it is held. Lines matched
    /// against a constant, an empty line among them, and a part of each
    /// held to a prefix, `!==` beside `===`, where the answer is written
    /// and where it is jumped on:
    ///
    /// ```php
    /// function f($lines) {
    ///     $n = 0; $p = 0; $e = 0; $q = 0;
    ///     foreach ($lines as $line) {
    ///         if ($line === '') { $e++; continue; }
    ///         $same = substr($line, 0, 3) === 'ab/';
    ///         if ($same) { $p++; }
    ///         if (substr($line, 0, 2) !== 'ab') { $q++; }
    ///         $n += ($line === 1) + ($line !== 'ab/cd');
    ///     }
    ///     return [$n, $p, $e, $q];
    /// }
    /// echo json_encode(f(['ab/cd', '', 'ab/x', 'xy', str_repeat('ab/', 3), '', 'a'])), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [4,3,2,2]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($lines) {
    ///     $n = 0; $p = 0; $e = 0; $q = 0;
    ///     foreach ($lines as $line) {
    ///         if ($line === '') { $e++; continue; }
    ///         $same = substr($line, 0, 3) === 'ab/';
    ///         if ($same) { $p++; }
    ///         if (substr($line, 0, 2) !== 'ab') { $q++; }
    ///         $n += ($line === 1) + ($line !== 'ab/cd');
    ///     }
    ///     return [$n, $p, $e, $q];
    /// }
    /// echo json_encode(f(['ab/cd', '', 'ab/x', 'xy', str_repeat('ab/', 3), '', 'a'])), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [4,3,2,2]
    /// ```
    pub(super) fn identical_strings(&mut self, at: OpId, line: &Line) -> Result<Value, Abort> {
        let same = if self.is_string(at, line.op1) && self.is_string(at, line.op2) {
            let a = self.string_word(line.frame, line.op1)?;
            let b = self.string_word(line.frame, line.op2)?;
            self.same_string(line, a, b)?
        } else {
            self.b.ins().iconst(I8, 0)
        };
        for operand in [line.op1, line.op2] {
            self.free_temp(line, operand)?;
        }
        Ok(match line.opcode {
            Opcode::IsNotIdentical => self.b.ins().icmp_imm_s(IntCC::Equal, same, 0),
            _ => same,
        })
    }

    /// Whether the strings at `a` and `b` are identical, php's
    /// `zend_string_equal_content`: one string, or of one length and the
    /// same bytes, which [`Kernel::Identical`] compares.
    fn same_string(&mut self, line: &Line, a: Value, b: Value) -> Result<Value, Abort> {
        let done = self.b.create_block();
        self.b.append_block_param(done, I8);
        let one = self.b.ins().iconst(I8, 1);
        let zero = self.b.ins().iconst(I8, 0);
        let lengths = self.b.create_block();
        let bytes = self.b.create_block();
        let pointer = self.b.ins().icmp(IntCC::Equal, a, b);
        self.b
            .ins()
            .brif(pointer, done, &[BlockArg::Value(one)], lengths, &[]);
        self.b.switch_to_block(lengths);
        self.b.seal_block(lengths);
        let la = self.b.ins().load(I64, trusted(), a, LEN_AT);
        let lb = self.b.ins().load(I64, trusted(), b, LEN_AT);
        let equal = self.b.ins().icmp(IntCC::Equal, la, lb);
        self.b
            .ins()
            .brif(equal, bytes, &[], done, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(bytes);
        self.b.seal_block(bytes);
        let [room, opline] = self.served(line)?;
        let slot = self.answer();
        let answer = self.b.ins().stack_addr(I64, slot, 0);
        let string = self.b.ins().iconst(I32, tag(ZvalType::String));
        self.call(
            Kernel::Identical,
            &[room, opline, a, string, b, string, answer],
            None,
        )?;
        let word = self.b.ins().stack_load(I64, I64, slot, 0);
        let same = self.b.ins().icmp_imm_u(IntCC::NotEqual, word, 0);
        self.b.ins().jump(done, &[BlockArg::Value(same)]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(self.b.block_params(done)[0])
    }

    /// A strict comparison where a string is one side, its answer
    /// written, or the jump fused to it held.
    pub(super) fn compare_strings(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let result = self.identical_strings(at, line)?;
        let jumps = match line.fused {
            Some(Fused::Jmpz) => self.b.ins().icmp_imm_s(IntCC::Equal, result, 0),
            Some(Fused::Jmpnz) => result,
            None => {
                if let Some(out) = line.out {
                    self.define(out, Scalar::Bool(result))?;
                }
                result
            }
        };
        self.direction(at, jumps)
    }

    /// A string's length, php's `zend_jit_strlen`: `ZSTR_LEN` of the
    /// string read in place, into what the opline writes, the string
    /// given up after where a temporary held it. A line's length and a
    /// joined string's, a temporary, each summed:
    ///
    /// ```php
    /// function f($lines) { $t = 0; foreach ($lines as $line) { $t += strlen($line) + strlen($line . "|") + strlen(""); } return $t; }
    /// echo f(['ab', '', str_repeat('x', 40), 'abc']), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 94
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($lines) { $t = 0; foreach ($lines as $line) { $t += strlen($line) + strlen($line . "|") + strlen(""); } return $t; }
    /// echo f(['ab', '', str_repeat('x', 40), 'abc']), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 94
    /// ```
    pub(super) fn strlen(&mut self, line: &Line) -> Result<(), Abort> {
        let word = self.string_word(line.frame, line.op1)?;
        let len = self.b.ins().load(I64, trusted(), word, LEN_AT);
        self.free_temp(line, line.op1)?;
        if let Some(out) = line.out {
            self.define(out, Scalar::Long(len))?;
        }
        Ok(())
    }

    /// The string `operand` of `frame` names, its two words, with an owner
    /// to hand on, as the handler's string arm of a rope takes a piece: a
    /// temporary's moved, its slot left `UNDEF`; a variable's or a
    /// constant's with one owner more, `zend_string_copy`.
    fn owned_string(&mut self, frame: Frame, operand: Operand) -> Result<(Value, Value), Abort> {
        let (base, at) = self.zval_place(frame, operand)?;
        let word = self.b.ins().load(I64, trusted(), base, at);
        let info = self.b.ins().load(I32, trusted(), base, at + TYPE_AT);
        if self.kind_of(operand)? == OperandKind::Tmp {
            let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
            self.b.ins().store(trusted(), undef, base, at + TYPE_AT);
        } else {
            self.addref(word, info)?;
        }
        Ok((word, info))
    }

    /// Where the zval `operand` of `frame` names is: its slot, or its
    /// constant among the op array's.
    fn zval_place(&mut self, frame: Frame, operand: Operand) -> Result<(Value, i32), Abort> {
        match operand {
            Operand::Value(value) => {
                let held = self.value(value)?;
                if held.home == Home::Register {
                    return Err(mismatch("a string held in a register"));
                }
                self.place(held.slot)
            }
            Operand::Constant(Constant::Other(idx)) => Ok((self.constant_zval(frame, idx)?, 0)),
            _ => Err(mismatch("a string read from a scalar")),
        }
    }

    /// The two words of the zval `operand` of `frame` names, as it stands.
    fn zval_words(&mut self, frame: Frame, operand: Operand) -> Result<(Value, Value), Abort> {
        let (base, at) = self.zval_place(frame, operand)?;
        let word = self.b.ins().load(I64, trusted(), base, at);
        let info = self.b.ins().load(I32, trusted(), base, at + TYPE_AT);
        Ok((word, info))
    }

    /// The pointer word of the string `operand` of `frame` names.
    fn string_word(&mut self, frame: Frame, operand: Operand) -> Result<Value, Abort> {
        let (base, at) = self.zval_place(frame, operand)?;
        Ok(self.b.ins().load(I64, trusted(), base, at))
    }

    /// `EX(opline) = opline`, php's `SAVE_OPLINE()`, written in place
    /// before a kernel that may raise reads where the frame stands.
    fn save_opline(&mut self, line: &Line) -> Result<(), Abort> {
        let room = self.room(line.frame)?;
        let opline = self.b.ins().iconst(I32, i64::from(line.opline.0));
        self.b.ins().store(trusted(), opline, room, EX_OPLINE);
        Ok(())
    }

    /// A rope's piece that is a string, as php's `zend_jit_rope` takes
    /// one, with its owner, onto the request's rope through
    /// [`Kernel::RopeInit`], [`Kernel::RopeAdd`] or, joining it,
    /// [`Kernel::RopeEnd`], php's `zend_jit_rope_end`: the rope's
    /// position in its temporary as `ROPE_INIT` writes it, and the joined
    /// string into what `ROPE_END` writes, its rope's temporary left
    /// `UNDEF` as the handler takes it. A piece of another kind is the
    /// handler's, as php's is. Lines of variables, a long among them,
    /// joined and appended:
    ///
    /// ```php
    /// function f($n) {
    ///     $all = ""; $t = "t";
    ///     for ($i = 0; $i < $n; $i++) { $p = "/p/" . ($i % 3); $all .= "<$i $p $t>"; $all .= "{$t}{$p}|"; }
    ///     return [strlen($all), crc32($all), "$t$t$t"];
    /// }
    /// echo json_encode(f(30)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [500,464975510,"ttt"]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($n) {
    ///     $all = ""; $t = "t";
    ///     for ($i = 0; $i < $n; $i++) { $p = "/p/" . ($i % 3); $all .= "<$i $p $t>"; $all .= "{$t}{$p}|"; }
    ///     return [strlen($all), crc32($all), "$t$t$t"];
    /// }
    /// echo json_encode(f(30)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [500,464975510,"ttt"]
    /// ```
    ///
    /// A piece that is a string on one turn and an object with a
    /// `__toString` on the next is held to its type before the rope takes
    /// it, so the object's turn is the handler's, its `__toString` served
    /// there, under force and where every guard fails once; so is a `.=`
    /// of it:
    ///
    /// ```php
    /// class S { function __toString(): string { return "s"; } }
    /// function f($n) { $o = new S; $t = ""; for ($i = 0; $i < $n; $i++) { $p = $i % 3 ? "p" : $o; $t .= "<$p|$i>"; $t .= $p; } return $t; }
    /// echo f(7), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// <s|0>s<p|1>p<p|2>p<s|3>s<p|4>p<p|5>p<s|6>s
    /// ```
    ///
    /// ```php
    /// class S { function __toString(): string { return "s"; } }
    /// function f($n) { $o = new S; $t = ""; for ($i = 0; $i < $n; $i++) { $p = $i % 3 ? "p" : $o; $t .= "<$p|$i>"; $t .= $p; } return $t; }
    /// echo f(7), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// <s|0>s<p|1>p<p|2>p<s|3>s<p|4>p<p|5>p<s|6>s
    /// ```
    pub(super) fn rope(&mut self, line: &Line) -> Result<(), Abort> {
        let (word, info) = self.owned_string(line.frame, line.op2)?;
        let [room, opline] = self.served(line)?;
        let rope_slot = match line.op1 {
            Operand::Value(rope) if line.opcode != Opcode::RopeInit => Some(self.value(rope)?.slot),
            _ => None,
        };
        let rope = match rope_slot {
            Some(slot) => {
                let (base, at) = self.place(slot)?;
                Some(self.b.ins().load(I64, trusted(), base, at))
            }
            None => None,
        };
        let out = line.out.map(|out| self.value(out)).transpose()?;
        match (line.opcode, rope) {
            (Opcode::RopeInit, None) => {
                let slot = self.answer();
                let answer = self.b.ins().stack_addr(I64, slot, 0);
                self.call(Kernel::RopeInit, &[room, opline, word, info, answer], None)?;
                let position = self.b.ins().stack_load(I64, I64, slot, 0);
                let out = out.ok_or_else(|| mismatch("a ROPE_INIT that writes no rope"))?;
                let (base, at) = self.place(out.slot)?;
                self.b.ins().store(trusted(), position, base, at);
                let long = self.b.ins().iconst(I32, tag(ZvalType::Long));
                self.b.ins().store(trusted(), long, base, at + TYPE_AT);
            }
            (Opcode::RopeAdd, Some(rope)) => {
                self.call(Kernel::RopeAdd, &[room, opline, rope, word, info], None)?;
            }
            (Opcode::RopeEnd, Some(rope)) => {
                self.save_opline(line)?;
                let slot = self.answer();
                let answer = self.b.ins().stack_addr(I64, slot, 0);
                self.call(
                    Kernel::RopeEnd,
                    &[room, opline, rope, word, info, answer],
                    line.unwind.map(Leave::Unwind),
                )?;
                if let Some(rope_slot) = rope_slot {
                    let (base, at) = self.place(rope_slot)?;
                    let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
                    self.b.ins().store(trusted(), undef, base, at + TYPE_AT);
                }
                let out = out.ok_or_else(|| mismatch("a ROPE_END that writes no string"))?;
                let (base, at) = self.place(out.slot)?;
                let joined = self.b.ins().stack_load(I64, I64, slot, 0);
                let joined_info = self.b.ins().stack_load(I64, I32, slot, TYPE_AT);
                self.b.ins().store(trusted(), joined, base, at);
                self.b
                    .ins()
                    .store(trusted(), joined_info, base, at + TYPE_AT);
            }
            _ => return Err(mismatch("a rope's piece of no rope opline")),
        }
        Ok(())
    }

    /// `.=` of a string onto a variable's string, as php's
    /// `zend_jit_assign_op` calls `zend_jit_fast_assign_concat_helper`:
    /// [`Kernel::AssignConcat`], which grows the variable's string in
    /// place where the variable is its one owner, the frame stood at the
    /// opline, and the string appended given up after where a temporary
    /// held it, `FREE_OP2`. A line built by `.=` grows in place, as php's
    /// `zend_string_extend` grows it, and memory moves as php's:
    ///
    /// ```php
    /// function f($n) { $s = ""; $m = memory_get_usage(); for ($i = 0; $i < $n; $i++) { $s .= str_repeat("x", $i % 5) . "|"; } return [strlen($s), memory_get_usage() - $m]; }
    /// echo json_encode(f(400)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [1200,1496]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($n) { $s = ""; $m = memory_get_usage(); for ($i = 0; $i < $n; $i++) { $s .= str_repeat("x", $i % 5) . "|"; } return [strlen($s), memory_get_usage() - $m]; }
    /// echo json_encode(f(400)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [1200,1496]
    /// ```
    ///
    /// An append the memory limit refuses is raised at its line, the
    /// frame stood there:
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// function f() { $s = ""; for ($i = 0; $i < 100000; $i++) { $s .= str_repeat("x", 100) . "|"; } return $s; }
    /// echo strlen(f()), "\n";
    /// ```
    /// ```expect exit=255 regime=jit-force process
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate %d bytes) in %s on line 3
    /// ```
    pub(super) fn assign_concat(&mut self, line: &Line) -> Result<(), Abort> {
        let Operand::Value(target) = line.op1 else {
            return Err(mismatch("a .= onto no variable"));
        };
        let (base, at) = self.place(self.value(target)?.slot)?;
        let place = self.b.ins().iadd_imm_s(base, i64::from(at));
        self.concat_onto(line, place)
    }

    /// `.=` of `line`'s string onto the zval at `place`, which holds a
    /// string, through [`Kernel::AssignConcat`]: php's
    /// `zend_jit_fast_assign_concat_helper` over a variable
    /// (`zend_jit_assign_op`) or an array's element (`zend_jit_assign_dim_op`
    /// past its `zend_jit_concat_helper`, `Z/ext/opcache/jit/zend_jit_ir.c:13706-13712`),
    /// whose right side is `line`'s `op2` or its `OP_DATA`'s.
    pub(super) fn concat_onto(&mut self, line: &Line, place: Value) -> Result<(), Abort> {
        let right = if line.opcode == Opcode::AssignDimOp {
            line.data
        } else {
            line.op2
        };
        let (word, info) = self.zval_words(line.frame, right)?;
        let [room, opline] = self.served(line)?;
        self.call_freeing(
            Kernel::AssignConcat,
            &[room, opline, place, word, info],
            (line, right),
        )
    }

    /// `kernel` called with `args`, then `line`'s `freed`, its `op2` or
    /// its `OP_DATA`'s, given up where a temporary holds it, `FREE_OP2`
    /// and `FREE_OP_DATA`, before the kernel's status is
    /// tested, so a throw leaves no temporary behind, as the handler
    /// frees the operand before it raises and php's code before
    /// `zend_jit_check_exception`; then the status tested, leaving through
    /// `line`'s unwind.
    fn call_freeing(
        &mut self,
        kernel: Kernel,
        args: &[Value],
        (line, freed): (&Line, Operand),
    ) -> Result<(), Abort> {
        let status = self.call_status(kernel, args)?;
        self.free_temp(line, freed)?;
        self.left_unless(status, line.unwind.map(Leave::Unwind))
    }

    /// The `IS_*` of the slot or constant `operand` names, as the handler
    /// reads its kind.
    fn kind_of(&self, operand: Operand) -> Result<OperandKind, Abort> {
        Ok(match operand {
            Operand::Value(value) => {
                if self.is_temporary(self.value(value)?.slot)? {
                    OperandKind::Tmp
                } else {
                    OperandKind::Cv
                }
            }
            Operand::Constant(_) => OperandKind::Const,
            Operand::Unused => return Err(mismatch("a concatenation of an unused operand")),
        })
    }

    /// A concatenation of two strings, as php's `zend_jit_concat` calls
    /// `zend_jit_fast_concat_helper`, or `_tmp_helper` over a temporary
    /// left side: [`Kernel::Concat`], the handler's join of two strings,
    /// a temporary left side moved to it, its slot left `UNDEF`, so it
    /// grows where it stands as the handler grows it, the frame stood at
    /// the opline; the right side freed after where a temporary held it,
    /// `FREE_OP2`, and the joined string into what the opline writes. A
    /// path built of strings, an empty one among them, and a line of one
    /// joined to strings of another:
    ///
    /// ```php
    /// function f($parts, $n) {
    ///     $all = []; $e = "";
    ///     for ($i = 0; $i < $n; $i++) { $p = $parts[$i % 3]; $all[] = "/" . $p . "/x" . $e . $p; }
    ///     return [count(array_unique($all)), strlen(implode("", $all))];
    /// }
    /// echo json_encode(f(["a", "", str_repeat("b", 9)], 30)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [3,290]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($parts, $n) {
    ///     $all = []; $e = "";
    ///     for ($i = 0; $i < $n; $i++) { $p = $parts[$i % 3]; $all[] = "/" . $p . "/x" . $e . $p; }
    ///     return [count(array_unique($all)), strlen(implode("", $all))];
    /// }
    /// echo json_encode(f(["a", "", str_repeat("b", 9)], 30)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [3,290]
    /// ```
    pub(super) fn concat(&mut self, line: &Line) -> Result<(), Abort> {
        let (a_kind, b_kind) = (self.kind_of(line.op1)?, self.kind_of(line.op2)?);
        let (a, a_info) = self.zval_words(line.frame, line.op1)?;
        if a_kind == OperandKind::Tmp
            && let Operand::Value(value) = line.op1
        {
            // The temporary's owner is the join's now.
            let (room, at) = self.place(self.value(value)?.slot)?;
            let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
            self.b.ins().store(trusted(), undef, room, at + TYPE_AT);
        }
        let (b, b_info) = self.zval_words(line.frame, line.op2)?;
        self.save_opline(line)?;
        let [room, opline] = self.served(line)?;
        let kinds = u32::from(a_kind as u8) | u32::from(b_kind as u8) << 8;
        let kinds = self.b.ins().iconst(I32, i64::from(kinds));
        let slot = self.answer();
        let answer = self.b.ins().stack_addr(I64, slot, 0);
        self.call_freeing(
            Kernel::Concat,
            &[room, opline, a, a_info, b, b_info, kinds, answer],
            (line, line.op2),
        )?;
        let out = line
            .out
            .ok_or_else(|| mismatch("a concatenation that writes nothing"))?;
        let (base, at) = self.place(self.value(out)?.slot)?;
        let joined = self.b.ins().stack_load(I64, I64, slot, 0);
        let joined_info = self.b.ins().stack_load(I64, I32, slot, TYPE_AT);
        self.b.ins().store(trusted(), joined, base, at);
        self.b
            .ins()
            .store(trusted(), joined_info, base, at + TYPE_AT);
        Ok(())
    }
}
