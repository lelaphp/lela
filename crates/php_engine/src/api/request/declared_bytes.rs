//! What php's compile `emalloc`s for each op array and class a
//! request-only unit declares, held beside the unit's strings
//! ([`super::unit_strings`]) and given back where php gives it back
//! (#3042). The engine's op arrays and classes are the unit's and the
//! linked tables'; these are php's bytes, so `memory_get_usage` reads
//! what php's does after an `eval` that declares a function, a closure
//! or a class.
//!
//! An op array php compiles for one request is laid out by
//! `init_op_array` (`Z/Zend/zend_opcode.c:47`) and `pass_two`
//! (`Z/Zend/zend_opcode.c:1086`): its refcount, `emalloc(sizeof(uint32_t))`;
//! its oplines, `INITIAL_OP_ARRAY_SIZE` of them, grown fourfold as the
//! compile fills them, and reallocated by `pass_two` to the oplines
//! with the constants after them; its variables' names, grown sixteen at
//! a time by `lookup_cv` and reallocated to their number; its
//! `arg_info`, one more for a return type (`zend_compile_params`); its
//! live ranges and `try`/`catch` elements, reallocated one by one; the
//! pointers to the functions it declares (`zend_add_dynamic_func_def`);
//! and its static variables, `zend_new_array(8)` and a bucket each. The
//! `zend_op_array` itself is carved from `CG(arena)`, as a class's
//! `zend_class_entry` is. A class adds the tables `zend_initialize_class_data`
//! lays down and its declarations fill: its methods', its properties'
//! and its constants', and the default values of its properties.
//!
//! A function or a class is held by the request's tables to the end of
//! the request, and so is what it declared; a dynamic function
//! definition, a closure's or one a `DECLARE_FUNCTION` binds, is held by
//! the main op array that declares it until that op array's execution
//! ends (`destroy_op_array`'s `dynamic_func_defs` loop,
//! `Z/Zend/zend_opcode.c:649`), and past it by each closure over it
//! (`(*op_array->refcount)++` in `zend_create_closure_ex`) and by the
//! function table once a `DECLARE_FUNCTION` bound it.

use php_unit::decl::{ClassDecl, ClassKind, ConstExpr, EnumBacking, FunctionDecl};
use php_unit::{OpArray, Unit};

use super::Request;
use super::hash_bytes::HashBytes;
use crate::api::raising::Thrown;
use crate::api::values::Emalloc;

/// `sizeof(zend_op)`.
const OPLINE: usize = 32;
/// `sizeof(zval)`.
const ZVAL: usize = 16;
/// `INITIAL_OP_ARRAY_SIZE` (`Z/Zend/zend_compile.h`).
const INITIAL_OPLINES: usize = 64;
/// `sizeof(zend_string *)`, an entry of `op_array->vars` and of
/// `op_array->dynamic_func_defs`.
const POINTER: usize = 8;
/// `lookup_cv`'s step: sixteen more names at a time.
const VARS_STEP: usize = 16;
/// `sizeof(zend_arg_info)`.
const ARG_INFO: usize = 32;
/// `sizeof(zend_live_range)`.
const LIVE_RANGE: usize = 12;
/// `sizeof(zend_try_catch_element)`.
const TRY_CATCH: usize = 16;
/// `sizeof(uint32_t)`, the op array's refcount.
const REFCOUNT: usize = 4;
/// The size `zend_initialize_class_data` gives each of a class's tables.
const CLASS_TABLE: u32 = 8;
/// `zend_ast_size(4)` and the two `zend_ast_zval` children of an enum
/// case's `ZEND_AST_CONST_ENUM_INIT`, its class's name and its own.
const CASE_AST: usize = 88;
/// `sizeof(zend_ast_zval)`, the node of a value written in place.
const AST_ZVAL: usize = 24;

/// php's blocks for what one request-only unit declares.
#[derive(Default)]
pub(super) struct Declared {
    /// Whether they were laid down, once for the unit.
    laid: bool,
    /// A function's, a class's and its methods' and hooks', held to the
    /// unit's end.
    tables: Blocks,
    /// Each dynamic function definition's, by its place in
    /// `Unit::dynamic`, and what holds it past the main op array.
    dynamic: Vec<Dynamic>,
}

/// One dynamic definition's blocks and what holds them: the op array
/// that declares it, by its `dynamic_func_defs`, each closure over it
/// and the function table once it is bound. php gives the op array back
/// at the last of them (`destroy_op_array`'s count).
#[derive(Default)]
struct Dynamic {
    blocks: Blocks,
    /// The op array whose `dynamic_func_defs` holds it.
    declarer: Declarer,
    /// The closures over it that stand.
    closures: usize,
    /// Whether a `DECLARE_FUNCTION` bound it.
    bound: bool,
    /// Whether its blocks were given back.
    gone: bool,
}

/// The op array a dynamic definition is declared in.
#[derive(Default, Clone, Copy)]
pub(super) enum Declarer {
    /// The unit's main op array, which ends with its execution.
    Main,
    /// Another dynamic definition, which goes with its own last count.
    Dynamic(usize),
    /// A function, a method or a hook, which the request's tables hold
    /// to the unit's end.
    #[default]
    Table,
}

/// What a unit declares, laid down: a function's, a class's and its
/// methods' and hooks' blocks together, and each dynamic definition's
/// apart with the op array that declares it.
#[derive(Default)]
pub(super) struct Laid {
    tables: Blocks,
    dynamic: Vec<(Blocks, Declarer)>,
}

impl Laid {
    /// Every block given back.
    pub(super) fn free(self, req: &mut Request) {
        self.tables.free(req);
        for (blocks, _) in self.dynamic {
            blocks.free(req);
        }
    }
}

/// `emalloc`'d blocks and hash tables, given back together.
#[derive(Default)]
pub(super) struct Blocks {
    blocks: Vec<Emalloc>,
    tables: Vec<HashBytes>,
}

impl Blocks {
    /// Whether it holds no block.
    fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.tables.is_empty()
    }

    /// Every block given back.
    pub(super) fn free(self, req: &mut Request) {
        for block in self.blocks {
            block.free(req);
        }
        for table in self.tables {
            table.free(req);
        }
    }

    /// `others`' blocks added to these.
    fn take(&mut self, others: Blocks) {
        self.blocks.extend(others.blocks);
        self.tables.extend(others.tables);
    }
}

impl Declared {
    /// Whether the unit's blocks were laid down: never for a unit that
    /// declares nothing, an `eval` in a loop's.
    pub(super) fn laid(&self) -> bool {
        self.laid
    }

    /// Whether the unit's blocks were laid down, and from here that they
    /// are.
    pub(super) fn lay(&mut self) -> bool {
        std::mem::replace(&mut self.laid, true)
    }

    /// What [`of_unit`] laid down, held: a function's, a class's and its
    /// members' to the unit's end, each dynamic definition's while what
    /// holds it stands.
    pub(super) fn hold(&mut self, laid: Laid) {
        self.tables.take(laid.tables);
        self.dynamic = (laid.dynamic.into_iter())
            .map(|(blocks, declarer)| Dynamic {
                blocks,
                declarer,
                ..Dynamic::default()
            })
            .collect();
    }

    /// One more closure over the dynamic definition at `at`.
    pub(super) fn count_closure(&mut self, at: usize) {
        if let Some(dynamic) = self.dynamic.get_mut(at) {
            dynamic.closures += 1;
        }
    }

    /// One closure fewer over the dynamic definition at `at`: the blocks
    /// of each definition nothing holds any longer, `over` where the main
    /// op array has ended.
    pub(super) fn uncount_closure(&mut self, at: usize, over: bool, dead: &mut Vec<Blocks>) {
        if let Some(dynamic) = self.dynamic.get_mut(at) {
            dynamic.closures = dynamic.closures.saturating_sub(1);
        }
        self.settle(over, dead);
    }

    /// The dynamic definition at `at` bound by a `DECLARE_FUNCTION`: the
    /// function table holds it from here.
    pub(super) fn bind(&mut self, at: usize) {
        if let Some(dynamic) = self.dynamic.get_mut(at) {
            dynamic.bound = true;
        }
    }

    /// The main op array ended: the blocks of each definition nothing
    /// holds any longer, as `destroy_op_array` gives back what its
    /// `dynamic_func_defs` held the last count of.
    pub(super) fn main_over(&mut self, dead: &mut Vec<Blocks>) {
        self.settle(true, dead);
    }

    /// Each definition no closure, no binding and no op array standing
    /// holds given back, and with it what it declared where nothing else
    /// holds that, as `destroy_op_array` walks `dynamic_func_defs`. A
    /// closure written in a function the `eval` declares stands with the
    /// function, and one written in a closure with the last count of
    /// that closure's op array:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('$q = 1; $GLOBALS["w"] = function () use ($q) { static $t = 0; return $q; };');
    /// echo m('function h() { return fn () => 1; }'), "\n";
    /// $m = memory_get_usage(); $c = h(); unset($c); echo memory_get_usage() - $m, "\n";
    /// echo m('$GLOBALS["o"] = function () { return function () { return 2; }; };'), "\n";
    /// $m = memory_get_usage(); $i = $o(); unset($o, $i); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 392
    /// 0
    /// 976
    /// -976
    /// ```
    fn settle(&mut self, over: bool, dead: &mut Vec<Blocks>) {
        if self.dynamic.is_empty() {
            return;
        }
        loop {
            let gone: Vec<bool> = self.dynamic.iter().map(|dynamic| dynamic.gone).collect();
            let mut settled = true;
            for dynamic in self.dynamic.iter_mut().filter(|dynamic| !dynamic.gone) {
                let declarer_stands = match dynamic.declarer {
                    Declarer::Main => !over,
                    Declarer::Dynamic(at) => !gone.get(at).copied().unwrap_or(true),
                    Declarer::Table => true,
                };
                if !declarer_stands && dynamic.closures == 0 && !dynamic.bound {
                    dynamic.gone = true;
                    dead.push(std::mem::take(&mut dynamic.blocks));
                    settled = false;
                }
            }
            if settled {
                return;
            }
        }
    }

    /// Every block, the unit gone; nothing for a unit that declared
    /// nothing, the most of them, an `eval` in a loop's.
    pub(super) fn all(self, dead: &mut Vec<Blocks>) {
        if !self.tables.is_empty() {
            dead.push(self.tables);
        }
        dead.extend(
            (self.dynamic.into_iter())
                .map(|dynamic| dynamic.blocks)
                .filter(|blocks| !blocks.is_empty()),
        );
    }
}

/// What `init_op_array`, the compile and `pass_two` leave on the heap for
/// `code`, an op array declared with `args` entries of `arg_info`, its
/// parameters and its return type: a function's with its parameters,
/// variables, live ranges and `try`/`catch` elements, one of more than
/// sixty-four oplines and sixteen variables, and a closure's with its
/// bound and static variables, which the closure copies:
///
/// ```php
/// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
/// m('$q = 1; $GLOBALS["w"] = function () use ($q) { static $t = 0; return $q; };');
/// echo m('function f1($a, $b) { $c = $a + $b; try { $d = 1; } catch (Exception $e) { $d = 2; } foreach ([1, 2] as $k => $v) { $c .= $k . $v; } return $c; }'), "\n";
/// $body = ''; for ($i = 0; $i < 40; $i++) { $body .= "\$v$i = $i + \$v" . max(0, $i - 1) . ";"; }
/// echo m("function f2() { $body return \$v39; }"), "\n";
/// echo m('$x = 1; $GLOBALS["c"] = function () use ($x) { static $s = 0; return $x + ++$s; };'), "\n";
/// ```
/// ```expect regime=jit-off
/// 1152
/// 5904
/// 1600
/// ```
fn op_array(
    req: &mut Request,
    blocks: &mut Blocks,
    code: &OpArray,
    args: usize,
) -> Result<(), Thrown> {
    blocks.blocks.push(Emalloc::new(req, REFCOUNT)?);
    // `get_next_op` grows the oplines fourfold from
    // `INITIAL_OP_ARRAY_SIZE`, and `pass_two` reallocates them to the
    // oplines with the constants after them, aligned to 16
    // (`ZEND_MM_ALIGNED_SIZE_EX`, `Z/Zend/zend_opcode.c:1117`).
    let last = code.oplines.len();
    let mut grown = INITIAL_OPLINES;
    while last > grown {
        grown = grown.saturating_mul(4);
    }
    let oplines = Emalloc::new(req, grown * OPLINE)?;
    let packed = (last * OPLINE).next_multiple_of(16) + code.literals.len() * ZVAL;
    blocks.blocks.push(oplines.resize(req, packed)?);
    // `lookup_cv` grows the names sixteen at a time, and `pass_two`
    // reallocates them to their number where it is not a multiple.
    let vars = code.vars.len();
    if vars > 0 {
        let names = Emalloc::new(req, vars.next_multiple_of(VARS_STEP) * POINTER)?;
        let names = if vars.is_multiple_of(VARS_STEP) {
            names
        } else {
            names.resize(req, vars * POINTER)?
        };
        blocks.blocks.push(names);
    }
    // One block each, which a reallocation one element at a time leaves
    // in the bin of its last size.
    for len in [
        args * ARG_INFO,
        code.live_ranges.len() * LIVE_RANGE,
        code.try_catch.len() * TRY_CATCH,
        code.dynamic_func_defs.len() * POINTER,
    ] {
        if len > 0 {
            blocks.blocks.push(Emalloc::new(req, len)?);
        }
    }
    // `zend_new_array(8)` at the first `static` or bound variable, and
    // one bucket each (`zend_compile_static_var_common`,
    // `zend_compile_closure_binding`).
    if !code.bound_vars.is_empty() {
        let mut statics = HashBytes::new_array(req, CLASS_TABLE)?;
        for _ in &code.bound_vars {
            statics.add(req)?;
        }
        blocks.tables.push(statics);
    }
    Ok(())
}

/// The `arg_info` entries of a function declared as `decl`: one a
/// parameter and one for its return type, which `__toString` has
/// whether or not it is written (`zend_compile_params`'s
/// `fallback_return_type`).
pub(super) fn arg_infos(decl: &FunctionDecl, method: bool) -> usize {
    let returns =
        decl.return_type.is_some() || (method && decl.name.0.eq_ignore_ascii_case(b"__tostring"));
    decl.parameters.len() + usize::from(returns)
}

/// What `zend_initialize_class_data` and the declarations of `class`
/// leave on the heap beside its methods' op arrays: its methods', its
/// properties' and its constants' tables, `zend_hash_init(…, 8, …)`
/// each and a bucket a declaration, and the default values of its
/// properties, instance and static, reallocated one at a time
/// (`zend_declare_typed_property`): a class with properties, constants
/// and methods, a method's lowered name its table's key where it is not
/// its name, and one `Stringable` by its `__toString`:
///
/// ```php
/// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
/// m('class W { function __toString() { return ""; } }');
/// echo m('class P { public $a = 1; private $b; protected static $c = 2; public int $d = 3; const X = 1; const Y = "y"; function __construct($q, $r = 5) {} function Two() {} }'), "\n";
/// echo m('class S { function __toString() { return ""; } }'), "\n";
/// echo m('class M { function f1() {} function f2() {} function f3() {} function f4() {} function f5() {} function f6() {} function f7() {} function f8() {} function f9() {} }'), "\n";
/// ```
/// ```expect regime=jit-off
/// 1576
/// 648
/// 1600
/// ```
fn class(req: &mut Request, blocks: &mut Blocks, class: &ClassDecl) -> Result<(), Thrown> {
    for count in [
        class.methods.len(),
        class.properties.len(),
        class.constants.len(),
    ] {
        let mut table = HashBytes::embedded(CLASS_TABLE);
        for _ in 0..count {
            table.add(req)?;
        }
        blocks.tables.push(table);
    }
    let stored = |instance: bool| {
        class
            .properties
            .iter()
            .filter(|property| property.is_instance == instance && !property.is_virtual)
            .count()
    };
    for count in [stored(true), stored(false)] {
        if count > 0 {
            blocks.blocks.push(Emalloc::new(req, count * ZVAL)?);
        }
    }
    // Each case's `ZEND_AST_CONST_ENUM_INIT`, copied out of the compile's
    // arena as its constant's value (`zend_const_expr_to_zval`'s
    // `zend_ast_copy`): the node and the class's and the case's names,
    // and the backing value's node where it has one.
    for case in class.constants.iter().filter(|constant| constant.is_case) {
        let backed = !matches!(case.value, ConstExpr::Null);
        blocks.blocks.push(Emalloc::new(
            req,
            CASE_AST + usize::from(backed) * AST_ZVAL,
        )?);
    }
    // `zend_do_link_class`'s `interfaces`, a pointer an interface: an
    // enum's `UnitEnum`, and a backed one's `BackedEnum` with the
    // `UnitEnum` it inherits counted again (`zend_do_inherit_interfaces`);
    // a class's own, `Stringable` among them where it declares
    // `__toString`. A class that extends another or uses a trait is
    // linked as #3200 says.
    let interfaces = match (class.kind, class.backing) {
        (ClassKind::Enum, EnumBacking::Pure) => 1,
        (ClassKind::Enum, _) => 3,
        _ if class.parent.is_none() && class.traits.is_empty() => {
            let stringable = class
                .methods
                .iter()
                .any(|method| method.function.name.0.eq_ignore_ascii_case(b"__tostring"))
                && !class
                    .interfaces
                    .iter()
                    .any(|name| name.0.eq_ignore_ascii_case(b"stringable"));
            class.interfaces.len() + usize::from(stringable)
        }
        _ => 0,
    };
    if interfaces > 0 {
        blocks.blocks.push(Emalloc::new(req, interfaces * POINTER)?);
    }
    Ok(())
}

/// What `unit` declares laid down ([`Laid`]); where the memory limit
/// refuses a block, every one laid before it given back.
pub(super) fn of_unit(req: &mut Request, unit: &Unit) -> Result<Laid, Thrown> {
    let mut laid = Laid::default();
    match lay(req, unit, &mut laid) {
        Ok(()) => Ok(laid),
        Err(thrown) => {
            laid.free(req);
            Err(thrown)
        }
    }
}

/// [`of_unit`]'s blocks, laid into `laid` one by one.
fn lay(req: &mut Request, unit: &Unit, laid: &mut Laid) -> Result<(), Thrown> {
    for (decl, code) in &unit.functions {
        op_array(req, &mut laid.tables, code, arg_infos(decl, false))?;
    }
    for decl in &unit.classes {
        class(req, &mut laid.tables, decl)?;
    }
    for method in &unit.methods {
        let decl = unit
            .classes
            .get(method.class as usize)
            .and_then(|class| class.methods.get(method.method as usize));
        let args = decl.map_or(0, |decl| arg_infos(&decl.function, true));
        op_array(req, &mut laid.tables, &method.body, args)?;
    }
    for hook in &unit.hooks {
        let decl = unit
            .classes
            .get(hook.class as usize)
            .and_then(|class| class.properties.get(hook.property as usize))
            .and_then(|property| property.hooks.as_deref())
            .and_then(|hooks| hooks.of(hook.kind));
        let args = decl.map_or(0, |decl| arg_infos(&decl.function, true));
        op_array(req, &mut laid.tables, &hook.body, args)?;
    }
    // Each definition's declarer: the main op array, another dynamic
    // definition, or else what the request's tables hold.
    let mut declarers = vec![Declarer::Table; unit.dynamic.len()];
    let mut declared_in = |defs: &[u32], declarer: Declarer| {
        for &at in defs {
            if let Some(slot) = declarers.get_mut(at as usize) {
                *slot = declarer;
            }
        }
    };
    declared_in(&unit.main.dynamic_func_defs, Declarer::Main);
    for (at, (_, code)) in unit.dynamic.iter().enumerate() {
        declared_in(&code.dynamic_func_defs, Declarer::Dynamic(at));
    }
    for ((decl, code), declarer) in unit.dynamic.iter().zip(declarers) {
        let mut blocks = Blocks::default();
        let made = op_array(req, &mut blocks, code, arg_infos(decl, false));
        laid.dynamic.push((blocks, declarer));
        made?;
    }
    Ok(())
}
