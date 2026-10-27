//! `zend_analyze_calls` and `zend_build_call_map` (`zend_call_graph.c`)
//! as `zend_jit_trace_build_ssa` runs them, with `ZEND_CALL_TREE` and
//! `script` null: each call the op array makes whose callee
//! `zend_optimizer_get_called_func` finds, the opline that begins it, the
//! one that makes it and the ones that send its arguments, mapped by
//! opline.
//!
//! php's own optimizer is off under the tier, so a call to a function
//! declared after the code that calls it stays an `INIT_FCALL_BY_NAME`,
//! which php maps to no callee, and what it returns stays unknown.

use php_unit::decl::HookKind;
use php_unit::{Opcode, OperandKind, Opline};

use crate::{Body, Callee, Function, LinkedTable, Visibility};

/// `zend_call_info`: one call whose callee is known.
#[derive(Debug, Clone)]
pub(crate) struct Call<'a, C> {
    pub(crate) callee: Callee<'a, C>,
    /// The opline that begins it, `caller_init_opline`.
    init: usize,
    /// The opline that makes it, `caller_call_opline`; none for a
    /// frameless call and one whose `DO_*CALL` was not reached.
    made: Option<usize>,
    /// The opline that sends each argument, `arg_info[i].opline`, as many
    /// as the call passes, `num_args`; none for one sent by name or not
    /// seen.
    pub(crate) args: Vec<Option<usize>>,
    /// `send_unpack`: an argument passed by `SEND_UNPACK` or `SEND_ARRAY`.
    pub(crate) send_unpack: bool,
    /// `is_prototype`: a method a class below the callee's may override.
    pub(crate) is_prototype: bool,
    /// `is_frameless`: a `FRAMELESS_ICALL_*`, whose arguments are its
    /// operands.
    pub(crate) frameless: bool,
}

/// `func_info->callee_info` and `func_info->call_map`: the calls, and
/// for each opline the call it begins, makes or sends an argument of.
#[derive(Debug)]
pub(crate) struct Calls<'a, C> {
    calls: Vec<Call<'a, C>>,
    map: Vec<Option<usize>>,
}

impl<'a, C> Calls<'a, C> {
    /// `call_map[at]`.
    pub(crate) fn at(&self, at: usize) -> Option<&Call<'a, C>> {
        self.calls.get((*self.map.get(at)?)?)
    }
}

/// `zend_analyze_calls` with `ZEND_CALL_TREE`, then `zend_build_call_map`;
/// none for an op array that calls nothing php knows, for which php
/// builds no map.
pub(crate) fn analyse<'t, T: LinkedTable>(
    function: &Function<'_>,
    table: &'t T,
) -> Option<Calls<'t, T::Class>> {
    let body = function.body;
    let mut calls: Vec<Call<'t, T::Class>> = Vec::new();
    // `call_stack`, and `call_info`: the call being built, none where its
    // callee is unknown.
    let mut stack: Vec<Option<usize>> = Vec::new();
    let mut current: Option<usize> = None;
    for (at, op) in body.oplines.iter().enumerate() {
        match op.opcode {
            Opcode::InitFcall
            | Opcode::InitMethodCall
            | Opcode::InitStaticMethodCall
            | Opcode::InitParentPropertyHookCall => {
                stack.push(current);
                current = called(function, table, op).map(|(callee, is_prototype)| {
                    calls.push(Call {
                        callee,
                        init: at,
                        made: None,
                        args: vec![None; op.extended_value as usize],
                        send_unpack: false,
                        is_prototype,
                        frameless: false,
                    });
                    calls.len() - 1
                });
            }
            Opcode::InitFcallByName
            | Opcode::InitNsFcallByName
            | Opcode::InitDynamicCall
            | Opcode::New
            | Opcode::InitUserCall => {
                stack.push(current);
                current = None;
            }
            Opcode::FramelessIcall0
            | Opcode::FramelessIcall1
            | Opcode::FramelessIcall2
            | Opcode::FramelessIcall3 => {
                if let Some(callee) = table.frameless(op.extended_value) {
                    let n = frameless_args(op.opcode);
                    calls.push(Call {
                        callee,
                        init: at,
                        made: None,
                        args: vec![None; n],
                        send_unpack: false,
                        is_prototype: false,
                        frameless: true,
                    });
                }
            }
            Opcode::DoFcall
            | Opcode::DoIcall
            | Opcode::DoUcall
            | Opcode::DoFcallByName
            | Opcode::CallableConvert => {
                if let Some(call) = current.and_then(|c| calls.get_mut(c)) {
                    call.made = Some(at);
                }
                current = stack.pop().flatten();
            }
            Opcode::SendVal
            | Opcode::SendVar
            | Opcode::SendValEx
            | Opcode::SendVarEx
            | Opcode::SendFuncArg
            | Opcode::SendRef
            | Opcode::SendVarNoRef
            | Opcode::SendVarNoRefEx
            | Opcode::SendUser => {
                // An argument sent by name, `named_args`, is no
                // positional one.
                if let Some(call) = current.and_then(|c| calls.get_mut(c))
                    && op.op2_type != OperandKind::Const
                    && let Some(arg) = call.args.get_mut(op.op2.saturating_sub(1) as usize)
                {
                    *arg = Some(at);
                }
            }
            Opcode::SendArray | Opcode::SendUnpack => {
                if let Some(call) = current.and_then(|c| calls.get_mut(c)) {
                    call.send_unpack = true;
                }
            }
            _ => {}
        }
    }
    if calls.is_empty() {
        return None;
    }
    let mut map = vec![None; body.oplines.len()];
    // php walks `callee_info`, the calls last first, so the earliest call
    // that names an opline keeps it.
    for (c, call) in calls.iter().enumerate().rev() {
        let sent = (!call.frameless)
            .then_some(&call.args)
            .into_iter()
            .flatten();
        for at in [Some(call.init), call.made]
            .into_iter()
            .chain(sent.copied())
        {
            if let Some(slot) = at.and_then(|at| map.get_mut(at)) {
                *slot = Some(c);
            }
        }
    }
    Some(Calls { calls, map })
}

/// `ZEND_FLF_NUM_ARGS`.
fn frameless_args(opcode: Opcode) -> usize {
    match opcode {
        Opcode::FramelessIcall1 => 1,
        Opcode::FramelessIcall2 => 2,
        Opcode::FramelessIcall3 => 3,
        _ => 0,
    }
}

/// `zend_optimizer_get_called_func` with `script` null: the function the
/// call `op` begins reaches, and whether a class below it may override
/// it.
fn called<'t, T: LinkedTable>(
    function: &Function<'_>,
    table: &'t T,
    op: &Opline,
) -> Option<(Callee<'t, T::Class>, bool)> {
    let literal = |n: u32| function.body.literals.get(n as usize);
    // The lower-cased name the compiler wrote after the constant `n`.
    let lcname = |n: u32| {
        literal(n)?.as_str()?;
        literal(n + 1)?.str_bytes()
    };
    match op.opcode {
        Opcode::InitFcall => Some((table.function(literal(op.op2)?.str_bytes()?)?, false)),
        Opcode::InitStaticMethodCall if op.op2_type == OperandKind::Const => {
            let class = crate::class_of_op1(function, table, op)?;
            let fbc = table.method(class, lcname(op.op2)?)?;
            let method = fbc.method?;
            let reachable =
                method.visibility == Visibility::Public || Some(method.scope) == table.scope();
            (!method.is_abstract && reachable).then_some((fbc, false))
        }
        Opcode::InitMethodCall
            if op.op1_type == OperandKind::Unused && op.op2_type == OperandKind::Const =>
        {
            let scope = table.scope().filter(|s| !table.is_trait(*s))?;
            let fbc = table.method(scope, lcname(op.op2)?)?;
            let method = fbc.method?;
            if method.visibility == Visibility::Private {
                // A private method is used only from its own class, and
                // never as a prototype, as a class below may declare one of
                // another signature.
                return (method.scope == scope).then_some((fbc, false));
            }
            let is_prototype = !method.is_final && !table.is_final(method.scope);
            Some((fbc, is_prototype))
        }
        Opcode::InitParentPropertyHookCall => {
            let kind = if op.op2 == 0 {
                HookKind::Get
            } else {
                HookKind::Set
            };
            let name = literal(op.op1)?.str_bytes()?;
            Some((table.parent_hook(name, kind)?, false))
        }
        _ => None,
    }
}

impl<C> Callee<'_, C> {
    /// Whether the callee is the op array analysed, whose own return a
    /// recursive tail call returns.
    pub(crate) fn is_analysed(&self) -> bool {
        matches!(self.body, Body::Analysed)
    }
}
