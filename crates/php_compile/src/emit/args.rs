//! A call site's arguments, after `zend_compile_args`
//! (`Z/Zend/zend_compile.c:3717`): each argument read off the list as
//! written, placed where php places it, and sent by the `SEND_*` opcode
//! php picks from what the argument is and what the table knows of the
//! callee.
//!
//! The placement reads the names and the callee alone, so it is decided
//! before any argument compiles ([`Plan`]): an argument written
//! positionally goes at its number; one written `name:` goes at the
//! number of the parameter the callee declares under that name where
//! the table has the callee and every argument before it went in order,
//! which is php's "passing in order" and no named argument at all; and
//! everywhere else at its name, an `IS_CONST` in `op2` the engine places
//! by `zend_handle_named_arg` as the program serves. The count the
//! `INIT_*` carries is the positional arguments', a `CHECK_UNDEF_ARGS`
//! follows the last argument where a named one may have skipped a
//! parameter, and the call op carries `ZEND_FCALL_MAY_HAVE_EXTRA_NAMED_PARAMS`
//! where a name may land in a variadic.
//!
//! A variable sent to a callee the compiler knows nothing of is fetched
//! as php fetches it: a CV as it stands, `$this` by `FETCH_THIS`, and a
//! dimension, a property, a class's property or a variable fetched by
//! name, an auto-global or `$$x`, by the `_FUNC_ARG` fetch under a
//! `CHECK_FUNC_ARG`, which reads the parameter's
//! by-reference bit off the callee as the program serves and fetches
//! for writing or for reading by it; `SEND_FUNC_ARG` then sends what
//! was fetched the same way.

use php_syntax::Arena;
use php_syntax::tree::{self, Node, NodeId};
use php_unit::decl::Parameter;
use php_unit::{AutoGlobal, Opcode, OperandKind, Opline};

use super::{Emit, Fetch, Over, SimpleVar, Source, Where, walk};
use crate::CompileError;

/// One argument at a call site, php's child of a `ZEND_AST_ARG_LIST`:
/// a plain one, one written `name: value` (`ZEND_AST_NAMED_ARG`), or one
/// written `...value` (`ZEND_AST_UNPACK`). A plain or a named one is a
/// [`Source`]: the expression written, or a value the compiler holds,
/// which is the value a pipe sends and the description
/// `zend_compile_assert` adds to the list.
#[derive(Clone, Copy)]
pub(super) enum Arg<'a> {
    Positional(Source),
    Named(&'a [u8], Source),
    Unpack(tree::Expr),
}

/// What a call site's argument list is, php's `args_ast->kind`, read
/// once where the call is: the arguments as written with the list's own
/// line, php's `zend_ast_get_lineno` over the `ZEND_AST_ARG_LIST`, which
/// a variable sent to a callee nothing is known of is sent at; or the
/// `...` of a first-class callable, `ZEND_AST_CALLABLE_CONVERT`, which
/// sends nothing and makes a closure of the call its `INIT_*` began
/// (`zend_compile_call_common`).
/// The list is in the compile's arena, as every temporary of the
/// compiler's that dies with it.
pub(super) enum Args<'a> {
    Written { list: &'a [Arg<'a>], line: u32 },
    CallableConvert,
}

impl<'a> Args<'a> {
    /// A list the compiler makes itself, of plain expressions at `line`.
    pub(super) fn of(arena: &'a Arena, list: &[tree::Expr], line: u32) -> Args<'a> {
        let mut made = arena.vec(list.len());
        for expr in list {
            made.push(Arg::Positional(Source::Expr(*expr)));
        }
        Args::Written {
            list: made.into_slice(),
            line,
        }
    }

    /// The arguments as written; none for a first-class callable's.
    pub(super) fn list(&self) -> &'a [Arg<'a>] {
        match self {
            Args::Written { list, .. } => list,
            Args::CallableConvert => &[],
        }
    }

    /// `!zend_args_contain_unpack_or_named`: the arguments as plain
    /// expressions where none is named or unpacked, which is all a
    /// special function, a frameless call and a constant fold read;
    /// none where one is, and none for a first-class callable, which
    /// php keeps out of all three by `is_callable_convert`.
    pub(super) fn positional(&self) -> Option<Vec<Source>> {
        let Args::Written { list, .. } = self else {
            return None;
        };
        list.iter()
            .map(|arg| match arg {
                Arg::Positional(value) => Some(*value),
                Arg::Named(..) | Arg::Unpack(_) => None,
            })
            .collect()
    }

    pub(super) fn len(&self) -> usize {
        self.list().len()
    }
}

/// How a call site whose callee the table has sends one argument, php's
/// `ZEND_SEND_BY_VAL`, `ZEND_SEND_BY_REF` and `ZEND_SEND_PREFER_REF`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SendMode {
    Value,
    Ref,
    PreferRef,
}

impl SendMode {
    fn of(parameter: &Parameter) -> SendMode {
        match (parameter.by_ref, parameter.prefer_ref) {
            (true, true) => SendMode::PreferRef,
            (true, false) => SendMode::Ref,
            (false, _) => SendMode::Value,
        }
    }
}

/// php's `fbc` as `zend_compile_args` reads it: the callee the table
/// has at a call site, each declared parameter's name and way of being
/// sent, and the variadic's way where it collects the rest.
pub(super) struct Callee {
    parameters: Vec<(php_heap::Bytes, SendMode)>,
    variadic: Option<SendMode>,
}

impl Callee {
    pub(super) fn of(parameters: &[Parameter]) -> Callee {
        Callee {
            parameters: parameters
                .iter()
                .filter(|parameter| !parameter.variadic)
                .map(|parameter| (parameter.name.0.clone(), SendMode::of(parameter)))
                .collect(),
            variadic: parameters
                .iter()
                .find(|parameter| parameter.variadic)
                .map(SendMode::of),
        }
    }

    /// `zend_get_arg_num` (`Z/Zend/zend_compile.c:3689`): the number,
    /// from one, of the declared parameter `name` names, byte for byte;
    /// none for a name the callee declares no parameter for, the
    /// variadic's own among them, since php walks `num_args` alone.
    fn arg_num(&self, name: &[u8]) -> Option<u32> {
        let at = self
            .parameters
            .iter()
            .position(|(declared, _)| **declared == *name)?;
        u32::try_from(at + 1).ok()
    }

    /// `ZEND_ARG_SEND_MODE` over `zend_check_arg_send_type`: the
    /// declared parameter's way at `number`, the variadic's past them
    /// and by value past a callee without one; `None` is php's
    /// `(uint32_t) -1`, which is past every parameter.
    fn mode(&self, number: Option<u32>) -> SendMode {
        number
            .and_then(|number| usize::try_from(number.checked_sub(1)?).ok())
            .and_then(|at| self.parameters.get(at))
            .map(|(_, mode)| *mode)
            .or(self.variadic)
            .unwrap_or(SendMode::Value)
    }
}

/// Where a `SEND_*` or a `CHECK_FUNC_ARG` puts its argument, php's
/// `op2`: its number, or its name, an `IS_CONST` the engine places by
/// `zend_handle_named_arg`.
#[derive(Clone, Copy)]
enum ArgAt<'a> {
    Number(u32),
    Name(&'a [u8]),
}

/// One argument as [`Plan`] placed it: where it goes, and the number of
/// the parameter it fills where the table has the callee and knows it,
/// php's `arg_num` other than `(uint32_t) -1`; an unpacked one, which
/// the engine places element by element after the `count` placed
/// before it (`SEND_UNPACK`); or php's compile error for an argument
/// the list may not have where it stands, raised where the compiler
/// reaches it.
#[derive(Clone, Copy)]
enum Placed<'a> {
    Send {
        value: Source,
        at: ArgAt<'a>,
        number: Option<u32>,
    },
    Unpack {
        value: tree::Expr,
        count: u32,
    },
    Refused(&'static str),
}

/// `zend_compile_args`' placement of a whole list, which reads the
/// names and the callee alone.
pub(super) struct Plan<'a> {
    placed: &'a [Placed<'a>],
    /// The number of arguments placed at their number, the
    /// `extended_value` of the `INIT_*` and what `used_stack` counts.
    pub(super) count: u32,
    /// `may_have_undef`: a named argument went at its name, so the
    /// engine may leave a parameter unfilled and `CHECK_UNDEF_ARGS`
    /// follows the last argument.
    may_have_undef: bool,
    /// `may_have_extra_named_args`: a name may land in a variadic,
    /// `ZEND_FCALL_MAY_HAVE_EXTRA_NAMED_PARAMS` on the call op.
    pub(super) extra_named: bool,
}

impl<'a> Plan<'a> {
    /// The plan of `arguments`, laid in `arena`.
    pub(super) fn of(arena: &'a Arena, arguments: &Args<'a>, callee: Option<&Callee>) -> Plan<'a> {
        let mut placed_all = arena.vec(arguments.len());
        let mut plan = Plan {
            placed: &[],
            count: 0,
            may_have_undef: false,
            extra_named: false,
        };
        let mut uses_named = false;
        // `fbc = NULL` once an argument is unpacked: what follows is sent
        // for a callee nothing is known of.
        let mut callee = callee;
        let mut uses_unpack = false;
        for argument in arguments.list() {
            let (name, value) = match argument {
                Arg::Unpack(value) => {
                    let placed = if uses_named {
                        Placed::Refused("Cannot use argument unpacking after named arguments")
                    } else {
                        // "Unpack may contain named arguments."
                        plan.may_have_undef = true;
                        if callee.is_none_or(|callee| callee.variadic.is_some()) {
                            plan.extra_named = true;
                        }
                        uses_unpack = true;
                        callee = None;
                        Placed::Unpack {
                            value: *value,
                            count: plan.count,
                        }
                    };
                    placed_all.push(placed);
                    continue;
                }
                Arg::Positional(value) => (None, *value),
                Arg::Named(name, value) => (Some(name), *value),
            };
            let placed = match name {
                None if uses_unpack => {
                    Placed::Refused("Cannot use positional argument after argument unpacking")
                }
                None if uses_named => {
                    Placed::Refused("Cannot use positional argument after named argument")
                }
                None => {
                    plan.count += 1;
                    Placed::Send {
                        value,
                        at: ArgAt::Number(plan.count),
                        number: Some(plan.count),
                    }
                }
                Some(name) => {
                    uses_named = true;
                    let number = callee.and_then(|callee| callee.arg_num(name));
                    if callee.is_some() && number == Some(plan.count + 1) && !plan.may_have_undef {
                        // "Using named arguments, but passing in order."
                        plan.count += 1;
                        Placed::Send {
                            value,
                            at: ArgAt::Number(plan.count),
                            number,
                        }
                    } else {
                        plan.may_have_undef = true;
                        if number.is_none() && callee.is_none_or(|callee| callee.variadic.is_some())
                        {
                            plan.extra_named = true;
                        }
                        Placed::Send {
                            value,
                            at: ArgAt::Name(name),
                            number,
                        }
                    }
                }
            };
            placed_all.push(placed);
        }
        plan.placed = placed_all.into_slice();
        plan
    }
}

impl<'a> Emit<'a> {
    /// The arguments of a call as written, or the `...` of a first-class
    /// callable, which the parser lets stand only as the whole list. An
    /// unpacked argument after a named one is php's compile error, and
    /// so is a positional one after an unpacked one, each raised where
    /// the compiler reaches it:
    ///
    /// ```php
    /// function f(...$a) {}
    /// f(x: 1, ...[2]);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use argument unpacking after named arguments in %s on line 3
    /// ```
    pub(super) fn arguments(
        &self,
        id: NodeId<tree::ArgList>,
        line: u32,
    ) -> Result<Args<'a>, CompileError> {
        let node = &self.parsed.tree[id];
        let mut list = self.arena.vec(node.args.len());
        for arg in self.list(node.args) {
            match arg {
                tree::Arg::Expr(expr) => list.push(Arg::Positional(Source::Expr(*expr))),
                tree::Arg::NamedArg(named) => {
                    let named = &self.parsed.tree[*named];
                    let Some(value) = named.value else {
                        return Err(walk::unread(line, "a named argument"));
                    };
                    list.push(Arg::Named(self.text(named.name), Source::Expr(value)));
                }
                tree::Arg::SpreadArg(spread) => {
                    let Some(value) = self.parsed.tree[*spread].value else {
                        return Err(walk::unread(line, "an unpacked argument"));
                    };
                    list.push(Arg::Unpack(value));
                }
                tree::Arg::VariadicPlaceholder(_) => return Ok(Args::CallableConvert),
            }
        }
        Ok(Args::Written {
            list: list.into_slice(),
            line: node.line(),
        })
    }

    /// `zend_compile_args`' loop over a [`Plan`], each argument compiled
    /// and sent in turn, then `CHECK_UNDEF_ARGS` where the plan says a
    /// parameter may be left unfilled.
    ///
    /// A named argument lands at the parameter of its name, a name the
    /// callee declares no parameter for in its variadic under that name,
    /// and the natives that take the rest by name, `call_user_func`
    /// first, hand them on:
    ///
    /// ```php
    /// function f($a, ...$rest) { var_dump($a, $rest); }
    /// f(1, x: 2);
    /// f(y: 3, a: 4);
    /// var_dump(call_user_func('str_pad', 'abc', 6, pad_type: STR_PAD_LEFT));
    /// ```
    /// ```expect
    /// int(1)
    /// array(1) {
    ///   ["x"]=>
    ///   int(2)
    /// }
    /// int(4)
    /// array(1) {
    ///   ["y"]=>
    ///   int(3)
    /// }
    /// string(6) "   abc"
    /// ```
    ///
    /// A positional argument after a named one is php's compile error,
    /// raised where the compiler reaches it:
    ///
    /// ```php
    /// function f($x, $y) {}
    /// f(x: 1, 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use positional argument after named argument in %s on line 3
    /// ```
    pub(super) fn send_args(
        &mut self,
        plan: &Plan,
        arguments: &Args,
        callee: Option<&Callee>,
    ) -> Result<(), CompileError> {
        // A first-class callable sends nothing.
        let Args::Written {
            line: list_line, ..
        } = *arguments
        else {
            return Ok(());
        };
        for placed in plan.placed {
            match placed {
                Placed::Send { value, at, number } => {
                    self.send_arg(*value, *at, callee, *number, list_line)?;
                }
                Placed::Unpack { value, count } => self.send_unpack(*value, *count)?,
                Placed::Refused(message) => return Err(self.compile_error(*message)),
            }
        }
        if plan.may_have_undef {
            self.push(Opline {
                opcode: Opcode::CheckUndefArgs,
                ..Opline::default()
            });
        }
        Ok(())
    }

    /// One argument of `zend_compile_args`, sent by the opcode php picks
    /// from its shape: a call, a variable, or any other expression.
    /// `number` is the parameter the argument fills where the table has
    /// the callee and knows it, php's `fbc && arg_num != (uint32_t) -1`,
    /// and the callee's way of taking it decides between the plain
    /// sends and `SEND_REF`; everywhere else the `_EX` sends read that
    /// off the callee as the program serves.
    ///
    /// ```php
    /// $a = [2, 1];
    /// function g() { return [2, 1]; }
    /// function &h() { global $a; return $a; }
    /// function r(&$x) { $x = 'r'; }
    /// var_dump(sort(g()), sort(h()), h());
    /// r(x: $v);
    /// var_dump($v, str_repeat(times: 2, string: strtoupper('a')));
    /// ```
    /// ```expect
    /// Notice: Only variables should be passed by reference in %s on line 6
    /// bool(true)
    /// bool(true)
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// string(1) "r"
    /// string(2) "AA"
    /// ```
    fn send_arg(
        &mut self,
        value: Source,
        at: ArgAt<'_>,
        callee: Option<&Callee>,
        number: Option<u32>,
        list_line: u32,
    ) -> Result<(), CompileError> {
        let mode = callee.map(|callee| callee.mode(number));
        let known = mode.filter(|_| number.is_some());
        // A `ZEND_AST_ZNODE` is no call and no variable: the value it
        // holds is sent as any other expression's is.
        let value = match value {
            Source::Expr(value) => self.peel(value),
            Source::Node(held) => return self.send_value(held, &at, known),
        };
        // "Treat passing of $GLOBALS the same as passing a call": its
        // `FETCH_GLOBALS` temporary is `SEND_VAL_EX` where the parameter
        // may take a reference, which is the error the program meets.
        if self.is_call(value) || self.is_globals(value) {
            // `zend_is_call`: the call's `VAR`, or the constant or the
            // temporary a call php served itself left.
            let returned = self.expr(value)?;
            let operand = self.operand(returned);
            let opcode = match (operand.0, known) {
                (OperandKind::Const | OperandKind::Tmp, _) => {
                    if mode.is_none_or(|mode| mode == SendMode::Ref) {
                        Opcode::SendValEx
                    } else {
                        Opcode::SendVal
                    }
                }
                (_, Some(SendMode::Ref)) => Opcode::SendVarNoRef,
                (_, Some(SendMode::PreferRef)) => Opcode::SendVal,
                (_, Some(SendMode::Value)) => Opcode::SendVar,
                (_, None) => Opcode::SendVarNoRefEx,
            };
            return self.send(opcode, operand, &at);
        }
        if let Some(variable) = self.variable_of(value)? {
            return self.send_variable(value, variable, &at, known, list_line);
        }
        let computed = self.expr(value)?;
        self.send_value(computed, &at, known)
    }

    /// `zend_compile_args`' last arm, an argument that is no call and no
    /// variable, sent from where it left its value by the opcode its
    /// operand kind and the parameter's way of being sent pick.
    fn send_value(
        &mut self,
        computed: Where,
        at: &ArgAt,
        known: Option<SendMode>,
    ) -> Result<(), CompileError> {
        let operand = self.operand(computed);
        let opcode = match (operand.0, known) {
            // "pass ++$a or something similar"
            (OperandKind::Var, Some(SendMode::Ref)) => Opcode::SendVarNoRef,
            (OperandKind::Var | OperandKind::Cv, Some(SendMode::Value)) => Opcode::SendVar,
            (OperandKind::Var, None) => Opcode::SendVarNoRefEx,
            (OperandKind::Cv, Some(SendMode::Ref | SendMode::PreferRef)) => Opcode::SendRef,
            (OperandKind::Cv, None) => Opcode::SendVarEx,
            // "Delay 'Only variables can be passed by reference' error
            // to execution", and a `VAR` php passes through as it stands
            // where the parameter takes a reference only where it can.
            (_, Some(SendMode::Value | SendMode::PreferRef)) => Opcode::SendVal,
            (_, Some(SendMode::Ref) | None) => Opcode::SendValEx,
        };
        self.send(opcode, operand, at)
    }

    /// `zend_is_variable`'s arm of `zend_compile_args`, a variable the
    /// argument is: where the table knows the parameter, fetched for
    /// writing and sent by `SEND_REF` where it takes its argument by
    /// reference, and read and sent by value otherwise; where it does
    /// not, a CV sent as it stands, `$this` fetched into a `VAR`, and
    /// anything else fetched by the `_FUNC_ARG` opcodes under a
    /// `CHECK_FUNC_ARG` and sent by `SEND_FUNC_ARG`.
    ///
    /// ```php
    /// class C {
    ///     public $p = [3, 1, 2]; public static $s = [2, 1];
    ///     function byref(&$a) { sort($a); }
    ///     function byval($a) { return $a; }
    /// }
    /// $o = new C; $a = ['k' => [9, 8]];
    /// sort($a['k']); sort(C::$s);
    /// $o->byref($o->p);
    /// $o->byref(a: $a['k']);
    /// var_dump($a, $o->p, C::$s, $o->byval($o->p[0]), $o->byval(a: C::$s));
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["k"]=>
    ///   array(2) {
    ///     [0]=>
    ///     int(8)
    ///     [1]=>
    ///     int(9)
    ///   }
    /// }
    /// array(3) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    ///   [2]=>
    ///   int(3)
    /// }
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// int(1)
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// ```
    fn send_variable(
        &mut self,
        value: tree::Expr,
        variable: Variable,
        at: &ArgAt,
        known: Option<SendMode>,
        list_line: u32,
    ) -> Result<(), CompileError> {
        match (known, variable) {
            // `zend_compile_var(&arg_node, arg, BP_VAR_W, 1)`, which
            // stands on the argument's own line.
            (Some(SendMode::Ref | SendMode::PreferRef), Variable::Cv(cv)) => {
                self.line = self.line_of(value);
                self.cv_numbered(cv, Fetch::Write);
                self.send(Opcode::SendRef, (OperandKind::Cv, cv), at)
            }
            (Some(SendMode::Ref | SendMode::PreferRef), Variable::This) => {
                self.line = self.line_of(value);
                let this = self.fetch_this(OperandKind::Var);
                let operand = self.operand(this);
                self.send(Opcode::SendRef, operand, at)
            }
            (Some(SendMode::Ref | SendMode::PreferRef), Variable::Fetched) => {
                self.line = self.line_of(value);
                let operand = self.var_for_reference(value)?;
                self.send(Opcode::SendRef, operand, at)
            }
            (Some(SendMode::Value), _) => {
                let read = self.expr(value)?;
                let operand = self.operand(read);
                let opcode = if operand.0 == OperandKind::Tmp {
                    Opcode::SendVal
                } else {
                    Opcode::SendVar
                };
                self.send(opcode, operand, at)
            }
            (None, Variable::Cv(cv)) => {
                self.line = list_line;
                self.cv_numbered(cv, Fetch::Read);
                self.send(Opcode::SendVarEx, (OperandKind::Cv, cv), at)
            }
            (None, Variable::This) => {
                self.line = list_line;
                let this = self.fetch_this(OperandKind::Var);
                let operand = self.operand(this);
                self.send(Opcode::SendVarEx, operand, at)
            }
            // The `CHECK_FUNC_ARG` stands where the compiler stood, and
            // `zend_compile_var` moves to the argument's line after it.
            (None, Variable::Fetched) => {
                let (op2_type, op2, out) = self.arg_operand(at)?;
                self.push(Opline {
                    opcode: Opcode::CheckFuncArg,
                    op2_type,
                    op2,
                    out: if op2_type == OperandKind::Const {
                        out
                    } else {
                        0
                    },
                    ..Opline::default()
                });
                self.line = self.line_of(value);
                let held = self.delayed_var(value, Fetch::FuncArg)?;
                let operand = self.base_fetch(held, Fetch::FuncArg, Over::Ref);
                self.send(Opcode::SendFuncArg, operand, at)
            }
        }
    }

    /// `zend_is_variable` over what `zend_compile_args` sends: a plain
    /// variable's CV, `$this`, or a dimension, a property, a class's
    /// property, an auto-global or a variable named by a value the
    /// program computes, which are fetched; none for anything else. A
    /// chain a `?->` stands in is sent as the value it reads, php's
    /// `zend_is_variable(arg) && !zend_ast_is_short_circuited(arg)`.
    ///
    /// ```php
    /// function v($x) { var_dump($x); }
    /// $o = null;
    /// v($o?->p);
    /// u($o?->p['k']);
    /// $o = new stdClass;
    /// $o->p = ['k' => 'v'];
    /// u($o?->p['k']);
    /// try { r($o?->p); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// function u($x) { var_dump($x); }
    /// function r(&$x) {}
    /// ```
    /// ```expect
    /// NULL
    /// NULL
    /// string(1) "v"
    /// r(): Argument #1 ($x) could not be passed by reference
    /// ```
    fn variable_of(&mut self, value: tree::Expr) -> Result<Option<Variable>, CompileError> {
        if self.is_short_circuited(value) {
            return Ok(None);
        }
        Ok(match value {
            tree::Expr::SimpleVarExpr(id) => Some(match self.simple_var_of(id, self.line)? {
                SimpleVar::Cv(name) => Variable::Cv(self.lookup_cv(name)),
                SimpleVar::This => Variable::This,
                // `zend_try_compile_cv` refuses an auto-global, so it is
                // fetched by its name from `EG(symbol_table)`.
                SimpleVar::Auto(_) => Variable::Fetched,
                // [`Self::send_arg`] sends `$GLOBALS` as a call.
                SimpleVar::Globals => return Ok(None),
            }),
            tree::Expr::VarVarExpr(_) | tree::Expr::DynamicVarExpr(_) => {
                self.named_variable(value)?
            }
            tree::Expr::SubscriptExpr(_) | tree::Expr::MemberAccessExpr(_) => {
                Some(Variable::Fetched)
            }
            tree::Expr::ScopedAccessExpr(_) if self.is_static_prop(value) => {
                Some(Variable::Fetched)
            }
            _ => None,
        })
    }

    /// `$$x` or `${expr}` as `zend_compile_args` sorts it, nothing
    /// compiled to answer: a name the program computes is no
    /// `ZEND_AST_ZVAL`, which `zend_try_compile_cv` refuses, and so is an
    /// auto-global's written out, both fetched by name; `${'this'}` is
    /// `is_this_fetch`'s; any other name written out is the CV it names,
    /// or `$GLOBALS`, and is sent as the value it reads.
    ///
    /// So to a callee nothing is known of, an auto-global and `$$x` are
    /// `CHECK_FUNC_ARG` and `FETCH_FUNC_ARG` from the table they name,
    /// and a parameter by reference binds the variable itself; `$GLOBALS`
    /// is sent as a call's value is, which such a parameter refuses; and
    /// to a callee the table has, `sort($_COOKIE)` is `FETCH_W (global)`
    /// and `SEND_REF`:
    ///
    /// ```php
    /// function run() {
    ///     fill($_ENV);
    ///     $n = 'loc';
    ///     fill($$n);
    ///     fill(v: ${'_POST'});
    ///     var_dump($_ENV, $loc, $_POST, byval($_GET), byval($$n));
    ///     try { fill($GLOBALS); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///     sort($_COOKIE);
    /// }
    /// $_COOKIE = [3, 1];
    /// run();
    /// var_dump($_COOKIE);
    /// function fill(&$v) { $v = 'filled'; }
    /// function byval($v) { return $v; }
    /// ```
    /// ```expect
    /// string(6) "filled"
    /// string(6) "filled"
    /// string(6) "filled"
    /// array(0) {
    /// }
    /// string(6) "filled"
    /// fill(): Argument #1 ($v) could not be passed by reference
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(3)
    /// }
    /// ```
    fn named_variable(&mut self, value: tree::Expr) -> Result<Option<Variable>, CompileError> {
        let inner = match value {
            tree::Expr::VarVarExpr(id) => self.parsed.tree[id].inner,
            tree::Expr::DynamicVarExpr(id) => self.parsed.tree[id].inner,
            _ => None,
        };
        let Some(inner) = inner else {
            return Ok(None);
        };
        let Some(written) = self.written(inner)? else {
            return Ok(Some(Variable::Fetched));
        };
        let written = self.stringified(written)?.unwrap_or(written);
        let Some(name) = written.as_str() else {
            return Ok(None);
        };
        let name = name.bytes();
        Ok(if name == b"this" {
            Some(Variable::This)
        } else if AutoGlobal::of(name).is_some() {
            Some(Variable::Fetched)
        } else {
            None
        })
    }

    /// `zend_compile_args`' unpack arm: the value compiled, then
    /// `SEND_UNPACK` over it with the number of arguments placed before
    /// it in `op2`, which the engine places the elements after
    /// (`ZEND_CALL_NUM_ARGS`), and the frame slot of the last of them in
    /// `result`, which php writes and never reads.
    fn send_unpack(&mut self, value: tree::Expr, count: u32) -> Result<(), CompileError> {
        let value = self.expr(value)?;
        let (op1_type, op1) = self.operand(value);
        self.push(Opline {
            opcode: Opcode::SendUnpack,
            op1_type,
            op1,
            op2: count,
            out: count.saturating_sub(1),
            ..Opline::default()
        });
        Ok(())
    }

    /// The `SEND_*` itself over `operand`, at where the plan put it.
    fn send(
        &mut self,
        opcode: Opcode,
        (op1_type, op1): (OperandKind, u32),
        at: &ArgAt,
    ) -> Result<(), CompileError> {
        let (op2_type, op2, out) = self.arg_operand(at)?;
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            op2_type,
            op2,
            out,
            ..Opline::default()
        });
        Ok(())
    }

    /// The `op2` and `result` of a send: the argument's number with the
    /// frame's slot for it (`EX_NUM_TO_VAR(arg_num - 1)`), or its name as
    /// one constant (`zend_add_literal_string`) with two cache slots
    /// (`zend_alloc_cache_slots(2)`), in that order.
    fn arg_operand(&mut self, at: &ArgAt) -> Result<(OperandKind, u32, u32), CompileError> {
        Ok(match at {
            ArgAt::Number(number) => (OperandKind::Unused, *number, number.saturating_sub(1)),
            ArgAt::Name(name) => {
                let name = self.string(name)?;
                let op2 = self.literal(name);
                (OperandKind::Const, op2, self.cache_slots(2))
            }
        })
    }
}

/// What [`Emit::variable_of`] found an argument to be.
enum Variable {
    Cv(u32),
    This,
    Fetched,
}
