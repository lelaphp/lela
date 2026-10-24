//! The string oplines compiled inline, as php's code generator compiles
//! them: a strict comparison where a string is one side
//! (`zend_jit_identical`, `Z/ext/opcache/jit/zend_jit_ir.c:7292-7532`), a
//! string's length (`zend_jit_strlen`, `:11623-11660`), a boolean cast
//! (`zend_jit_bool_jmpznz`, `:7533-7800`) and the free of a `foreach`'s
//! array (`zend_jit_free`, `:11543-11580`). A string is read through
//! [`Str::LEN_AT`] alone; its bytes are compared by
//! [`Kernel::Identical`], where php calls `zend_is_identical`.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I32, I64};
use cranelift_codegen::ir::{BlockArg, InstBuilder, Value};
use php_engine::tier::FRAME;
use php_heap::{Str, ZvalType};
use php_unit::{Opcode, OperandKind};

use super::{COUNTED, Emit, Kernel, Scalar, TYPE_AT, imm, mismatch, slot_offset, tag, trusted};
use crate::types::{
    Abort, Check, Constant, Frame, Fused, GuardAt, Home, Leave, Line, OpId, Operand, Slot,
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

    /// The pointer word of the string `operand` names, of `frame`: its
    /// slot's word, or the word of its zval among the op array's
    /// constants.
    fn string_word(&mut self, frame: Frame, operand: Operand) -> Result<Value, Abort> {
        match operand {
            Operand::Value(value) => {
                let held = self.value(value)?;
                if held.home == Home::Register {
                    return Err(mismatch("a string held in a register"));
                }
                let (room, at) = self.place(held.slot)?;
                Ok(self.b.ins().load(I64, trusted(), room, at))
            }
            Operand::Constant(Constant::Other(idx)) => {
                let zval = self.constant_zval(frame, idx)?;
                Ok(self.b.ins().load(I64, trusted(), zval, 0))
            }
            _ => Err(mismatch("a string read from a scalar")),
        }
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
            php_unit::Opcode::IsNotIdentical => self.b.ins().icmp_imm_s(IntCC::Equal, same, 0),
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

    /// A cast of a scalar to a boolean, its truth, or its negation for
    /// `!`, as php's `zend_jit_bool_jmpznz` writes it.
    pub(super) fn bool(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let held = self.scalar(at, line.op1)?;
        let truth = self.truth(held);
        let truth = match line.opcode {
            php_unit::Opcode::BoolNot => self.b.ins().icmp_imm_s(IntCC::Equal, truth, 0),
            _ => truth,
        };
        if let Some(out) = line.out {
            self.define(out, Scalar::Bool(truth))?;
        }
        Ok(())
    }

    /// `FREE_OP` of `operand` where it is a temporary, as the handler
    /// takes it: what it holds out of its slot, which is left `UNDEF`,
    /// then its owner given up through [`Kernel::Release`] where it is
    /// counted, what a destructor throws leaving through the opline's
    /// unwind. A `foreach`'s array freed at its loop's end is this,
    /// php's `zend_jit_free` of `FE_FREE` over an array, which holds no
    /// iterator to delete.
    pub(super) fn free_temp(&mut self, line: &Line, operand: Operand) -> Result<(), Abort> {
        let Operand::Value(value) = operand else {
            return Ok(());
        };
        let held = self.value(value)?;
        if held.home == Home::Register || held.slot.var < self.frame_cvs(held.slot.frame)? {
            return Ok(());
        }
        self.free_slot(line, held.slot)
    }

    /// The temporary `slot` freed as [`Emit::free_temp`] frees it.
    pub(super) fn free_slot(&mut self, line: &Line, slot: Slot) -> Result<(), Abort> {
        let room = self.room(slot.frame)?;
        let at = slot_offset(slot.var)?;
        let info = self.b.ins().load(I32, trusted(), room, at + TYPE_AT);
        let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
        self.b.ins().store(trusted(), undef, room, at + TYPE_AT);
        let counted = self.b.ins().band_imm_u(info, COUNTED);
        let [room_arg, opline] = self.served(line)?;
        self.when(counted, |emit| {
            let word = emit.b.ins().load(I64, trusted(), room, at);
            emit.call(
                Kernel::Release,
                &[room_arg, opline, word, info],
                line.unwind.map(Leave::Unwind),
            )
        })
    }

    /// The string `operand` of `frame` names, its two words, with an owner
    /// to hand on, as the handler's string arm of a rope takes a piece: a
    /// temporary's moved, its slot left `UNDEF`; a variable's or a
    /// constant's with one owner more, `zend_string_copy`.
    fn owned_string(&mut self, frame: Frame, operand: Operand) -> Result<(Value, Value), Abort> {
        let (base, at, temporary) = match operand {
            Operand::Value(value) => {
                let held = self.value(value)?;
                if held.home == Home::Register {
                    return Err(mismatch("a string held in a register"));
                }
                let (room, at) = self.place(held.slot)?;
                let temporary = held.slot.var >= self.frame_cvs(held.slot.frame)?;
                (room, at, temporary)
            }
            Operand::Constant(Constant::Other(idx)) => (self.constant_zval(frame, idx)?, 0, false),
            _ => return Err(mismatch("a string read from a scalar")),
        };
        let word = self.b.ins().load(I64, trusted(), base, at);
        let info = self.b.ins().load(I32, trusted(), base, at + TYPE_AT);
        if temporary {
            let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
            self.b.ins().store(trusted(), undef, base, at + TYPE_AT);
        } else {
            self.addref(word, info)?;
        }
        Ok((word, info))
    }

    /// `EX(opline) = opline`, php's `SAVE_OPLINE()`, written in place
    /// before a kernel that may raise reads where the frame stands.
    fn save_opline(&mut self, line: &Line) -> Result<(), Abort> {
        let room = self.room(line.frame)?;
        let opline = self.b.ins().iconst(I32, i64::from(line.opline.0));
        let at =
            i32::try_from(FRAME.opline()).map_err(|_| mismatch("an opline past an immediate"))?;
        self.b.ins().store(trusted(), opline, room, at);
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
        let var = self.value(target)?.slot.var;
        let (word, info) = match line.op2 {
            Operand::Value(value) => {
                let held = self.value(value)?;
                let (room, at) = self.place(held.slot)?;
                let word = self.b.ins().load(I64, trusted(), room, at);
                let info = self.b.ins().load(I32, trusted(), room, at + TYPE_AT);
                (word, info)
            }
            Operand::Constant(Constant::Other(idx)) => {
                let zval = self.constant_zval(line.frame, idx)?;
                let word = self.b.ins().load(I64, trusted(), zval, 0);
                let info = self.b.ins().load(I32, trusted(), zval, TYPE_AT);
                (word, info)
            }
            _ => return Err(mismatch("a .= of a scalar")),
        };
        let [room, opline] = self.served(line)?;
        let var = self.b.ins().iconst(I32, i64::from(var));
        self.call_freeing(Kernel::AssignConcat, &[room, opline, var, word, info], line)
    }

    /// `kernel` called with `args`, then `line`'s `op2` given up where a
    /// temporary holds it, `FREE_OP2`, before the kernel's status is
    /// tested, so a throw leaves no temporary behind, as the handler
    /// frees the operand before it raises and php's code before
    /// `zend_jit_check_exception`; then the status tested, leaving through
    /// `line`'s unwind.
    fn call_freeing(&mut self, kernel: Kernel, args: &[Value], line: &Line) -> Result<(), Abort> {
        let status = self.call_status(kernel, args)?;
        self.free_temp(line, line.op2)?;
        let to = self.left_by(line.unwind.map(Leave::Unwind))?;
        let on = self.b.create_block();
        self.b.ins().brif(status, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// The two words of the zval `operand` of `frame` names, as it stands:
    /// its slot's, or its constant's.
    fn zval_words(&mut self, frame: Frame, operand: Operand) -> Result<(Value, Value), Abort> {
        let (base, at) = match operand {
            Operand::Value(value) => {
                let held = self.value(value)?;
                if held.home == Home::Register {
                    return Err(mismatch("a string held in a register"));
                }
                self.place(held.slot)?
            }
            Operand::Constant(Constant::Other(idx)) => (self.constant_zval(frame, idx)?, 0),
            _ => return Err(mismatch("a string read from a scalar")),
        };
        let word = self.b.ins().load(I64, trusted(), base, at);
        let info = self.b.ins().load(I32, trusted(), base, at + TYPE_AT);
        Ok((word, info))
    }

    /// The `IS_*` of the slot or constant `operand` names, as the handler
    /// reads its kind.
    fn kind_of(&self, operand: Operand) -> Result<OperandKind, Abort> {
        Ok(match operand {
            Operand::Value(value) => {
                let slot = self.value(value)?.slot;
                if slot.var < self.frame_cvs(slot.frame)? {
                    OperandKind::Cv
                } else {
                    OperandKind::Tmp
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
            line,
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
