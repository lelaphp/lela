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
//! and its constants', and the default values of its properties; and
//! what `zend_do_link_class` adds to them as the class links, sized from
//! what the link took from its parent, its traits and its interfaces
//! ([`ClassBlocks::link`]).
//!
//! A function or a class is held by the request's tables to the end of
//! the request, and so is what it declared; a dynamic function
//! definition, a closure's or one a `DECLARE_FUNCTION` binds, is held by
//! the main op array that declares it until that op array's execution
//! ends (`destroy_op_array`'s `dynamic_func_defs` loop,
//! `Z/Zend/zend_opcode.c:649`), and past it by each closure over it
//! (`(*op_array->refcount)++` in `zend_create_closure_ex`) and by the
//! function table once a `DECLARE_FUNCTION` bound it.

use php_link::Row;
use php_unit::decl::{ClassDecl, ConstExpr, FunctionDecl, PropertyDecl, TraitAdaptation};
use php_unit::{OpArray, Unit};

use super::Request;
use super::unit_strings::OpArrayStrings;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Emalloc, PhpArray, Value};

/// `sizeof(zend_op)`.
pub(super) const OPLINE: usize = 32;
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
/// `sizeof(zend_ast_ref)`, the counted header `zend_ast_copy` lays
/// before the tree it copies.
const AST_REF: usize = 8;
/// `XtOffsetOf(zend_ast, child)`: a node's kind, attributes and line,
/// before a pointer a child (`zend_ast_size`).
const AST_HEAD: usize = 8;
/// `XtOffsetOf(zend_ast_list, child)`: a list's head and its count,
/// before a pointer an element (`zend_ast_list_size`).
const AST_LIST_HEAD: usize = 16;
/// `sizeof(zend_ast_zval)`, the node of a value written in place, and of
/// a constant's name (`ZEND_AST_CONSTANT`).
const AST_ZVAL: usize = 24;
/// `sizeof(zend_ast_op_array)` and `sizeof(zend_ast_fcc)`, a closure's
/// node and a first-class callable's.
const AST_LEAF: usize = 16;
/// `_ZSTR_HEADER_SIZE`, a `zend_string` before its bytes.
const STRING_HEADER: usize = 24;
/// `sizeof(zend_class_name)`, a name and its lowered form.
const CLASS_NAME: usize = 16;
/// `sizeof(zend_trait_precedence)`, with room for the one name of a
/// trait it excludes.
const PRECEDENCE: usize = 32;
/// `sizeof(zend_trait_alias)`.
const ALIAS: usize = 32;

/// php's blocks for what one request-only unit declares.
#[derive(Default)]
pub(super) struct Declared {
    /// Whether they were laid down, once for the unit.
    laid: bool,
    /// A function's and a method's and hook's, held to the unit's end.
    tables: Blocks,
    /// Each class's, by its place in `Unit::classes`, held to the unit's
    /// end and added to as the class links ([`Request::link_blocks`]);
    /// none while it links.
    classes: Vec<Option<ClassBlocks>>,
    /// Each dynamic function definition's, by its place in
    /// `Unit::dynamic`, and what holds it past the main op array.
    dynamic: Vec<Dynamic>,
}

/// One dynamic definition's blocks and strings and what holds them: the
/// op array that declares it, by its `dynamic_func_defs`, each closure
/// over it and the function table once it is bound. php gives the op
/// array back at the last of them (`destroy_op_array`'s count).
#[derive(Default)]
struct Dynamic {
    blocks: Blocks,
    /// The strings it owns, its name and its constants among them
    /// ([`OpArrayStrings`]).
    strings: OpArrayStrings,
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

/// What `destroy_op_array` gives back of what a unit declared that is
/// over: its blocks and the strings an op array of it owned.
#[derive(Default)]
pub(super) struct Gone {
    pub(super) blocks: Blocks,
    pub(super) strings: OpArrayStrings,
}

/// What a unit declares, laid down: a function's and its methods' and
/// hooks' blocks together, each class's apart, and each dynamic
/// definition's apart with the op array that declares it.
#[derive(Default)]
pub(super) struct Laid {
    tables: Blocks,
    classes: Vec<ClassBlocks>,
    dynamic: Vec<(Blocks, Declarer)>,
}

impl Laid {
    /// Every block given back.
    pub(super) fn free(self, req: &mut Request) {
        self.tables.free(req);
        for class in self.classes {
            class.into_blocks().free(req);
        }
        for (blocks, _) in self.dynamic {
            blocks.free(req);
        }
    }
}

/// One class's blocks: the three tables `zend_initialize_class_data`
/// lays down inside the `zend_class_entry` (`function_table`,
/// `properties_info`, `constants_table`), which its declarations fill
/// and its link extends; its default property tables; `ce->interfaces`,
/// which only its link lays; and what else its compile `emalloc`s, its
/// `trait_names` and the copy of each constant expression its constants
/// and cases were written as.
pub(super) struct ClassBlocks {
    functions: php_heap::EmbeddedArr,
    properties: php_heap::EmbeddedArr,
    constants: php_heap::EmbeddedArr,
    /// `ce->default_properties_table`.
    defaults: Slots,
    /// `ce->default_static_members_table`.
    statics: Slots,
    /// `ce->interfaces`.
    interfaces: Option<Emalloc>,
    emalloced: Vec<Emalloc>,
    /// Each copied constant expression ([`AstCopy`]) by what it is the
    /// value of, until its last holder evaluates it.
    copies: Vec<Copied>,
}

/// What a copied constant expression is the value of, by its place among
/// the class's declarations as its unit wrote them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Of {
    /// A constant's or a case's, given back where it is evaluated
    /// ([`Request::constant_evaluated`]).
    Constant(usize),
    /// An instance property's default in `default_properties_table`,
    /// given back where the last table holding it is evaluated
    /// ([`Request::default_evaluated`]).
    Default(usize),
    /// A static property's default in `default_static_members_table`,
    /// which keeps it: `zend_class_init_statics` copies it into the
    /// request's table, and that copy is what is evaluated.
    Static(usize),
}

/// One constant expression's copy: its blocks, and how many default
/// tables hold it, the declaring class's and each subclass's whose link
/// copied the slot with one more owner (`do_inherit_property`).
struct Copied {
    of: Of,
    holders: usize,
    blocks: Vec<Emalloc>,
}

impl Copied {
    /// The blocks `copy` lays for what `of` names, held by the declaring
    /// class alone.
    fn lay(req: &mut Request, of: Of, copy: &AstCopy) -> Result<Copied, Thrown> {
        let mut laid = Copied {
            of,
            holders: 1,
            blocks: Vec::new(),
        };
        for len in copy.blocks() {
            match Emalloc::new(req, len) {
                Ok(block) => laid.blocks.push(block),
                Err(thrown) => {
                    laid.free(req);
                    return Err(thrown);
                }
            }
        }
        Ok(laid)
    }

    /// Every block given back.
    fn free(self, req: &mut Request) {
        for block in self.blocks {
            block.free(req);
        }
    }
}

impl ClassBlocks {
    /// The `HashTable` inside the `zend_class_entry` that is `table`.
    fn table(&self, table: Table) -> php_heap::Arr {
        match table {
            Table::Methods => self.functions.arr(),
            Table::Constants => self.constants.arr(),
            Table::Properties => self.properties.arr(),
        }
    }

    /// Its blocks as [`Blocks`], to be given back with a unit's.
    fn into_blocks(self) -> Blocks {
        let mut emalloced = self.emalloced;
        emalloced.extend(
            [self.defaults.block, self.statics.block, self.interfaces]
                .into_iter()
                .flatten(),
        );
        emalloced.extend(self.copies.into_iter().flat_map(|copied| copied.blocks));
        Blocks {
            emalloced,
            arrays: Vec::new(),
            embedded: vec![self.functions, self.properties, self.constants],
        }
    }

    /// `zend_do_link_class`'s blocks over these, in its order
    /// (`Z/Zend/zend_inheritance.c:3618`): an enum's `cases`, `from` and
    /// `tryFrom` (`zend_enum_register_funcs`) and the traits' members a
    /// row each, the traits' properties a slot each
    /// (`zend_declare_typed_property`'s `perealloc`); then the parent's,
    /// each default table laid anew at the two counts together and each
    /// table `zend_hash_extend`ed by the parent's before the rows the
    /// class did not declare (`zend_do_inheritance_ex`); then the traits'
    /// abstract methods and the interfaces' members a row each, and
    /// `ce->interfaces` at each size `zend_do_implement_interfaces` and
    /// `zend_do_inherit_interfaces` give it. A class that extends
    /// another, one that uses a trait, one that extends a native class
    /// and implements an interface that extends another, and an enum:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W { function __toString() { return ""; } }');
    /// echo m('class Pa { public $a; function f() {} } class Ch extends Pa {}'), "\n";
    /// echo m('trait T7 { public $tp = 1; function t() {} } class Q7 { use T7; }'), "\n";
    /// echo m('interface I1 { const K = 1; function i(); } interface I2 extends I1 {} class C5 extends ArrayObject implements I2 { function i() {} }'), "\n";
    /// echo m('enum E2: int { case A = 1; }'), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 1600
    /// 1712
    /// 3368
    /// 1288
    /// ```
    fn link(&mut self, req: &mut Request, linking: &Linking) -> Result<(), Thrown> {
        for table in Table::ALL {
            add_rows(req, self.table(table), linking.first.of(table))?;
        }
        self.defaults.grow(req, linking.trait_slots.instance)?;
        self.statics.grow(req, linking.trait_slots.statics)?;
        if let Some(parent) = &linking.parent {
            self.defaults.inherit(req, parent.slots.instance)?;
            self.statics.inherit(req, parent.slots.statics)?;
            // As `zend_do_inheritance_ex` walks them, each only where the
            // parent's table has rows.
            for table in [Table::Properties, Table::Constants, Table::Methods] {
                let size = parent.tables.of(table);
                if size == 0 {
                    continue;
                }
                let rows = self.table(table);
                let room = u32::try_from(rows.num_used() as usize + size).unwrap_or(u32::MAX);
                let extended = req.heap.array_extend(rows, room);
                req.fatal_of(extended)?;
                add_rows(req, rows, parent.rows.of(table))?;
            }
        }
        for table in Table::ALL {
            add_rows(req, self.table(table), linking.last.of(table))?;
        }
        for &size in &linking.interfaces {
            let laid = match self.interfaces.take() {
                None => Emalloc::new(req, size * POINTER)?,
                Some(block) => block.resize(req, size * POINTER)?,
            };
            self.interfaces = Some(laid);
        }
        Ok(())
    }
}

/// A class's default property table, `ce->default_properties_table` or
/// `ce->default_static_members_table`, and its number of slots.
#[derive(Default)]
struct Slots {
    block: Option<Emalloc>,
    count: usize,
}

impl Slots {
    /// `added` slots more, one `perealloc` a property as
    /// `zend_declare_typed_property` makes them, each leaving the block
    /// in the bin of its size.
    fn grow(&mut self, req: &mut Request, added: usize) -> Result<(), Thrown> {
        for _ in 0..added {
            self.count += 1;
            let len = self.count * ZVAL;
            let block = match self.block.take() {
                None => Emalloc::new(req, len)?,
                Some(block) => block.resize(req, len)?,
            };
            self.block = Some(block);
        }
        Ok(())
    }

    /// `zend_do_inheritance_ex`'s: where the parent has `inherited`
    /// slots, a table of the two counts together laid down and the one
    /// standing given back.
    fn inherit(&mut self, req: &mut Request, inherited: usize) -> Result<(), Thrown> {
        if inherited == 0 {
            return Ok(());
        }
        self.count += inherited;
        let table = Emalloc::new(req, self.count * ZVAL)?;
        if let Some(old) = self.block.replace(table) {
            old.free(req);
        }
        Ok(())
    }
}

/// A count for each of a class's three tables: its methods', its
/// constants' and its properties'.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Members {
    methods: usize,
    constants: usize,
    properties: usize,
}

impl Members {
    /// The count for `table`.
    fn of(self, table: Table) -> usize {
        match table {
            Table::Methods => self.methods,
            Table::Constants => self.constants,
            Table::Properties => self.properties,
        }
    }
}

/// `default_properties_count` and `default_static_members_count`: the
/// slots of a class's two default tables.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SlotCounts {
    instance: usize,
    statics: usize,
}

impl SlotCounts {
    /// The slots `properties` declare: one each but a virtual one's.
    fn of(properties: &[PropertyDecl]) -> SlotCounts {
        let stored = |instance: bool| {
            (properties.iter())
                .filter(|property| property.is_instance == instance && !property.is_virtual)
                .count()
        };
        SlotCounts {
            instance: stored(true),
            statics: stored(false),
        }
    }
}

/// What a class's parent gives its link: the sizes of the parent's
/// tables, which `zend_hash_extend` makes room for, the rows of them the
/// class did not declare, and the parent's slots.
#[derive(Debug, PartialEq, Eq)]
struct Inherited {
    tables: Members,
    rows: Members,
    slots: SlotCounts,
}

/// What `zend_do_link_class` adds to one class's blocks
/// ([`ClassBlocks::link`]), counted off its link ([`Request::linking`]).
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Linking {
    /// Rows before the parent's: an enum's three methods and the traits'
    /// members.
    first: Members,
    /// The slots the traits' properties take.
    trait_slots: SlotCounts,
    parent: Option<Inherited>,
    /// Rows after the parent's: the traits' abstract methods and the
    /// interfaces' members.
    last: Members,
    /// `ce->interfaces`' sizes, a pointer an interface: laid at the
    /// first, reallocated to each after; empty for a class that
    /// implements none.
    interfaces: Vec<usize>,
}

/// `emalloc`'d blocks and hash tables, given back together. A table is
/// the heap's own ([`php_heap::Arr`]), sized, grown and freed as
/// `zend_hash.c` sizes, grows and frees it; its rows are keyed by their
/// declaration's place, the names being the unit's strings
/// ([`super::unit_strings`]) and the values the engine's tables', so a
/// row holds nothing beyond its bucket.
#[derive(Default)]
pub(super) struct Blocks {
    /// Blocks `emalloc` lays down alone.
    emalloced: Vec<Emalloc>,
    /// Tables `zend_new_array` lays down, the header `emalloc`'d.
    arrays: Vec<php_heap::Arr>,
    /// Tables `zend_hash_init` lays down inside the `zend_class_entry`,
    /// which the compile's arena holds, so only their data is the heap's.
    embedded: Vec<php_heap::EmbeddedArr>,
}

impl Blocks {
    /// Whether it holds no block.
    fn is_empty(&self) -> bool {
        self.emalloced.is_empty() && self.arrays.is_empty() && self.embedded.is_empty()
    }

    /// Every block given back: each table's rows, its data and, for one
    /// `zend_new_array` laid down, its header, as `zend_array_destroy`
    /// and `zend_hash_destroy` give them back.
    pub(super) fn free(self, req: &mut Request) {
        for block in self.emalloced {
            block.free(req);
        }
        for table in self.arrays {
            req.release(Value::array(PhpArray::of(table)));
        }
        for table in self.embedded {
            let rows = table.arr();
            for at in 0..rows.num_used() {
                req.heap.array_take_at(rows, at);
            }
            table.free_data(&mut req.heap);
        }
    }

    /// `others`' blocks added to these.
    fn take(&mut self, others: Blocks) {
        self.emalloced.extend(others.emalloced);
        self.arrays.extend(others.arrays);
        self.embedded.extend(others.embedded);
    }
}

/// `count` rows put in `table`, a hash table php keys by name, one
/// bucket each and the table grown as `ZEND_HASH_IF_FULL_DO_RESIZE`
/// grows it. An uninitialised table is laid down as a hash at the first,
/// as `zend_hash_real_init_mixed` lays a table a string key is first
/// written to, never as the packed list an integer key would lay.
pub(super) fn add_rows(
    req: &mut Request,
    table: php_heap::Arr,
    count: usize,
) -> Result<(), Thrown> {
    if count == 0 {
        return Ok(());
    }
    let laid = req.heap.array_extend(table, table.table_size());
    req.fatal_of(laid)?;
    let from = table.num_used() as usize;
    for at in from..from + count {
        let key = php_heap::Key::Long(i64::try_from(at).unwrap_or(i64::MAX));
        let added = req.heap.array_add_new(table, key, php_heap::Zval::null());
        req.fatal_of(added)?;
    }
    Ok(())
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
        self.classes = laid.classes.into_iter().map(Some).collect();
        for (at, (blocks, declarer)) in laid.dynamic.into_iter().enumerate() {
            if let Some(dynamic) = self.definition_laid(at) {
                dynamic.blocks.take(blocks);
                dynamic.declarer = declarer;
            }
        }
    }

    /// The dynamic definition at `at`, laid down empty where it is not
    /// yet, as the unit's strings are taken in before its blocks.
    fn definition_laid(&mut self, at: usize) -> Option<&mut Dynamic> {
        if self.dynamic.len() <= at {
            self.dynamic.resize_with(at + 1, Dynamic::default);
        }
        self.dynamic.get_mut(at)
    }

    /// The strings of each dynamic definition that stands.
    pub(super) fn strings(&self) -> impl Iterator<Item = &OpArrayStrings> {
        (self.dynamic.iter())
            .filter(|dynamic| !dynamic.gone)
            .map(|dynamic| &dynamic.strings)
    }

    /// The strings the dynamic definition at `at` owns, which go with its
    /// op array.
    pub(super) fn definition_strings(&mut self, at: usize) -> Option<&mut OpArrayStrings> {
        self.definition_laid(at).map(|dynamic| &mut dynamic.strings)
    }

    /// The place of the class at `at` of the unit's classes' blocks.
    pub(super) fn class_slot(&mut self, at: usize) -> Option<&mut Option<ClassBlocks>> {
        self.classes.get_mut(at)
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
    pub(super) fn uncount_closure(&mut self, at: usize, over: bool, dead: &mut Vec<Gone>) {
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
    pub(super) fn main_over(&mut self, dead: &mut Vec<Gone>) {
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
    fn settle(&mut self, over: bool, dead: &mut Vec<Gone>) {
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
                    dead.push(Gone {
                        blocks: std::mem::take(&mut dynamic.blocks),
                        strings: std::mem::take(&mut dynamic.strings),
                    });
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
    pub(super) fn all(self, dead: &mut Vec<Gone>) {
        let mut blocks = self.tables;
        for class in self.classes.into_iter().flatten() {
            blocks.take(class.into_blocks());
        }
        if !blocks.is_empty() {
            dead.push(Gone {
                blocks,
                strings: OpArrayStrings::default(),
            });
        }
        dead.extend(
            (self.dynamic.into_iter())
                .filter(|dynamic| !dynamic.gone)
                .map(|dynamic| Gone {
                    blocks: dynamic.blocks,
                    strings: dynamic.strings,
                }),
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
    declared: Option<(&FunctionDecl, bool)>,
) -> Result<(), Thrown> {
    let args = declared.map_or(0, |(decl, method)| arg_infos(decl, method));
    blocks.emalloced.push(Emalloc::new(req, REFCOUNT)?);
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
    blocks.emalloced.push(oplines.resize(req, packed)?);
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
        blocks.emalloced.push(names);
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
            blocks.emalloced.push(Emalloc::new(req, len)?);
        }
    }
    // Each parameter's default the compile left a tree, copied as the
    // `RECV_INIT`'s literal (`zend_compile_params`' `zend_const_expr_to_zval`),
    // which the op array keeps.
    let defaults = declared.map_or(&[][..], |(decl, _)| &decl.parameters);
    for default in defaults
        .iter()
        .filter_map(|parameter| parameter.default.expr())
    {
        if !super::linking::is_literal(default) {
            for len in AstCopy::of(default).blocks() {
                blocks.emalloced.push(Emalloc::new(req, len)?);
            }
        }
    }
    // `zend_new_array(8)` at the first `static` or bound variable, and
    // one bucket each (`zend_compile_static_var_common`,
    // `zend_compile_closure_binding`).
    if !code.bound_vars.is_empty() {
        let laid = req.heap.array_sized(CLASS_TABLE);
        let statics = req.fatal_of(laid)?;
        blocks.arrays.push(statics);
        add_rows(req, statics, code.bound_vars.len())?;
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

/// What `zend_ast_copy` lays for one constant expression, as php's
/// compile wrote its tree: one block of `zend_ast_tree_size` and the
/// `zend_ast_ref` before it, and the strings its nodes hold
/// (`ZVAL_COPY`), each a `zend_string` the scanner or the compile made
/// for the tree, which stand as long as the copy does. A string of one
/// byte is php's `ZSTR_CHAR` and the empty one `ZSTR_EMPTY`, interned
/// and so no block; an enum case's own name and its class's are the
/// constant's key and the class's name, which the node shares.
///
/// The six declarations and reads #3241 measured: an enum whose constant names a case, the enum
/// without it, a class whose constant names one of a class not yet
/// declared, and the first read of that constant, which gives the copy
/// back; then a constant naming one by two longer names, an array still
/// to evaluate, and a backed case whose value is an expression, each
/// against its like written out; and a backed enum's first `from()`,
/// which evaluates its constants with its cases and gives back their
/// copies, so the constant's first read costs nothing:
///
/// ```php
/// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
/// m('class W {}');
/// echo m('enum S: string { case A = "abcdefgh"; case B = "x"; const K = self::B; }'), "\n";
/// echo m('enum S2: string { case A = "abcdefgh"; case B = "x"; }'), "\n";
/// echo m('class Q { const K = R::B; }'), "\n";
/// echo m('class Q2 { }'), "\n";
/// echo m('class R { const B = 1; }'), "\n";
/// echo m('$GLOBALS["q"] = Q::K;'), "\n";
/// $base = m('class C0 { const K = 1; }');
/// echo m('class C1 { const K = Rlonger::Blonger; }') - $base, "\n";
/// echo m('class C2 { const K = [1, "abcdefghij" . X1, Xlongname]; }') - $base, "\n";
/// echo m('enum E4: int { case A = S::B === S::A ? 1 : 2; }') - m('enum E3: int { case A = 1; }'), "\n";
/// m('enum S14: string { case A = "abcdefgh"; case B = "x"; const K = self::B; }');
/// echo m('$GLOBALS["a"] = S14::from("x");'), ' ', m('$GLOBALS["b"] = S14::K;'), "\n";
/// ```
/// ```expect regime=jit-off exit=0
/// 1864
/// 1440
/// 496
/// 160
/// 416
/// 432
/// 144
/// 368
/// 272
/// 200 0
/// ```
struct AstCopy {
    /// The tree's block, the header before it among its bytes.
    tree: usize,
    /// Each string a node holds, by its length.
    strings: Vec<usize>,
}

impl AstCopy {
    /// The `zend_ast_ref` alone, before the tree is laid after it.
    const fn header() -> AstCopy {
        AstCopy {
            tree: AST_REF,
            strings: Vec::new(),
        }
    }

    /// The copy of a constant's expression.
    fn of(expr: &ConstExpr) -> AstCopy {
        let mut copy = AstCopy::header();
        copy.expr(expr);
        copy
    }

    /// The copy of a case's `ZEND_AST_CONST_ENUM_INIT`
    /// (`zend_compile_enum_case`), its backing value the third child,
    /// `NULL` for a pure case.
    fn of_case(value: &ConstExpr) -> AstCopy {
        let mut copy = AstCopy::header();
        copy.enum_init();
        if !matches!(value, ConstExpr::Null) {
            copy.expr(value);
        }
        copy
    }

    /// A `ZEND_AST_CONST_ENUM_INIT` over its class's name and the case's,
    /// which the class's name and the constant's key share, before its
    /// value.
    fn enum_init(&mut self) {
        self.node(3);
        self.tree += 2 * AST_ZVAL;
    }

    /// The blocks, the tree first, by their lengths.
    fn blocks(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.tree).chain(
            self.strings
                .iter()
                .map(|len| (STRING_HEADER + len + 1).next_multiple_of(8)),
        )
    }

    /// A node of `children` (`zend_ast_size`).
    fn node(&mut self, children: usize) {
        self.tree += AST_HEAD + children * POINTER;
    }

    /// A `ZEND_AST_ZVAL` of a string the tree holds.
    fn string(&mut self, bytes: &[u8]) {
        self.tree += AST_ZVAL;
        if bytes.len() > 1 {
            self.strings.push(bytes.len());
        }
    }

    /// `zend_ast_tree_size` over `expr`'s node in php's tree, and the
    /// strings it holds.
    fn expr(&mut self, expr: &ConstExpr) {
        match expr {
            ConstExpr::Null
            | ConstExpr::Bool(_)
            | ConstExpr::Long { .. }
            | ConstExpr::Double(_)
            | ConstExpr::ImmutableArray(_) => self.tree += AST_ZVAL,
            ConstExpr::Str(bytes) => self.string(bytes),
            ConstExpr::Constant { name, .. } => self.string(&name.resolved().0),
            ConstExpr::ClassConstant { class, name, .. } => {
                self.node(2);
                self.string(&class.0);
                self.string(&name.0);
            }
            ConstExpr::DynamicClassConstant { class, name, .. } => {
                self.node(2);
                self.string(&class.0);
                self.expr(name);
            }
            ConstExpr::EnumCase { .. } => self.enum_init(),
            ConstExpr::ClassName { class, .. } => {
                self.node(1);
                self.string(&class.0);
            }
            ConstExpr::Unary { operand, .. }
            | ConstExpr::Unpack(operand)
            | ConstExpr::Cast { operand, .. } => {
                self.node(1);
                self.expr(operand);
            }
            ConstExpr::Binary { lhs, rhs, .. } => {
                self.node(2);
                self.expr(lhs);
                self.expr(rhs);
            }
            ConstExpr::Dim { container, dim, .. } => {
                self.node(2);
                self.expr(container);
                self.expr(dim);
            }
            ConstExpr::Prop { object, name, .. } => {
                self.node(2);
                self.expr(object);
                self.expr(name);
            }
            ConstExpr::Conditional {
                cond,
                then,
                otherwise,
            } => {
                self.node(3);
                self.expr(cond);
                if let Some(then) = then {
                    self.expr(then);
                }
                self.expr(otherwise);
            }
            // `ZEND_AST_ARRAY`, a list of `ZEND_AST_ARRAY_ELEM`s, value
            // then key, and of the `ZEND_AST_UNPACK` a spread stands as.
            ConstExpr::Array { elements, .. } => {
                self.tree += AST_LIST_HEAD + elements.len() * POINTER;
                for (key, value) in elements {
                    if !matches!(value, ConstExpr::Unpack(_)) {
                        self.node(2);
                    }
                    self.expr(value);
                    if let Some(key) = key {
                        self.expr(key);
                    }
                }
            }
            // `ZEND_AST_NEW` over the class and the `ZEND_AST_ARG_LIST`,
            // a named argument a `ZEND_AST_NAMED_ARG` over its name and
            // its value.
            ConstExpr::New {
                class, arguments, ..
            } => {
                self.node(2);
                self.string(&class.0);
                self.tree += AST_LIST_HEAD + arguments.len() * POINTER;
                for (name, value) in arguments {
                    if let Some(name) = name {
                        self.node(2);
                        self.string(&name.0);
                    }
                    self.expr(value);
                }
            }
            ConstExpr::Closure { .. } => self.tree += AST_LEAF,
            // `ZEND_AST_CALL` and `ZEND_AST_STATIC_CALL` over the
            // `ZEND_AST_CALLABLE_CONVERT` that stands for `...`.
            ConstExpr::FunctionCallable { name, .. } => {
                self.node(2);
                self.string(&name.0);
                self.tree += AST_LEAF;
            }
            ConstExpr::MethodCallable { class, method, .. } => {
                self.node(3);
                self.string(&class.0);
                self.string(&method.0);
                self.tree += AST_LEAF;
            }
            ConstExpr::ScopeClass => self.node(0),
        }
    }
}

/// What `zend_initialize_class_data` and the declarations of `class`
/// leave on the heap beside its methods' op arrays: its methods', its
/// properties' and its constants' tables, `zend_hash_init(…, 8, …)`
/// each and a bucket a declaration, and the default values of its
/// properties, instance and static, reallocated one at a time
/// (`zend_declare_typed_property`), and its link's blocks
/// ([`ClassBlocks::link`]): a class with properties, constants and
/// methods, a method's lowered name its table's key where it is not its
/// name, and one `Stringable` by its `__toString`:
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
fn class(req: &mut Request, class: &ClassDecl) -> Result<ClassBlocks, Thrown> {
    let mut blocks = ClassBlocks {
        functions: php_heap::EmbeddedArr::new(CLASS_TABLE),
        properties: php_heap::EmbeddedArr::new(CLASS_TABLE),
        constants: php_heap::EmbeddedArr::new(CLASS_TABLE),
        defaults: Slots::default(),
        statics: Slots::default(),
        interfaces: None,
        emalloced: Vec::new(),
        copies: Vec::new(),
    };
    match lay_class(req, &mut blocks, class) {
        Ok(()) => Ok(blocks),
        Err(thrown) => {
            blocks.into_blocks().free(req);
            Err(thrown)
        }
    }
}

/// [`class`]'s blocks, laid into `blocks`: a class's adaptations of
/// the traits it uses among them, which its compile keeps beside their
/// names:
///
/// ```php
/// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
/// m('class W { function __toString() { return ""; } } trait TA { function a() {} } trait TB { function a() {} }');
/// echo m('class X1 { use TA, TB { TA::a insteadof TB; TB::a as aa; TA::a as protected; } }'), "\n";
/// ```
/// ```expect regime=jit-off
/// 936
/// ```
fn lay_class(req: &mut Request, blocks: &mut ClassBlocks, class: &ClassDecl) -> Result<(), Thrown> {
    // An enum's `cases`, `from` and `tryFrom` are its link's.
    let written = Members {
        methods: php_unit::written_methods(class).len(),
        constants: class.constants.len(),
        properties: class.properties.len(),
    };
    for table in Table::ALL {
        add_rows(req, blocks.table(table), written.of(table))?;
    }
    // The default values, reallocated one at a time
    // (`zend_declare_typed_property`), which leaves the bin of the last.
    let slots = SlotCounts::of(&class.properties);
    for (table, count) in [
        (&mut blocks.defaults, slots.instance),
        (&mut blocks.statics, slots.statics),
    ] {
        if count > 0 {
            table.block = Some(Emalloc::new(req, count * ZVAL)?);
            table.count = count;
        }
    }
    // Each constant expression, copied out of the compile's arena as its
    // constant's or its property's value (`zend_const_expr_to_zval`'s
    // `zend_ast_copy`, from `zend_compile_class_const_decl` and
    // `zend_compile_prop_decl`): a case's `ZEND_AST_CONST_ENUM_INIT`, and
    // a constant's or a default's tree where the compile left one, which
    // a value written out is not.
    for (at, constant) in class.constants.iter().enumerate() {
        let copy = if constant.is_case {
            AstCopy::of_case(&constant.value)
        } else if super::linking::is_literal(&constant.value) {
            continue;
        } else {
            AstCopy::of(&constant.value)
        };
        blocks
            .copies
            .push(Copied::lay(req, Of::Constant(at), &copy)?);
    }
    for (at, property) in class.properties.iter().enumerate() {
        let Some(default) = (property.default.as_ref()).filter(|d| !super::linking::is_literal(d))
        else {
            continue;
        };
        let of = if property.is_instance {
            Of::Default(at)
        } else {
            Of::Static(at)
        };
        blocks
            .copies
            .push(Copied::lay(req, of, &AstCopy::of(default))?);
    }
    // `zend_compile_use_trait`'s `trait_names`, a `zend_class_name` a
    // trait, reallocated a `use` at a time; the names themselves are the
    // unit's strings ([`Request::hold_unit_names`]).
    if !class.traits.is_empty() {
        blocks
            .emalloced
            .push(Emalloc::new(req, class.traits.len() * CLASS_NAME)?);
    }
    // Each adaptation, a `zend_trait_precedence` with a name a trait it
    // excludes (`zend_compile_trait_precedence`) or a `zend_trait_alias`
    // (`zend_compile_trait_alias`), and the two lists `zend_add_to_list`
    // keeps them in, reallocated an adaptation at a time to one more
    // than it holds, the end's `NULL`.
    let (mut precedences, mut aliases) = (0, 0);
    for adaptation in &class.adaptations {
        let len = match adaptation {
            TraitAdaptation::Insteadof { excluded, .. } => {
                precedences += 1;
                PRECEDENCE + excluded.len().saturating_sub(1) * POINTER
            }
            TraitAdaptation::As { .. } => {
                aliases += 1;
                ALIAS
            }
        };
        blocks.emalloced.push(Emalloc::new(req, len)?);
    }
    for count in [precedences, aliases] {
        if count > 0 {
            blocks
                .emalloced
                .push(Emalloc::new(req, (count + 1) * POINTER)?);
        }
    }
    Ok(())
}

/// What `unit` declares laid down ([`Laid`]); where the memory limit
/// refuses a block, every one laid before it given back.
///
/// Only a unit the request compiled for itself is laid. A file is
/// compiled as opcache compiles it in every regime (design §6.3), so it
/// costs what opcache's lookup allocates, where `php -n` compiles it
/// into the request's heap and keeps its op arrays' and declarations'
/// blocks, and the strings it interns, to the request's end: a
/// by-design divergence (#3224, landed by #3233). A request's primary
/// file is one too (design §5.13), laid nothing here: two programs
/// of one length, one of which reads an element, apart by the delayed
/// oplines' stack alone here, `512`, and by the main op array's block too
/// under php's:
///
/// ```php
/// $f = sys_get_temp_dir() . '/argv' . getmypid() . '.php';
/// $usage = function ($code) use ($f) {
///     file_put_contents($f, "<?php $code echo memory_get_usage(), \"\\n\";");
///     return (int) shell_exec(escapeshellarg(PHP_BINARY) . ' -n ' . escapeshellarg($f));
/// };
/// echo $usage('$x = $argv[0];') - $usage('$x = $argv;   '), "\n";
/// unlink($f);
/// ```
/// ```expect process exit=0
/// 576
/// ```
///
/// An include that declares a function and a class with a property, and
/// one whose main op array assigns an 18-byte constant, which the engine
/// reads `96` and `256` (each file is written where its name's length
/// stands still):
///
/// ```php
/// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; $t = "$f." . getmypid(); file_put_contents($t, $src); touch($t, time() - 10); rename($t, $f); return $f; };
/// $f = $put('w3233-decl.inc', '<?php function q1($a) { return "abcdefghijklmnop" . $a; } class K1 { public $p = 1; function m() {} }');
/// $m = memory_get_usage(); include $f; echo memory_get_usage() - $m, "\n";
/// $g = $put('w3233-literal.inc', '<?php $y = "qrstuvwxyzabcdefgh"; return 1;');
/// $m = memory_get_usage(); include $g; echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect exit=0
/// 1184
/// 304
/// ```
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
        op_array(req, &mut laid.tables, code, Some((decl, false)))?;
    }
    for decl in &unit.classes {
        let blocks = class(req, decl)?;
        laid.classes.push(blocks);
    }
    for method in &unit.methods {
        let decl = unit
            .classes
            .get(method.class as usize)
            .and_then(|class| class.methods.get(method.method as usize));
        let declared = decl.map(|decl| (&decl.function, true));
        op_array(req, &mut laid.tables, &method.body, declared)?;
    }
    for hook in &unit.hooks {
        let decl = unit
            .classes
            .get(hook.class as usize)
            .and_then(|class| class.properties.get(hook.property as usize))
            .and_then(|property| property.hooks.as_deref())
            .and_then(|hooks| hooks.of(hook.kind));
        let declared = decl.map(|decl| (&decl.function, true));
        op_array(req, &mut laid.tables, &hook.body, declared)?;
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
        let made = op_array(req, &mut blocks, code, Some((decl, false)));
        laid.dynamic.push((blocks, declarer));
        made?;
    }
    Ok(())
}

/// One table of a linked record.
type RecordRows<C> = php_link::Rows<Row<ClassId>, C>;

/// One of a class's three tables, which names a row of it.
#[derive(Clone, Copy)]
enum Table {
    Methods,
    Constants,
    Properties,
}

impl Table {
    /// The three, in the order `zend_do_link_class` binds a trait's
    /// members.
    const ALL: [Table; 3] = [Table::Methods, Table::Constants, Table::Properties];
}

impl Request {
    /// `zend_do_link_class`'s blocks for `class` as it binds
    /// ([`ClassBlocks::link`]), where a unit the request compiled for
    /// itself declared it; nothing for any other class. The `Err` is the
    /// memory limit's fatal.
    pub(super) fn link_blocks(&mut self, class: ClassId) -> Result<(), Thrown> {
        if self.unit_strings.is_empty() {
            return Ok(());
        }
        let Some((unit, at)) = self
            .declared_row(class)
            .map(|row| (php_unit::PublishedUnit::as_ptr(&row.unit), row.at as usize))
        else {
            return Ok(());
        };
        let Some(mut blocks) = self.unit_strings.class_blocks(unit, at) else {
            return Ok(());
        };
        let linked = match self.linking(class) {
            Some(linking) => blocks.link(self, &linking),
            None => Ok(()),
        };
        match self.unit_strings.class_slot(unit, at) {
            Some(slot) => *slot = Some(blocks),
            None => blocks.into_blocks().free(self),
        }
        linked?;
        self.hold_inherited_copies(class);
        self.hold_trait_property_names(class)
    }

    /// The constant or case `name` of `class` evaluated, which a unit the
    /// request compiled for itself declared: its constant expression's
    /// copy given back ([`AstCopy`]), as `zval_update_constant_with_ctx`
    /// destroys the request's copy once the value, or the case's object,
    /// stands in its place. A cached file's is
    /// opcache's and stays. So the first read of an `eval`'d enum's case
    /// costs its object less its expression, and `cases()` and `from()`
    /// give back each one they evaluate; a case read again costs what a
    /// constant's read does:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('enum E4: string { case A = "a"; case B = "b"; } enum E6 { case A; case B; case C; }');
    /// m('$q = 1;');
    /// echo m('$GLOBALS["x"] = E6::C;'), "\n";
    /// echo m('$GLOBALS["x2"] = E6::C;'), "\n";
    /// echo m('$GLOBALS["y"] = E4::from("b");'), "\n";
    /// echo m('$GLOBALS["c"] = E6::cases();'), "\n";
    /// enum F { case A; case B; }
    /// echo m('$GLOBALS["f"] = F::B;'), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 472
    /// 32
    /// 312
    /// 136
    /// 56
    /// ```
    pub(super) fn constant_evaluated(&mut self, class: ClassId, name: &[u8]) {
        if self.unit_strings.is_empty() {
            return;
        }
        let Some((unit, at, constant)) = self.declared_row(class).and_then(|row| {
            let written = row.unit.classes.get(row.at as usize)?;
            let constant = (written.constants.iter()).position(|c| *c.name.0 == *name)?;
            Some((
                php_unit::PublishedUnit::as_ptr(&row.unit),
                row.at as usize,
                constant,
            ))
        }) else {
            return;
        };
        self.copy_released(unit, at, Of::Constant(constant));
    }

    /// One holder fewer of the copy of what `of` names in the class at
    /// `at` of `unit`, and its blocks given back with the last.
    fn copy_released(&mut self, unit: *const Unit, at: usize, of: Of) {
        let Some(blocks) = (self.unit_strings.class_slot(unit, at)).and_then(Option::as_mut) else {
            return;
        };
        let Some(place) = blocks.copies.iter().position(|copied| copied.of == of) else {
            return;
        };
        let last = (blocks.copies.get_mut(place)).is_some_and(|copied| {
            copied.holders = copied.holders.saturating_sub(1);
            copied.holders == 0
        });
        if last {
            blocks.copies.swap_remove(place).free(self);
        }
    }

    /// The default `info` names evaluated in a class's
    /// `default_properties_table`, which a unit the request compiled for
    /// itself declared, by that class's own update: one holder fewer of
    /// its expression's copy ([`AstCopy`]), which
    /// `zend_update_class_constants` destroys in place once the value
    /// stands, given back by the last holder. A subclass's table holds it
    /// past both classes' updates, as php's does. So the first object of
    /// an `eval`'d class gives the copy back, and of a class with a
    /// subclass gives nothing back; a static's copy and a parameter's
    /// stand with the class and the op array:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W { function __toString() { return ""; } }');
    /// m('$q0 = 1;');
    /// foreach ([
    ///     'class Q9 { public $p = R::B; }',
    ///     'class Q10 { public static $p = R::B; }',
    ///     'class Q11 { function f($a = R::B) {} }',
    ///     'class R { const B = 1; }',
    ///     '$GLOBALS["l"] = new Q9;',
    ///     'class D1 { public $p = R::B; } class D2 extends D1 {}',
    ///     '$GLOBALS["d1"] = new D1;',
    ///     '$GLOBALS["d2"] = new D2;',
    /// ] as $c) echo m($c), "  ", $c, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 576  class Q9 { public $p = R::B; }
    /// 576  class Q10 { public static $p = R::B; }
    /// 720  class Q11 { function f($a = R::B) {} }
    /// 416  class R { const B = 1; }
    /// 488  $GLOBALS["l"] = new Q9;
    /// 896  class D1 { public $p = R::B; } class D2 extends D1 {}
    /// 88  $GLOBALS["d1"] = new D1;
    /// 88  $GLOBALS["d2"] = new D2;
    /// ```
    pub(super) fn default_evaluated(&mut self, info: super::PropertyInfo) {
        if self.unit_strings.is_empty() {
            return;
        }
        let Some((unit, at)) = self.declared_row(info.declaring).and_then(|row| {
            let written = row.unit.classes.get(row.at as usize)?;
            (written.properties.get(info.at as usize)).filter(|p| p.is_instance)?;
            Some((php_unit::PublishedUnit::as_ptr(&row.unit), row.at as usize))
        }) else {
            return;
        };
        self.copy_released(unit, at, Of::Default(info.at as usize));
    }

    /// One more holder of each instance default's copy `class`'s link
    /// took from a class above it, as `do_inherit_property` copies the
    /// parent's `default_properties_table` with one more owner of each
    /// value.
    fn hold_inherited_copies(&mut self, class: ClassId) {
        let inherited: Vec<(*const Unit, usize, usize)> = (self.placed_properties(class).iter())
            .filter(|info| info.declaring != class)
            .filter_map(|info| {
                let row = self.declared_row(info.declaring)?;
                Some((
                    php_unit::PublishedUnit::as_ptr(&row.unit),
                    row.at as usize,
                    info.at as usize,
                ))
            })
            .collect();
        for (unit, at, property) in inherited {
            let Some(blocks) = (self.unit_strings.class_slot(unit, at)).and_then(Option::as_mut)
            else {
                continue;
            };
            if let Some(copied) = (blocks.copies.iter_mut()).find(|c| c.of == Of::Default(property))
            {
                copied.holders += 1;
            }
        }
    }

    /// The names `zend_declare_typed_property` makes for each property a
    /// trait gave `class` that is not public, mangled under the class
    /// (`zend_mangle_property_name`), as strings of the class's unit; a
    /// public one keeps the trait's:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W { function __toString() { return ""; } } trait TP { private $pp = 1; protected $qq = 2; } trait TQ { public $rr = 3; }');
    /// echo m('class CP2 { use TP; }') - m('class CP3 { use TQ; }'), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 80
    /// ```
    fn hold_trait_property_names(&mut self, class: ClassId) -> Result<(), Thrown> {
        let Some(row) = self.declared_row(class) else {
            return Ok(());
        };
        let Some((decl, written)) = row.decl().zip(row.unit.classes.get(row.at as usize)) else {
            return Ok(());
        };
        let unit = php_unit::PublishedUnit::as_ptr(&row.unit);
        let mangled: Vec<Vec<u8>> = (decl.properties.get(written.properties.len()..))
            .unwrap_or_default()
            .iter()
            .filter_map(|property| {
                let prefix = php_compile::mangling(property.visibility, &decl.name.0)?;
                Some([prefix.as_slice(), &property.name.0].concat())
            })
            .collect();
        for name in mangled {
            self.hold_class_name(unit, &name)?;
        }
        Ok(())
    }

    /// What `class`'s link adds to its blocks, counted off its
    /// declaration as the unit wrote it, the one its traits composed, its
    /// linked record and its parent's.
    pub(super) fn linking(&self, class: ClassId) -> Option<Linking> {
        let row = self.declared_row(class)?;
        let written = row.unit.classes.get(row.at as usize)?;
        let decl = row.decl()?;
        let record = row.record()?;
        let abstracts = row
            .composed
            .as_ref()
            .map_or(0, |composed| composed.abstracts as usize);
        let composed = decl.methods.len().saturating_sub(abstracts);
        let first = Members {
            methods: composed.saturating_sub(php_unit::written_methods(written).len()),
            constants: decl.constants.len().saturating_sub(written.constants.len()),
            properties: decl
                .properties
                .len()
                .saturating_sub(written.properties.len()),
        };
        let trait_slots = SlotCounts::of(
            decl.properties
                .get(written.properties.len()..)
                .unwrap_or_default(),
        );
        let parent = (decl.parent.as_ref())
            .and_then(|name| self.class_named(name.into()))
            .and_then(|parent| {
                let own = Members {
                    methods: composed,
                    constants: decl.constants.len(),
                    properties: decl.properties.len(),
                };
                self.inherited(class, parent, own)
            });
        let taken = parent
            .as_ref()
            .map_or_else(Members::default, |parent| parent.rows);
        let last = Members {
            methods: (record.methods.rows().len()).saturating_sub(composed + taken.methods),
            constants: (record.constants.rows().len())
                .saturating_sub(decl.constants.len() + taken.constants),
            properties: (record.properties.rows().len())
                .saturating_sub(decl.properties.len() + taken.properties),
        };
        let interfaces = self.interface_sizes(decl, written);
        Some(Linking {
            first,
            trait_slots,
            parent,
            last,
            interfaces,
        })
    }

    /// What `parent` gives `class` as it links: its tables' sizes, the
    /// rows of them `class`'s record took past its own, the first `own`
    /// of each table, and its slots.
    fn inherited(&self, class: ClassId, parent: ClassId, own: Members) -> Option<Inherited> {
        let theirs = self.linked_class(parent)?;
        let ours = self.linked_class(class)?;
        let Members {
            methods,
            constants,
            properties,
        } = own;
        let pair = (class, parent);
        Some(Inherited {
            tables: Members {
                methods: theirs.methods.rows().len(),
                constants: theirs.constants.rows().len(),
                properties: theirs.properties.rows().len(),
            },
            rows: Members {
                methods: self.taken(
                    pair,
                    Table::Methods,
                    (&ours.methods, &theirs.methods),
                    methods,
                ),
                constants: self.taken(
                    pair,
                    Table::Constants,
                    (&ours.constants, &theirs.constants),
                    constants,
                ),
                properties: self.taken(
                    pair,
                    Table::Properties,
                    (&ours.properties, &theirs.properties),
                    properties,
                ),
            },
            slots: self.slots(parent),
        })
    }

    /// How many rows of `theirs`, a table of `parent`'s, `class`'s
    /// table `ours` took from it: those it holds past its first `own`,
    /// its own and its traits', as `do_inherit_method`,
    /// `do_inherit_class_constant` and `do_inherit_property` append a row
    /// the class does not declare.
    fn taken<C: php_link::Case>(
        &self,
        (class, parent): (ClassId, ClassId),
        table: Table,
        (ours, theirs): (&RecordRows<C>, &RecordRows<C>),
        own: usize,
    ) -> usize {
        (theirs.rows().iter())
            .filter(|row| {
                let Some(name) = self.row_name(parent, **row, table) else {
                    return false;
                };
                ours.find_at(name, |row| {
                    self.row_name(class, *row, table).unwrap_or_default()
                })
                .is_some_and(|(at, _)| at >= own)
            })
            .count()
    }

    /// The name of the member a row of `class`'s `table` is; none where
    /// no declaration names it.
    fn row_name(&self, class: ClassId, row: Row<ClassId>, table: Table) -> Option<&[u8]> {
        let decl = self.symbols().class_decl(row.holder_in(class))?;
        let at = row.at as usize;
        let name = match table {
            Table::Methods => &decl.methods.get(at)?.function.name,
            Table::Constants => &decl.constants.get(at)?.name,
            Table::Properties => &decl.properties.get(at)?.name,
        };
        Some(&name.0)
    }

    /// `default_static_members_count` of a linked class ([`Self::slots`]).
    pub(super) fn static_members(&self, class: ClassId) -> usize {
        self.slots(class).statics
    }

    /// `default_properties_count` and `default_static_members_count` of
    /// a linked class: its own slots and every class's above it, a
    /// redeclared property's among them, whose own slot
    /// `do_inherit_property` leaves undefined and counted.
    fn slots(&self, class: ClassId) -> SlotCounts {
        let mut slots = SlotCounts::default();
        let mut at = Some(class);
        // No declaration makes a chain this deep.
        for _ in 0..256 {
            let Some(decl) = at.and_then(|class| self.symbols().class_decl(class)) else {
                break;
            };
            let own = SlotCounts::of(&decl.properties);
            slots.instance += own.instance;
            slots.statics += own.statics;
            at = (decl.parent.as_ref()).and_then(|name| self.class_named(name.into()));
        }
        slots
    }

    /// `ce->interfaces`' sizes as `decl`'s link lays it
    /// ([`Linking::interfaces`]): with interfaces of its own, a block for
    /// them and the parent's (`zend_do_link_class`), reallocated for each
    /// new one that extends others to the interfaces placed and the ones
    /// it extends (`zend_do_inherit_interfaces`); with none, the parent's
    /// copied, where it has any. `Stringable`, which the link adds where
    /// a trait gave the class `__toString`, is one more after them all.
    fn interface_sizes(&self, decl: &ClassDecl, written: &ClassDecl) -> Vec<usize> {
        let interfaces_of = |class: ClassId| {
            self.linked_class(class)
                .map_or(&[][..], |record| &record.interfaces)
        };
        let theirs = (decl.parent.as_ref())
            .and_then(|name| self.class_named(name.into()))
            .map_or(&[][..], interfaces_of);
        let own = written.interfaces.len().min(decl.interfaces.len());
        let mut sizes = Vec::new();
        let mut placed: Vec<ClassId> = theirs.to_vec();
        if own > 0 {
            sizes.push(own + theirs.len());
            let mut new = Vec::new();
            for name in decl.interfaces.get(..own).unwrap_or_default() {
                if let Some(interface) = self.class_named(name.into())
                    && !placed.contains(&interface)
                {
                    placed.push(interface);
                    new.push(interface);
                }
            }
            for interface in new {
                let extended = interfaces_of(interface);
                if extended.is_empty() {
                    continue;
                }
                sizes.push(placed.len() + extended.len());
                for &above in extended {
                    if !placed.contains(&above) {
                        placed.push(above);
                    }
                }
            }
        } else if !theirs.is_empty() {
            sizes.push(theirs.len());
        }
        let mut count = placed.len();
        for _ in own..decl.interfaces.len() {
            count += 1;
            sizes.push(count);
        }
        sizes
    }
}
