//! `zend_compile_assert` (`Z/Zend/zend_compile.c:4369`): a call of
//! `assert()` by a name the compiler resolves to it, or by the bare name
//! in a namespace, which the program resolves, is compiled behind an
//! `ASSERT_CHECK` that jumps over the whole call while `zend.assertions`
//! is at or below zero, so the arguments are never evaluated; and an
//! assertion written with no description is handed its own source text
//! as php prints it ([`Emit::assertion_text`]), named `description:`
//! where the assertion was named. Under `zend.assertions = -1` nothing
//! of the call is compiled and it is the constant `true`.
//!
//! ```php
//! function f() { echo "called\n"; return false; }
//! try { assert(1 === 2); } catch (AssertionError $e) { echo $e->getMessage(), "\n"; }
//! try { assert(assertion: f() || false); } catch (AssertionError $e) { echo $e->getMessage(), "\n"; }
//! try { \assert(false, 'mine'); } catch (AssertionError $e) { echo $e->getMessage(), "\n"; }
//! ini_set('zend.assertions', 0);
//! var_dump(assert(f()));
//! ini_set('zend.assertions', 1);
//! var_dump(assert(true));
//! ```
//! ```expect
//! assert(1 === 2)
//! called
//! assert(assertion: f() || false)
//! mine
//! bool(true)
//! bool(true)
//! ```
//!
//! A namespaced `assert` is resolved as the program serves, and is the
//! same check over the call by name:
//!
//! ```php
//! namespace Foo;
//! try { assert(strlen('ab') > 2); } catch (\AssertionError $e) { echo $e->getMessage(), "\n"; }
//! try { \assert(false); } catch (\AssertionError $e) { echo $e->getMessage(), "\n"; }
//! ```
//! ```expect
//! assert(strlen('ab') > 2)
//! assert(false)
//! ```
//!
//! Which is why no namespace may declare a function of the name
//! (`zend_begin_func_decl`), wherever the declaration stands:
//!
//! ```php
//! namespace Foo;
//! if (true) { function assert($a) {} }
//! ```
//! ```expect exit=255
//! Fatal error: Defining a custom assert() function is not allowed, as the function has special semantics in %s on line 3
//! ```

use php_heap::Zval;
use php_unit::{Opcode, OperandKind, Opline};

use super::args::{Arg, Args, Callee, Plan};
use super::{Emit, Fbc, Source, Where, used_stack};
use crate::CompileError;

/// What `zend_compile_assert` makes of a call of `assert()`, by
/// `EG(assertions)` while the file compiles
/// ([`crate::Options::assertions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Assertions {
    /// At or above zero: the call behind an `ASSERT_CHECK`.
    Checked,
    /// `-1`: the constant `true`, and nothing of the call compiled.
    Removed,
}

impl Assertions {
    pub(super) fn of(compiled: bool) -> Assertions {
        if compiled {
            Assertions::Checked
        } else {
            Assertions::Removed
        }
    }
}

impl Emit<'_> {
    /// `zend_compile_assert`: `resolved` is the name the call resolves
    /// to, and `native` whether the function table holds `assert`, php's
    /// `fbc`, which is `INIT_FCALL` over it; otherwise the name is the
    /// program's to resolve, `INIT_NS_FCALL_BY_NAME`.
    pub(super) fn assert(
        &mut self,
        resolved: &[u8],
        arguments: &Args,
        native: Option<usize>,
    ) -> Result<Where, CompileError> {
        if self.assertions == Assertions::Removed {
            return Ok(Where::Constant(Zval::bool(true)));
        }
        let line = self.line;
        let check = self.push(Opline {
            opcode: Opcode::AssertCheck,
            ..Opline::default()
        });
        let written = arguments.list();
        let mut list = self.arena.vec(written.len() + 1);
        list.extend_from_slice(written);
        if let [only] = written {
            let text = self.assertion_text(only)?;
            let description = self.string(&text)?;
            // "If the original argument was named, add the new argument
            // as named as well, as mixing named and positional is not
            // allowed."
            let description = Source::Node(Where::Constant(description));
            list.push(match only {
                Arg::Named(..) => Arg::Named(b"description", description),
                Arg::Positional(_) | Arg::Unpack(_) => Arg::Positional(description),
            });
        }
        let arguments = match arguments {
            Args::Written { line, .. } => Args::Written {
                list: list.into_slice(),
                line: *line,
            },
            Args::CallableConvert => Args::CallableConvert,
        };
        let served = if let Some(index) = native {
            let parameters = self
                .functions
                .rows()
                .get(index)
                .map(|decl| &decl.parameters[..])
                .unwrap_or_default();
            let callee = Callee::of(parameters);
            let plan = Plan::of(self.arena, &arguments, Some(&callee));
            let name = self.string(b"assert")?;
            let op2 = self.literal(name);
            let out = self.cache_slot();
            self.push(Opline {
                opcode: Opcode::InitFcall,
                op1: used_stack(plan.count, None),
                op2_type: OperandKind::Const,
                op2,
                out,
                extended_value: plan.count,
                ..Opline::default()
            });
            self.send_args(&plan, &arguments, Some(&callee))?;
            self.extended_call(self.get_call_op(Fbc::Native, false), line, plan.extra_named)
        } else {
            let pair = self.ns_func_name_literal(resolved)?;
            let plan = Plan::of(self.arena, &arguments, None);
            let out = self.cache_slot();
            self.push(Opline {
                opcode: Opcode::InitNsFcallByName,
                op2_type: OperandKind::Const,
                op2: pair,
                out,
                extended_value: plan.count,
                ..Opline::default()
            });
            self.send_args(&plan, &arguments, None)?;
            self.extended_call(self.get_call_op(Fbc::ByName, false), line, plan.extra_named)
        };
        let after = self.next();
        if let (Where::Returned(var), Some(opline)) = (served, self.oplines.get_mut(check as usize))
        {
            opline.op2 = after;
            opline.out_type = OperandKind::Var;
            opline.out = var;
        }
        Ok(served)
    }
}
