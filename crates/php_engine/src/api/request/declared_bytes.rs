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
use php_unit::decl::{
    Attribute, ClassDecl, ConstExpr, FunctionDecl, PropertyDecl, TraitAdaptation, Visibility,
};
use php_unit::{Declares, Grown, OpArray, Opcode, Unit};

use super::Request;
use super::unit_strings::OpArrayStrings;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Emalloc, PhpArray, Value};
use crate::unit_cache::{Level, Optimizer};
use php_compile::syntax::ScannerHeap;

/// `sizeof(zend_op)`.
pub(super) const OPLINE: usize = 32;
/// `sizeof(zval)`.
const ZVAL: usize = 16;
/// `sizeof(zend_class_entry)`, carved at `Z/Zend/zend_compile.c:9253`.
const CLASS_ENTRY: usize = 520;
/// `sizeof(zend_property_info)`, carved at `Z/Zend/zend_API.c:4525`.
const PROPERTY_INFO: usize = 72;
/// `sizeof(zend_class_constant)`, carved at `Z/Zend/zend_API.c:4888`.
const CLASS_CONSTANT: usize = 56;
/// A block of `CG(ast_arena)` (`Z/Zend/zend_language_scanner.l:599`).
const AST_BLOCK: usize = 32 * 1024;
/// `sizeof(zend_ast_zval)`, and a two-child `zend_ast`'s (`Z/Zend/zend_ast.h`).
const AST_NODE: usize = 24;
/// `INITIAL_OP_ARRAY_SIZE` (`Z/Zend/zend_compile.h`).
const INITIAL_OPLINES: usize = 64;
/// `sizeof(zend_string *)`, an entry of `op_array->vars` and of
/// `op_array->dynamic_func_defs`.
const POINTER: usize = 8;
/// `lookup_cv`'s step: sixteen more names at a time.
const VARS_STEP: usize = 16;
/// `zend_add_literal`'s step: sixteen more constants at a time.
const LITERALS_STEP: usize = 16;
/// `STACK_BLOCK_SIZE`, the elements `zend_stack_push` grows a stack by
/// (`Z/Zend/zend_stack.h:31`).
const STACK_BLOCK: usize = 16;
/// `sizeof(int)`, an element of the scanner's `state_stack`.
const SCANNER_STATE: usize = 4;
/// `sizeof(zend_nest_location)`, an element of the scanner's
/// `nest_location_stack` (`Z/Zend/zend_language_scanner.h:72`).
const NEST_LOCATION: usize = 8;
/// `sizeof(zend_arg_info)`.
const ARG_INFO: usize = 32;
/// `sizeof(zend_live_range)`.
const LIVE_RANGE: usize = 12;
/// `sizeof(zend_try_catch_element)`.
const TRY_CATCH: usize = 16;
/// `sizeof(zend_brk_cont_element)`, four `int`s and a `bool`
/// (`Z/Zend/zend_compile.h:160`).
const BRK_CONT: usize = 20;
/// `sizeof(uint32_t)`, the op array's refcount.
const REFCOUNT: usize = 4;
/// The size `zend_initialize_class_data` gives each of a class's tables.
const CLASS_TABLE: u32 = 8;
/// The size `zend_file_context_begin` gives `FC(seen_symbols)`
/// (`Z/Zend/zend_compile.c:411`).
const SEEN_SYMBOLS: u32 = 8;
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
/// `sizeof(HashTable)`, a holder's `attributes`.
const HASH_TABLE: usize = 56;
/// `ZEND_ATTRIBUTE_SIZE(0)`, a `zend_attribute` with no argument.
const ATTRIBUTE: usize = 40;
/// `sizeof(zend_attribute_arg)`, what each argument adds to it.
const ATTRIBUTE_ARG: usize = 24;
/// `HT_HASH_SIZE(HT_MIN_MASK)`, the two hash slots a packed table keeps
/// before its elements.
const PACKED_HASH: usize = 8;
/// The size `zend_add_attribute` inits a holder's `attributes` with.
const ATTRIBUTES: usize = 8;

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

/// What a unit declares, laid down: the functions' blocks together, the
/// methods' and hooks' together, each class's apart by its place in
/// `Unit::classes`, and each dynamic definition's apart with the op
/// array that declares it.
#[derive(Default)]
pub(super) struct Laid {
    tables: Blocks,
    members: Blocks,
    classes: Vec<Option<ClassBlocks>>,
    dynamic: Vec<(Blocks, Declarer)>,
}

impl Laid {
    /// Every block given back.
    pub(super) fn free(self, req: &mut Request) {
        self.tables.free(req);
        self.members.free(req);
        for class in self.classes.into_iter().flatten() {
            class.into_blocks().free(req);
        }
        for (blocks, _) in self.dynamic {
            blocks.free(req);
        }
    }

    /// Each op array's oplines taken out, as `zend_foreach_op_array`
    /// walks them after the main op array: its dynamic definitions, then
    /// the functions, then the methods and hooks.
    fn take_oplines(&mut self) -> Vec<Oplines> {
        let dynamic = self
            .dynamic
            .iter_mut()
            .flat_map(|(blocks, _)| blocks.oplines.drain(..));
        (dynamic.chain(self.tables.oplines.drain(..)))
            .chain(self.members.oplines.drain(..))
            .collect()
    }
}

/// One op array's oplines with its constants after them, the block
/// `pass_two` reallocates them into (`Z/Zend/zend_opcode.c:1117`), and
/// how many of each it holds.
pub(super) struct Oplines {
    block: Emalloc,
    lines: usize,
    literals: usize,
    /// How pass 11 would lay its table of the constants.
    merge: Merge,
}

/// How `zend_optimizer_compact_literals` lays its table of an op array's
/// constants, `zend_hash_init`ed to their number, at least eight and a
/// power of two (`Z/Zend/Optimizer/compact_literals.c:272`): by the first
/// it keys, a whole number below the table's size keying it packed and
/// any other whole number, string or float keying it as a hash; and a
/// string or a float after a packed start turns it into a hash. A
/// `null`, a boolean or an array is merged without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Merge {
    Unkeyed,
    Hash,
    Packed,
    Converted,
}

impl Merge {
    /// The table for `literals` constants.
    fn of(literals: &[php_heap::Zval]) -> Merge {
        use php_heap::ZvalType;
        let size = table_size(literals.len());
        literals.iter().fold(Merge::Unkeyed, |merge, literal| {
            let below = literal
                .as_long()
                .and_then(|long| usize::try_from(long).ok())
                .is_some_and(|long| long < size);
            match (merge, literal.kind()) {
                (Merge::Unkeyed, ZvalType::Long) if below => Merge::Packed,
                (Merge::Unkeyed, ZvalType::Long | ZvalType::Double | ZvalType::String) => {
                    Merge::Hash
                }
                (Merge::Packed, ZvalType::Double | ZvalType::String) => Merge::Converted,
                (merge, _) => merge,
            }
        })
    }

    /// The bytes of the table laid as this for `count` entries: a hash's
    /// buckets and its two slots each (`HT_SIZE_EX`), a packed table's
    /// values and its two slots of hash (`HT_PACKED_SIZE_EX`).
    fn bytes(self, count: usize) -> usize {
        let size = table_size(count);
        match self {
            Merge::Packed => size * ZVAL + 2 * HASH_SLOT,
            _ => size * BUCKET + 2 * size * HASH_SLOT,
        }
    }
}

/// `zend_hash_check_size`: at least eight, and a power of two.
fn table_size(count: usize) -> usize {
    count.next_power_of_two().max(8)
}

/// `pass_two`'s block for `oplines` oplines and `literals` constants:
/// the oplines aligned to 16 (`ZEND_MM_ALIGNED_SIZE_EX`) and the
/// constants after them.
fn packed(oplines: usize, literals: usize) -> usize {
    (oplines * OPLINE).next_multiple_of(16) + literals * ZVAL
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
enum CopyOf {
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
    of: CopyOf,
    holders: usize,
    blocks: Vec<Emalloc>,
}

impl Copied {
    /// The blocks `copy` lays for what `of` names, held by the declaring
    /// class alone.
    fn lay(req: &mut Request, of: CopyOf, copy: &AstCopy) -> Result<Copied, Thrown> {
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
    /// A class's blocks as `lay` lays them into its three tables, each
    /// `zend_initialize_class_data`'s, given back where it fails.
    fn laid(
        req: &mut Request,
        lay: impl FnOnce(&mut Request, &mut ClassBlocks) -> Result<(), Thrown>,
    ) -> Result<ClassBlocks, Thrown> {
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
        match lay(req, &mut blocks) {
            Ok(()) => Ok(blocks),
            Err(thrown) => {
                blocks.into_blocks().free(req);
                Err(thrown)
            }
        }
    }

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
            oplines: Vec::new(),
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
            self.defaults.grow(req, linking.hooked_slots)?;
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
    /// The slots `do_inherit_property` adds, one `perealloc` each, for a
    /// virtual property with hooks the class declares over a parent's
    /// backed one with none (`use_child_prop`).
    hooked_slots: usize,
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
    /// Each op array's oplines with its constants after them.
    oplines: Vec<Oplines>,
    /// Tables `zend_new_array` lays down, the header `emalloc`'d.
    arrays: Vec<php_heap::Arr>,
    /// Tables `zend_hash_init` lays down inside the `zend_class_entry`,
    /// which the compile's arena holds, so only their data is the heap's.
    embedded: Vec<php_heap::EmbeddedArr>,
}

impl Blocks {
    /// Whether it holds no block.
    fn is_empty(&self) -> bool {
        self.emalloced.is_empty()
            && self.oplines.is_empty()
            && self.arrays.is_empty()
            && self.embedded.is_empty()
    }

    /// Every block given back: each table's rows, its data and, for one
    /// `zend_new_array` laid down, its header, as `zend_array_destroy`
    /// and `zend_hash_destroy` give them back.
    pub(super) fn free(self, req: &mut Request) {
        for block in self.emalloced {
            block.free(req);
        }
        for oplines in self.oplines {
            oplines.block.free(req);
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
        self.oplines.extend(others.oplines);
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
        self.tables.take(laid.members);
        self.classes = laid.classes;
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
    /// 400
    /// 0
    /// 992
    /// -992
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
/// 1616
/// ```
fn op_array(
    req: &mut Request,
    blocks: &mut Blocks,
    code: &OpArray,
    declared: Option<(&FunctionDecl, bool)>,
) -> Result<(), Thrown> {
    let growing = init_op_array(req, blocks, code, declared)?;
    compiled(req, blocks, code, growing)
}

/// An op array between its `init_op_array` and its `pass_two`, its
/// tables grown as php's compile grows them while it is
/// `CG(active_op_array)`.
struct Growing {
    /// `op_array->opcodes`: `INITIAL_OP_ARRAY_SIZE` oplines, four times
    /// as many each time `get_next_op` fills them
    /// (`Z/Zend/zend_compile.c:157`).
    oplines: Option<Emalloc>,
    /// `op_array->literals`: sixteen more each time `zend_add_literal`
    /// fills them (`:597`).
    constants: Option<Emalloc>,
    /// `op_array->vars`: sixteen more names each time `lookup_cv` fills
    /// them (`:556`).
    names: Option<Emalloc>,
    /// `op_array->dynamic_func_defs`: one pointer more for each function
    /// `zend_add_dynamic_func_def` adds (`:8336`).
    defs: Option<Emalloc>,
    /// `CG(context).brk_cont_array`: one `zend_brk_cont_element` more
    /// for each loop and `switch` `zend_begin_loop` begins
    /// (`get_next_brk_cont_element`, `:171`).
    loops: Option<Emalloc>,
    grown: Grown,
    /// The main op array's constants php's compile has not made yet,
    /// each by its place among them ([`held`]).
    held: Vec<(u32, php_heap::Uncounted)>,
}

impl Growing {
    /// The tables grown, one `erealloc` a step, to hold what `to` counts,
    /// after the constants it [`held`] that they hold are made.
    fn to(&mut self, req: &mut Request, to: Grown) -> Result<(), Thrown> {
        self.made(req, to.constants);
        let mut size = (self.oplines.as_ref().map_or(0, Emalloc::len) / OPLINE).max(1);
        while (to.oplines as usize) > size {
            size = size.saturating_mul(4);
            regrow(req, &mut self.oplines, size * OPLINE)?;
        }
        let steps = |from: u32, to: u32, step: usize| {
            let from = (from as usize).next_multiple_of(step);
            (from..(to as usize).next_multiple_of(step))
                .step_by(step)
                .map(move |at| at + step)
        };
        for size in steps(self.grown.constants, to.constants, LITERALS_STEP) {
            regrow(req, &mut self.constants, size * ZVAL)?;
        }
        for size in steps(self.grown.vars, to.vars, VARS_STEP) {
            regrow(req, &mut self.names, size * POINTER)?;
        }
        for size in steps(self.grown.dynamic_func_defs, to.dynamic_func_defs, 1) {
            regrow(req, &mut self.defs, size * POINTER)?;
        }
        for size in steps(self.grown.loops, to.loops, 1) {
            regrow(req, &mut self.loops, size * BRK_CONT)?;
        }
        self.grown = Grown {
            oplines: self.grown.oplines.max(to.oplines),
            constants: self.grown.constants.max(to.constants),
            vars: self.grown.vars.max(to.vars),
            dynamic_func_defs: self.grown.dynamic_func_defs.max(to.dynamic_func_defs),
            loops: self.grown.loops.max(to.loops),
        };
        Ok(())
    }

    /// Each constant it [`held`] whose place is below `below` counted.
    fn made(&mut self, req: &mut Request, below: u32) {
        let (made, held): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(at, _)| *at < below);
        self.held = held;
        for (_, value) in made {
            req.heap_mut().recount(value);
        }
    }
}

/// `erealloc(block, len)`, an `emalloc` where there is none yet.
fn regrow(req: &mut Request, block: &mut Option<Emalloc>, len: usize) -> Result<(), Thrown> {
    *block = Some(match block.take() {
        Some(laid) => laid.resize(req, len)?,
        None => Emalloc::new(req, len)?,
    });
    Ok(())
}

/// How far `code` grew by its `pass_two`: all it holds.
fn grown(code: &OpArray) -> Grown {
    let count = |len: usize| u32::try_from(len).unwrap_or(u32::MAX);
    Grown {
        oplines: count(code.oplines.len()),
        constants: count(code.literals.len()),
        vars: count(code.vars.len()),
        dynamic_func_defs: count(code.dynamic_func_defs.len()),
        loops: code.loops,
    }
}

/// What php's compile of `code` lays before its body
/// (`zend_compile_func_decl_ex`, `Z/Zend/zend_compile.c:8442`), handed
/// back as it grows ([`Growing`]): `init_op_array`'s refcount and
/// `INITIAL_OP_ARRAY_SIZE` oplines (`Z/Zend/zend_opcode.c:54`, `:57`);
/// for a function declared as `declared`, its `attributes`, the
/// function's and its parameters' in one table
/// (`zend_compile_attributes`, `zend_compile.c:8511`), its `arg_info`,
/// one an entry for each parameter and one for its return type
/// (`zend_compile_params`, `:8555`), and the copy of each default the
/// compile left a tree, which the `RECV_INIT`'s constant keeps
/// (`zend_const_expr_to_zval`); and its static and bound variables'
/// table, `zend_new_array(8)` at the first and a bucket each
/// (`zend_compile_static_var_common`, `zend_compile_closure_binding`).
fn init_op_array(
    req: &mut Request,
    blocks: &mut Blocks,
    code: &OpArray,
    declared: Option<(&FunctionDecl, bool)>,
) -> Result<Growing, Thrown> {
    blocks.emalloced.push(Emalloc::new(req, REFCOUNT)?);
    let growing = Growing {
        oplines: Some(Emalloc::new(req, INITIAL_OPLINES * OPLINE)?),
        constants: None,
        names: None,
        defs: None,
        loops: None,
        grown: Grown::default(),
        held: Vec::new(),
    };
    if let Some((decl, method)) = declared {
        let parameters = decl.parameters.iter().flat_map(|p| &p.attributes);
        attributes(
            req,
            &mut blocks.emalloced,
            decl.attributes.iter().chain(parameters),
            false,
        )?;
        let args = arg_infos(decl, method) * ARG_INFO;
        if args > 0 {
            blocks.emalloced.push(Emalloc::new(req, args)?);
        }
        let defaults = decl.parameters.iter().filter_map(|p| p.default.expr());
        for default in defaults.filter(|default| !super::linking::is_literal(default)) {
            for len in AstCopy::of(default).blocks() {
                blocks.emalloced.push(Emalloc::new(req, len)?);
            }
        }
    }
    if !code.bound_vars.is_empty() {
        let laid = req.heap.array_sized(CLASS_TABLE);
        let statics = req.fatal_of(laid)?;
        blocks.arrays.push(statics);
        add_rows(req, statics, code.bound_vars.len())?;
    }
    Ok(growing)
}

/// What the compile of `code` lays after [`init_op_array`] and what
/// `pass_two` makes of it: its tables grown to all they hold
/// ([`Growing`]) and its `try`/`catch` elements, one block, which a
/// reallocation one element at a time leaves in the bin of its last
/// size; then `pass_two`, the names reallocated to their number where it
/// is not a multiple (`Z/Zend/zend_opcode.c:1103`), the oplines
/// reallocated to hold the constants after them, aligned to 16
/// (`ZEND_MM_ALIGNED_SIZE_EX`, `:1117`), beside the constants until they
/// are copied in and freed (`:1123`), and the live ranges
/// `zend_calc_live_ranges` lays (`:1251`); and last its
/// `CG(context).brk_cont_array`, an element for each loop and `switch`
/// ([`Growing`]), which `pass_two` reads for each `BRK` and `CONT`
/// (`zend_get_brk_cont_target`, `Z/Zend/zend_opcode.c:721`) and
/// `zend_oparray_context_end` frees after it (`Z/Zend/zend_compile.c:355`).
/// So a loop of the main op array stands under the `pass_two` of a
/// closure written in it:
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3823-' . sprintf('%010d', getmypid()) . '.php';
/// foreach (['', 'for (;;) { break; } ', 'foreach ([1] as $a) { for (;;) { switch ($a) { default: break 2; } } while (0) {} } ', 'for (;;) { $c = function () { while (0) {} do {} while (0); }; break; } '] as $code) {
///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
///     [$usage, $peak] = explode(' ', trim($fresh));
///     echo $peak - $usage, "\n";
/// }
/// unlink($f);
/// ```
/// ```expect process
/// 35312
/// 35400
/// 35440
/// 36896
/// ```
fn compiled(
    req: &mut Request,
    blocks: &mut Blocks,
    code: &OpArray,
    mut growing: Growing,
) -> Result<(), Thrown> {
    growing.to(req, grown(code))?;
    let try_catch = code.try_catch.len() * TRY_CATCH;
    if try_catch > 0 {
        blocks.emalloced.push(Emalloc::new(req, try_catch)?);
    }
    blocks.emalloced.extend(growing.defs);
    if let Some(names) = growing.names {
        let vars = code.vars.len();
        let names = if vars.is_multiple_of(VARS_STEP) {
            names
        } else {
            names.resize(req, vars * POINTER)?
        };
        blocks.emalloced.push(names);
    }
    let last = code.oplines.len();
    let literals = code.literals.len();
    let oplines = match growing.oplines {
        Some(oplines) => oplines.resize(req, packed(last, literals))?,
        None => Emalloc::new(req, packed(last, literals))?,
    };
    blocks.oplines.push(Oplines {
        block: oplines,
        lines: last,
        literals,
        merge: Merge::of(&code.literals),
    });
    if let Some(constants) = growing.constants {
        constants.free(req);
    }
    let live_ranges = code.live_ranges.len() * LIVE_RANGE;
    if live_ranges > 0 {
        blocks.emalloced.push(Emalloc::new(req, live_ranges)?);
    }
    if let Some(loops) = growing.loops {
        loops.free(req);
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

/// What `zend_compile_attributes` lays on the request's heap for the
/// attributes of one holder (`zend_add_attribute`): a function's with
/// its parameters', a class's, a property's, a class constant's, or a
/// `const` statement's, which `DECLARE_ATTRIBUTED_CONST` hands the
/// constant it declares (`zend_constant_add_attributes`). That is the
/// `attributes` table, `zend_hash_init(…, 8, …)` and packed as each is
/// appended, and per attribute its `zend_attribute`, its name as
/// resolved where it is a string of its own ([`Attribute::own_name`]),
/// its lowered name where lowering changes it (`zend_string_tolower_ex`),
/// each argument's name, and the tree `zend_const_expr_to_zval` copies
/// for an argument that folds to no value ([`AstCopy`]); a value that
/// folds is the unit's, a string the scanner's. A string of one byte is
/// the scanner's `ZSTR_CHAR`. A promoted parameter's property holds its
/// list `again`, the arguments' names the parameter's strings. An
/// uncached include's and an `eval`'s alike (the program under the
/// recipe's `TMPDIR`, written into the past, the includes written during
/// the request):
///
/// ```php
/// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
/// $f = $put('w3322-attributes.inc', <<<'PHP'
/// <?php
/// ini_set('opcache.file_update_protection', '2');
/// function inc($name, $src) { $f = sys_get_temp_dir() . "/w3322-$name.inc"; file_put_contents($f, $src); $m = memory_get_usage(); include $f; $d = memory_get_usage() - $m; unlink($f); return $d; }
/// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
/// inc('w0', '<?php class W {} $q0 = 1;'); m('$q1 = 1;'); m('function w1() {}');
/// echo inc('class', '<?php #[Attribute] class Az1 { }'), " ", m('#[Attribute] class Ez1 { }'), "\n";
/// echo inc('fn', '<?php #[Az1] function az1() {}'), " ", m('#[Az1] function ez1() {}'), "\n";
/// echo inc('arg', '<?php #[Az1("y")] function az2() {}'), "\n";
/// echo inc('prop', '<?php class Az3 { #[Az1] public $p; }'), "\n";
/// echo inc('param', '<?php function az4(#[SensitiveParameter] $p) {}'), " ", m('function ez4(#[SensitiveParameter] $p) {}'), "\n";
/// echo m('#[Az1(name: "abcdefgh", v: [1, 2], w: X1 . "y")] #[az1, A] function ez5(#[Az1] $a, #[Az1] $b) {}'), "\n";
/// echo m('class Ez6 { #[Az1] const C = 1; #[Az1] function m(#[Az1] $p) {} }'), "\n";
/// $junk = array_fill(0, 1000, str_repeat("x", 100));
/// echo count((new ReflectionFunction('ez5'))->getAttributes()), count((new ReflectionClass('Ez6'))->getMethod('m')->getAttributes()), "\n";
/// PHP);
/// include $f;
/// unlink($f);
/// ```
/// ```expect regime=jit-off
/// 496 512
/// 496 528
/// 520
/// 816
/// 624 640
/// 1496
/// 1672
/// 31
/// ```
pub(super) fn attributes<'a>(
    req: &mut Request,
    emalloced: &mut Vec<Emalloc>,
    attributes: impl Iterator<Item = &'a Attribute>,
    again: bool,
) -> Result<(), Thrown> {
    let string = |bytes: &[u8]| (bytes.len() > 1).then_some(STRING_HEADER + bytes.len() + 1);
    let mut count = 0;
    for attribute in attributes {
        let name = &attribute.name.0;
        let lowered = name.iter().any(u8::is_ascii_uppercase);
        let mut lens = vec![ATTRIBUTE + attribute.arguments.len() * ATTRIBUTE_ARG];
        lens.extend(string(name).filter(|_| attribute.own_name));
        lens.extend(lowered.then_some(STRING_HEADER + name.len() + 1));
        for argument in &attribute.arguments {
            let named = argument.name.as_ref().filter(|_| !again);
            lens.extend(named.and_then(|name| string(&name.0)));
            if !super::linking::is_literal(&argument.value) {
                lens.extend(AstCopy::of(&argument.value).blocks());
            }
        }
        for len in lens {
            emalloced.push(Emalloc::new(req, len)?);
        }
        count += 1;
    }
    if count > 0 {
        // `HT_PACKED_SIZE_EX(nTableSize, HT_MIN_MASK)`, doubled from 8 as
        // `zend_hash_packed_grow` doubles it.
        let slots = count.max(ATTRIBUTES).next_power_of_two();
        emalloced.push(Emalloc::new(req, HASH_TABLE)?);
        emalloced.push(Emalloc::new(req, slots * ZVAL + PACKED_HASH)?);
    }
    Ok(())
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
    /// strings it holds. A trait's `self::class` and `parent::class` stay
    /// a `ZEND_AST_CLASS_NAME` the using class evaluates, a node with no
    /// child and no string (the file under the recipe's `TMPDIR`):
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('w3310-trait-class-name.inc', <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// echo m('trait TT { const K = self::class; public $p = self::class; function g($a = self::class) { return [$a, self::K, $this->p]; } } class UT { use TT; } function ft() { return (new UT)->g(); }'), "\n";
    /// echo m('trait TP { const K = parent::class; } class BP {} class CP extends BP { use TP; }'), "\n";
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo json_encode(eval('return [ft(), CP::K];')), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 3704
    /// 1048
    /// [["UT","UT","UT"],"BP"]
    /// ```
    fn expr(&mut self, expr: &ConstExpr) {
        match expr {
            ConstExpr::Null
            | ConstExpr::Bool(_)
            | ConstExpr::Long { .. }
            | ConstExpr::Double(_)
            | ConstExpr::UnitValue(_) => self.tree += AST_ZVAL,
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
            // `self::class` and `parent::class` keep the fetch type in
            // the node's `attr` and no child (`zend_compile_const_expr_class_name`).
            ConstExpr::ClassName { class, .. } => {
                self.node(1);
                if !(class.0.eq_ignore_ascii_case(b"self")
                    || class.0.eq_ignore_ascii_case(b"parent"))
                {
                    self.string(&class.0);
                }
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
///
/// The rows of the `bodies` methods with a body are left to the walk,
/// which lays each as its method begins ([`Walk::method`]), and its
/// constants and properties, each where it is written ([`Walk::member`]).
fn class(req: &mut Request, class: &ClassDecl, bodies: usize) -> Result<ClassBlocks, Thrown> {
    let mut compiled = Compiled::default();
    compiled.written.methods = php_unit::written_methods(class)
        .len()
        .saturating_sub(bodies);
    ClassBlocks::laid(req, |req, blocks| lay_class(req, blocks, class, compiled))
}

/// The constant at `at` of `class` as `zend_compile_class_const_decl`
/// declares it: its expression copied out of the compile's arena
/// (`zend_const_expr_to_zval`'s `zend_ast_copy`, `:9044`), a case's
/// `ZEND_AST_CONST_ENUM_INIT` (`:9498`) and a constant's tree where the
/// compile left one, which a value written out is not; its row; its
/// attributes.
fn constant(
    req: &mut Request,
    blocks: &mut ClassBlocks,
    class: &ClassDecl,
    at: usize,
) -> Result<(), Thrown> {
    let Some(constant) = class.constants.get(at) else {
        return Ok(());
    };
    let copy = if constant.is_case {
        Some(AstCopy::of_case(&constant.value))
    } else {
        (!super::linking::is_literal(&constant.value)).then(|| AstCopy::of(&constant.value))
    };
    if let Some(copy) = copy {
        let copied = Copied::lay(req, CopyOf::Constant(at), &copy)?;
        blocks.copies.push(copied);
    }
    add_rows(req, blocks.table(Table::Constants), 1)?;
    let held = constant.attributes.iter();
    attributes(req, &mut blocks.emalloced, held, false)
}

/// The property at `at` of `class` as `zend_compile_prop_decl` and
/// `zend_compile_params` declare it: its default's tree copied where the
/// compile left one (`:8920`); its slot of `default_properties_table`
/// or `default_static_members_table`, one `perealloc` more, but for a
/// virtual one; its row; its attributes.
fn property(
    req: &mut Request,
    blocks: &mut ClassBlocks,
    class: &ClassDecl,
    at: usize,
) -> Result<(), Thrown> {
    let Some(property) = class.properties.get(at) else {
        return Ok(());
    };
    let (of, slots) = if property.is_instance {
        (CopyOf::Default(at), &mut blocks.defaults)
    } else {
        (CopyOf::Static(at), &mut blocks.statics)
    };
    if let Some(default) = (property.default.as_ref()).filter(|d| !super::linking::is_literal(d)) {
        blocks
            .copies
            .push(Copied::lay(req, of, &AstCopy::of(default))?);
    }
    if !property.is_virtual {
        slots.grow(req, 1)?;
    }
    add_rows(req, blocks.table(Table::Properties), 1)?;
    let held = property.attributes.iter();
    attributes(req, &mut blocks.emalloced, held, property.is_promoted)
}

/// `zend_lazy_class_load`'s copy of a class of a file the cache keeps,
/// which php links in each request that declares it where the
/// inheritance cache cannot keep the link (an enum, whose link adds its
/// `cases`, `from` and `tryFrom`, and a class whose link took a class
/// the request declared): its default tables at their counts, and each
/// table its compile filled at the size it was compiled to, the
/// `zend_class_entry`, its members and their types in the compile's
/// arena. A cached file's enum costs its copy and its link, and a cached
/// class over an `eval`'d parent its copy and the parent's rows, one
/// using an `eval`'d trait its own and the trait's properties' (the
/// program under the recipe's `TMPDIR`, written into the past so opcache
/// keeps it):
///
/// ```php
/// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/w3325-$name.inc"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
/// ini_set('opcache.file_update_protection', '0');
/// include $put('w0', '<?php class W {} $q0 = 1;');
/// eval('class EvP { public $a = 1; function f() {} } trait EvT { public $t = 1; public $u = 2; public $v = 3; }');
/// $fs = [
///     $put('unit', '<?php enum E1 { case A; case B; }'),
///     $put('string', '<?php enum Ez: string { case A = "a"; }'),
///     $put('methods', '<?php enum E3: string { case A = "a"; const K = 1; function m() { return 1; } static function s() {} }'),
///     $put('implements', '<?php interface I4 { function i(); } enum E4: string implements I4 { case A = "a"; function i() {} }'),
///     $put('trait', '<?php trait T5 { function t() {} } enum E5 { use T5; case A; }'),
///     $put('nine', '<?php enum E6 { case A; case B; case C; case D; case E; case F; case G; case H; case I; }'),
///     $put('child', '<?php class M1 extends EvP { public $b = 2; function g() {} const K = 1; }'),
///     $put('empty', '<?php class M2 extends EvP { }'),
///     $put('traited', '<?php class M3 { use EvT; public $o = 0; }'),
/// ];
/// $costs = [];
/// foreach ($fs as $f) { $m = memory_get_usage(); include $f; $costs[] = memory_get_usage() - $m; }
/// echo implode(" ", $costs), "\n";
/// $junk = array_fill(0, 1000, str_repeat("x", 100));
/// echo Ez::from("a")->name, E3::A->m(), E4::tryFrom("z") === null ? "-" : "+", E5::A->t(), count(E6::cases()), (new M1)->a, (new M2)->f(), (new M3)->v, "\n";
/// foreach ($fs as $f) { unlink($f); }
/// ```
/// ```expect regime=jit-off
/// 1088 1112 1112 1120 1088 1408 1408 752 480
/// A1-913
/// ```
fn lazy_copy(req: &mut Request, compiled: Compiled) -> Result<ClassBlocks, Thrown> {
    ClassBlocks::laid(req, |req, blocks| lay_tables(req, blocks, compiled))
}

/// What a class's compile fills its tables with: the rows its
/// declarations write, an enum's `cases`, `from` and `tryFrom` being its
/// link's, and the slots of its default tables.
#[derive(Clone, Copy, Default)]
struct Compiled {
    written: Members,
    slots: SlotCounts,
}

impl Compiled {
    /// What `class`'s compile fills.
    fn of(class: &ClassDecl) -> Compiled {
        Compiled {
            written: Members {
                methods: php_unit::written_methods(class).len(),
                constants: class.constants.len(),
                properties: class.properties.len(),
            },
            slots: SlotCounts::of(&class.properties),
        }
    }
}

/// The three tables of a class as its declarations fill them, and its
/// default values, reallocated one at a time
/// (`zend_declare_typed_property`), which leaves the bin of the last.
fn lay_tables(
    req: &mut Request,
    blocks: &mut ClassBlocks,
    compiled: Compiled,
) -> Result<(), Thrown> {
    let Compiled { written, slots } = compiled;
    for table in Table::ALL {
        add_rows(req, blocks.table(table), written.of(table))?;
    }
    for (table, count) in [
        (&mut blocks.defaults, slots.instance),
        (&mut blocks.statics, slots.statics),
    ] {
        if count > 0 {
            table.block = Some(Emalloc::new(req, count * ZVAL)?);
            table.count = count;
        }
    }
    Ok(())
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
fn lay_class(
    req: &mut Request,
    blocks: &mut ClassBlocks,
    class: &ClassDecl,
    compiled: Compiled,
) -> Result<(), Thrown> {
    lay_tables(req, blocks, compiled)?;
    // `ce->attributes`.
    attributes(req, &mut blocks.emalloced, class.attributes.iter(), false)?;
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
    in_order(req, unit, None)
}

/// What php's compile of `unit` carves from `CG(arena)`, which nothing
/// gives back before `shutdown_compiler`, whether or not opcache
/// persists the unit: a `zend_class_entry` for each class
/// (`zend_compile_class_decl`), then a `zend_property_info` for each of
/// its properties and a `zend_class_constant` for each of its constants
/// and cases (`zend_declare_typed_property`,
/// `zend_declare_typed_class_constant`), and a `zend_op_array` for each
/// function, method, hook and closure (`zend_compile_func_decl`). A
/// union type's list and a hooked property's table are not carved.
pub(super) fn carve(req: &mut Request, unit: &Unit) -> Result<(), Thrown> {
    let classes = unit.classes.iter().flat_map(|class| {
        std::iter::once(CLASS_ENTRY)
            .chain(std::iter::repeat_n(PROPERTY_INFO, class.properties.len()))
            .chain(std::iter::repeat_n(CLASS_CONSTANT, class.constants.len()))
    });
    let op_arrays =
        unit.functions.len() + unit.methods.len() + unit.hooks.len() + unit.dynamic.len();
    let sizes = classes.chain(std::iter::repeat_n(crate::exec::OP_ARRAY_SIZE, op_arrays));
    for size in sizes {
        let carved = req.heap_mut().carve(size);
        req.fatal_of(carved)?;
    }
    Ok(())
}

/// php's compile of a file opcache persists, charged to the request
/// that includes it as it compiles (design §5.13, §6.3): its tree in
/// `CG(ast_arena)`, `zend_arena_create(1024 * 32)` and a block of that
/// size more each time it is full, here a `zend_ast` of two children or
/// a `zend_ast_zval`, 24 bytes either, for each of the `tree` elements of
/// the engine's tree, whose shape is not php's; its carves ([`carve`]), held to
/// the request's end; and the op arrays' and the classes' blocks it
/// `emalloc`s ([`of_unit`]), the main op array's last, which stand
/// after the tree is given back, through what opcache lays while the
/// `optimizer` runs ([`optimized`]), until `zend_persist` has copied them
/// into shared memory and the request's copies are given back. So a
/// file too large for the limit meets it in its compile, at the hard
/// limit as php's does (`oom_hard_limit_exceeded_during_complilation.phpt`),
/// and a large include leaves php's carves standing, its peak above
/// them:
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3479-' . sprintf('%010d', getmypid()) . '.php';
/// $src = "<?php\n";
/// for ($i = 0; $i < 2000; $i++) { $src .= "class C$i { public \$a; public \$b; function f() {} function g() {} }\n"; }
/// file_put_contents($f, $src);
/// $p = memory_get_peak_usage(); $m = memory_get_usage();
/// require $f;
/// $held = memory_get_usage() - $m;
/// echo $held, " ", memory_get_peak_usage() - $p > 2 * $held ? "above" : "within", "\n";
/// unlink($f);
/// ```
/// ```expect regime=immutable-literals
/// 2359392 above
/// ```
pub(super) fn persisted(
    req: &mut Request,
    unit: &Unit,
    tree: usize,
    optimizer: Optimizer,
) -> Result<(), Thrown> {
    let (mut main, mut laid) = with_tree(req, tree, |req| {
        carve(req, unit)?;
        let laid = of_unit(req, unit)?;
        let mut main = Blocks::default();
        match op_array(req, &mut main, &unit.main, None) {
            Ok(()) => Ok((main, laid)),
            Err(thrown) => {
                main.free(req);
                laid.free(req);
                Err(thrown)
            }
        }
    })?;
    let held = match optimizer {
        Optimizer::On { level, .. } => {
            let mut op_arrays = std::mem::take(&mut main.oplines);
            op_arrays.extend(laid.take_oplines());
            let tables = [unit.functions.len(), unit.classes.len()];
            optimized(req, tables, level, op_arrays)
        }
        Optimizer::Off => Ok(()),
    };
    main.free(req);
    laid.free(req);
    held
}

/// `sizeof(zend_persistent_script)`, in the 512-byte bin.
const PERSISTENT_SCRIPT: usize = 512;
/// `zend_arena_create(64 * 1024)`, the optimizer's arena.
const OPTIMIZER_ARENA: usize = 64 * 1024;
/// `sizeof(Bucket)`.
const BUCKET: usize = 32;
/// `sizeof(uint32_t)`, a slot of a table's hash.
const HASH_SLOT: usize = 4;

/// What opcache lays on the request's heap after the compile of a file it
/// persists and before it gives the request's op arrays back: the
/// `zend_persistent_script` that `create_persistent_script` `emalloc`s
/// for them (`Z/ext/opcache/zend_accelerator_util_funcs.c:37`), with its
/// function and class tables, each `zend_hash_extend`ed to the `tables`
/// the compile declared as they are moved into it (`:89`, `:120`); and
/// `zend_optimize_script` (`Z/Zend/Optimizer/zend_optimizer.c:1599`)
/// over `op_arrays` under `level`, main's first. That is its arena,
/// `zend_arena_create(64 * 1024)` (`:1606`), whatever the level, and
/// per op array the copy of its constants `zend_revert_pass_two` takes
/// out of the block `pass_two` packed them into (`:1192`); the CFG
/// pass's oplines, which `assemble_code_blocks` lays and puts in the
/// place of the op array's (`Z/Zend/Optimizer/block_pass.c:1058`); pass
/// 11's table of the constants it merges, `zend_hash_init` over their
/// number and laid as its first key lays it
/// (`Z/Zend/Optimizer/compact_literals.c:272`); and `zend_redo_pass_two`,
/// which grows the oplines to hold the constants again and gives the
/// copy back. Over the call graph every op array is reverted and passed
/// before pass 11 and the redo walk them all (`:1612-1714`); else each is
/// redone before the next. The passes' own work is in the arena, and
/// what else they lay, a few words each, is not laid here; nor does the
/// engine pass them, so the oplines and constants are the compiler's, as
/// the engine's op arrays hold them. So a program opcache compiled reads the peak the
/// arena set at its first line, whatever the level, and the merge's table
/// past it where a whole number keyed it packed before a string turned it
/// into a hash:
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3793-' . sprintf('%010d', getmypid()) . '.php';
/// foreach (['', '$x = 5; '] as $code) {
///     file_put_contents($f, "<?php {$code}echo memory_get_peak_usage() - memory_get_usage(), \"\\n\";");
///     touch($f, time() - 10);
///     foreach (['', ' -d opcache.optimization_level=0'] as $level) {
///         echo shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1$level " . escapeshellarg($f));
///     }
/// }
/// unlink($f);
/// ```
/// ```expect process
/// 66448
/// 66192
/// 66704
/// 66288
/// ```
fn optimized(
    req: &mut Request,
    tables: [usize; 2],
    level: Level,
    op_arrays: Vec<Oplines>,
) -> Result<(), Thrown> {
    let script = Emalloc::new(req, PERSISTENT_SCRIPT)?;
    let mut moved = Vec::new();
    for declared in tables.into_iter().filter(|declared| *declared > 0) {
        moved.push(Emalloc::new(req, Merge::Hash.bytes(declared))?);
    }
    let arena = Emalloc::new(req, OPTIMIZER_ARENA)?;
    let mut redone = Vec::with_capacity(op_arrays.len());
    let mut reverted = Vec::new();
    for oplines in op_arrays {
        let passed = Reverted::passed(req, oplines, level)?;
        if level.call_graph() {
            reverted.push(passed);
        } else {
            passed.merged(req, level)?;
            redone.push(passed.redone(req)?);
        }
    }
    for passed in &reverted {
        passed.merged(req, level)?;
    }
    for passed in reverted {
        redone.push(passed.redone(req)?);
    }
    arena.free(req);
    for oplines in redone {
        oplines.block.free(req);
    }
    for table in moved {
        table.free(req);
    }
    script.free(req);
    Ok(())
}

/// One op array between `zend_revert_pass_two` and `zend_redo_pass_two`:
/// its oplines as the passes left them, and the copy of its constants.
struct Reverted {
    oplines: Oplines,
    constants: Option<Emalloc>,
}

impl Reverted {
    /// `oplines` reverted, and passed by the CFG pass where `level` runs it.
    fn passed(req: &mut Request, oplines: Oplines, level: Level) -> Result<Reverted, Thrown> {
        let constants = match oplines.literals {
            0 => None,
            literals => Some(Emalloc::new(req, literals * ZVAL)?),
        };
        let oplines = if level.cfg() {
            let assembled = Emalloc::new(req, oplines.lines * OPLINE)?;
            oplines.block.free(req);
            Oplines {
                block: assembled,
                ..oplines
            }
        } else {
            oplines
        };
        Ok(Reverted { oplines, constants })
    }

    /// Pass 11's table where `level` runs it, laid as [`Merge`] says and
    /// given back once the constants are merged.
    fn merged(&self, req: &mut Request, level: Level) -> Result<(), Thrown> {
        if !level.compacts_literals() {
            return Ok(());
        }
        let literals = self.oplines.literals;
        match self.oplines.merge {
            Merge::Unkeyed => {}
            merge @ (Merge::Hash | Merge::Packed) => {
                Emalloc::new(req, merge.bytes(literals))?.free(req);
            }
            // `zend_hash_packed_to_hash` lays the hash before it gives
            // the packed table back.
            Merge::Converted => {
                let packed = Emalloc::new(req, Merge::Packed.bytes(literals))?;
                let hash = Emalloc::new(req, Merge::Hash.bytes(literals))?;
                packed.free(req);
                hash.free(req);
            }
        }
        Ok(())
    }

    /// `zend_redo_pass_two`: the oplines grown to hold the constants after
    /// them, and the copy given back.
    fn redone(self, req: &mut Request) -> Result<Oplines, Thrown> {
        let Oplines {
            block,
            lines,
            literals,
            ..
        } = self.oplines;
        let block = block.resize(req, packed(lines, literals))?;
        if let Some(constants) = self.constants {
            constants.free(req);
        }
        Ok(Oplines {
            block,
            ..self.oplines
        })
    }
}

/// `CG(ast_arena)` over a tree of `elements` laid on the heap while
/// `then` runs, and given back after it, as `zend_compile` destroys it
/// once the file's op arrays stand (`Z/Zend/zend_language_scanner.l:599`).
pub(super) fn with_tree<T>(
    req: &mut Request,
    elements: usize,
    then: impl FnOnce(&mut Request) -> Result<T, Thrown>,
) -> Result<T, Thrown> {
    let mut ast = Vec::new();
    let charged = (0..(elements * AST_NODE).div_ceil(AST_BLOCK).max(1))
        .try_for_each(|_| {
            ast.push(Emalloc::new(req, AST_BLOCK)?);
            Ok(())
        })
        .and_then(|()| then(req));
    for block in ast {
        block.free(req);
    }
    charged
}

/// The main op array of a unit the request compiled for itself, as
/// `zend_compile` leaves it on the request's heap: its `zend_op_array`
/// (`Z/Zend/zend_language_scanner.l:607`), until the frame that runs it
/// holds it ([`Request::main_op_array`]), and what `init_op_array`, the
/// compile and `pass_two` laid for it ([`op_array`]), which
/// `destroy_op_array` gives back as its execution ends.
#[derive(Default)]
pub(crate) struct MainOpArray {
    op_array: Option<Emalloc>,
    blocks: Blocks,
}

impl MainOpArray {
    /// The `zend_op_array`, where no frame has taken it yet.
    pub(super) fn take_op_array(&mut self) -> Option<Emalloc> {
        self.op_array.take()
    }

    /// Its blocks, to be given back with what else is over.
    pub(super) fn into_blocks(self) -> Blocks {
        let mut blocks = self.blocks;
        blocks.emalloced.extend(self.op_array);
        blocks
    }
}

/// What [`alone`] laid that stands after the compile: the main op
/// array's, and what the unit declares ([`of_unit`]), each held by the
/// unit once it is the request's ([`Request::hold_alone`]).
pub(crate) struct Alone {
    pub(super) main: MainOpArray,
    pub(super) declared: Laid,
}

/// php's compile of a source it compiles for the request alone and
/// persists nothing of, `zend_compile` under `compile_file` or
/// `compile_string` (`Z/Zend/zend_language_scanner.l:591`), charged in
/// its order once the engine's compile has ended: `CG(ast_arena)` over the
/// tree ([`with_tree`]); in it what the scanner laid as it read the
/// source ([`ScannerHeap`]), the string of each token whose bytes the
/// compile kept no string of, `unkept`, which `zend_ast_destroy` gives
/// back before the arena (`:632`), and its two stacks, which
/// `zend_restore_lexical_state` destroys after it
/// (`Z/Zend/zend_language_scanner.l:264`, `:267`); and, where the compile
/// made `unit`, its main op array ([`MainOpArray`]) and what it declares
/// ([`Alone`]), which stand after them. So the program's peak is where `pass_two` reallocates its
/// oplines beside the constants and the tree, and its first line reads
/// the main op array's blocks a file opcache persists has none of (the
/// program written within `opcache.file_update_protection`, whose
/// compile opcache leaves to php's own):
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3805-' . sprintf('%010d', getmypid()) . '.php';
/// foreach (['', '$abcdef = [1, 2]; ', 'if (strlen("abc")) { $o = new stdClass; $o->p = "xyz"; } ', 'class C {} function f() {} '] as $code) {
///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
///     touch($f, time() - 10);
///     $cached = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
///     [$usage, $peak] = explode(' ', trim($fresh));
///     echo $usage - (int) $cached, ' ', $peak - $usage, "\n";
/// }
/// unlink($f);
/// ```
/// ```expect process
/// 400 35312
/// 728 35312
/// 720 35016
/// 464 37392
/// ```
pub(super) fn alone(
    req: &mut Request,
    unit: Option<&Unit>,
    tree: usize,
    scanned: &ScannerHeap,
    unkept: Vec<usize>,
) -> Result<Option<Alone>, Thrown> {
    // The name written for each hook, given back as the hook has
    // compiled (`zend_compile_property_hooks`' `zend_string_release(name)`,
    // `Z/Zend/zend_compile.c:8814`), `get` and `set` alike three bytes.
    let mut hooks = unit.map_or(0, |unit| unit.hooks.len());
    let (names, strings): (Vec<usize>, Vec<usize>) = unkept.into_iter().partition(|len| {
        let name = *len == 3 && hooks > 0;
        hooks -= usize::from(name);
        name
    });
    let string = |len| STRING_HEADER + len + 1;
    let (names, strings) = (
        names.into_iter().map(string),
        strings.into_iter().map(string),
    );
    let stacks = [
        (scanned.nesting, NEST_LOCATION),
        (scanned.states, SCANNER_STATE),
    ]
    .into_iter()
    .filter(|(depth, _)| *depth > 0)
    .map(|(depth, element)| depth.next_multiple_of(STACK_BLOCK) * element);
    let mut stacks_laid = Vec::new();
    let laid = with_tree(req, tree, |req| {
        let (mut strings_laid, mut names_laid) = (Vec::new(), Vec::new());
        let main = lay_each(req, &mut strings_laid, strings)
            .and_then(|()| lay_each(req, &mut names_laid, names))
            .and_then(|()| lay_each(req, &mut stacks_laid, stacks))
            .and_then(|()| {
                (unit.map(|unit| main_op_array(req, unit, scanned, &mut names_laid))).transpose()
            });
        for string in strings_laid.into_iter().chain(names_laid) {
            string.free(req);
        }
        main
    });
    for stack in stacks_laid {
        stack.free(req);
    }
    laid
}

/// A block of each of `sizes` laid into `laid`, up to the first the
/// heap refuses.
fn lay_each(
    req: &mut Request,
    laid: &mut Vec<Emalloc>,
    sizes: impl Iterator<Item = usize>,
) -> Result<(), Thrown> {
    for size in sizes {
        laid.push(Emalloc::new(req, size)?);
    }
    Ok(())
}

/// `zend_compile`'s main op array of `unit`, laid in its order
/// ([`MainOpArray`]), and between its `init_op_array` and its
/// `pass_two` the file's `FC(seen_symbols)`, a bucket per name its
/// declarations registered ([`Unit::seen_symbols`],
/// `Z/Zend/zend_compile.c:438`), what the compile of the functions and
/// classes it declares carves ([`carve`]), and each of them laid at its
/// place among the main op array's statements, which grow it
/// ([`in_order`]); the table is destroyed after `pass_two`, by
/// `zend_file_context_end` (`:418`).
fn main_op_array(
    req: &mut Request,
    unit: &Unit,
    scanned: &ScannerHeap,
    hook_names: &mut Vec<Emalloc>,
) -> Result<Alone, Thrown> {
    let mut main = MainOpArray {
        op_array: Some(Emalloc::new(req, crate::exec::OP_ARRAY_SIZE)?),
        blocks: Blocks::default(),
    };
    let mut declared = Laid::default();
    let mut seen = Blocks {
        embedded: vec![php_heap::EmbeddedArr::new(SEEN_SYMBOLS)],
        ..Blocks::default()
    };
    let laid = init_op_array(req, &mut main.blocks, &unit.main, None).and_then(|mut growing| {
        growing.held = held(req, &unit.main, scanned);
        let walked = lay_each(req, &mut seen.emalloced, seen_keys(unit))
            .and_then(|()| match seen.embedded.first() {
                Some(table) => add_rows(req, table.arr(), unit.seen_symbols as usize),
                None => Ok(()),
            })
            .and_then(|()| carve(req, unit))
            .and_then(|()| in_order(req, unit, Some((&mut growing, hook_names))));
        match walked {
            Ok(laid) => {
                declared = laid;
                compiled(req, &mut main.blocks, &unit.main, growing)
            }
            Err(thrown) => {
                growing.made(req, u32::MAX);
                Err(thrown)
            }
        }
    });
    seen.free(req);
    match laid {
        Ok(()) => Ok(Alone { main, declared }),
        Err(thrown) => {
            main.into_blocks().free(req);
            declared.free(req);
            Err(thrown)
        }
    }
}

/// The constants of `main` php's compile makes after a function, a
/// closure, a method or a hook it declares has compiled, each by its
/// place, left uncounted until the walk lays the op arrays php laid
/// before them ([`in_order`]) and `main` grows to hold them
/// ([`Growing::to`]): a constant array (`zend_try_ct_eval_array`,
/// `Z/Zend/zend_compile.c:10089`), a string `zend_compile_binary_op`
/// folds (`:10228`) and a call's names in a namespace
/// (`zend_add_ns_func_name_literal`, `:630`), which php makes as it
/// compiles the statement after the declaration's `pass_two`. One the
/// constants alone own that is no string the scanner made is the
/// compile's; the name a `DECLARE_CLASS` binds is made as its class
/// begins (`zend_compile_class_decl`'s `zend_string_tolower`, `:9276`).
/// So an array or a folded string after a function stands only past its
/// `pass_two`, the compile's peak:
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3836-' . sprintf('%010d', getmypid()) . '.php';
/// foreach (['$abcdef = [1, 2]; ', 'class C {} function f() {} $abcdef = [1, 2]; ', 'function f() {} $abcdef = "abcdefghijklmnopqrstuvwxyz" . PHP_EOL; ', 'namespace a; function f() {} ', 'namespace A; function f() {} '] as $code) {
///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
///     [$usage, $peak] = explode(' ', trim($fresh));
///     echo $peak - $usage, "\n";
/// }
/// unlink($f);
/// ```
/// ```expect process
/// 35312
/// 37096
/// 37312
/// 37200
/// 37104
/// ```
fn held(
    req: &mut Request,
    main: &OpArray,
    scanned: &ScannerHeap,
) -> Vec<(u32, php_heap::Uncounted)> {
    let body = |nested: &&php_unit::Nested| nested.declares.is_body();
    let Some(from) = main
        .nested
        .iter()
        .find(body)
        .map(|nested| nested.grown.constants)
    else {
        return Vec::new();
    };
    let scanned: std::collections::HashSet<&[u8]> =
        scanned.strings().map(|(_, bytes)| bytes).collect();
    let declares = |op: &&php_unit::Opline| {
        matches!(
            op.opcode,
            Opcode::DeclareClass | Opcode::DeclareClassDelayed
        )
    };
    let bound: Vec<u32> = main
        .oplines
        .iter()
        .filter(declares)
        .map(|op| op.op1)
        .collect();
    // How many of the constants hold each string: one the compile made
    // is owned by them alone, its name and its lowered name where the
    // two are one string.
    let address = |s: php_heap::Str| s.bytes().as_ptr().addr();
    let mut owners = std::collections::HashMap::<usize, u32>::new();
    for s in main
        .literals
        .iter()
        .filter(|z| z.is_refcounted())
        .filter_map(|z| z.as_str())
    {
        *owners.entry(address(s)).or_default() += 1;
    }
    let mut held = Vec::new();
    for (at, literal) in (0u32..).zip(&main.literals) {
        let made = literal.is_refcounted()
            && match (literal.as_str(), literal.as_arr()) {
                (Some(s), _) => {
                    owners.remove(&address(s)) == Some(s.refcount()) && !scanned.contains(s.bytes())
                }
                (_, Some(a)) => a.refcount() == 1,
                _ => false,
            };
        if made && at >= from && !bound.contains(&at) {
            held.push((at, req.heap_mut().uncount(*literal)));
        }
    }
    held
}

/// The keys of `FC(seen_symbols)` the table holds the only reference
/// to: the lowered name `zend_string_tolower` copies of a closure's
/// name (`Z/Zend/zend_compile.c:8395`) and of a named class's
/// (`:9276`) that holds an upper-case letter, which nothing keeps but
/// the table, the class's being interned anew (`:9299`). A function's
/// lowered name is kept by the function table or by the constant of the
/// `DECLARE_FUNCTION` that binds it, and a constant's is its name.
fn seen_keys(unit: &Unit) -> impl Iterator<Item = usize> {
    let closures = unit
        .dynamic
        .iter()
        .map(|(decl, _)| &decl.name.0)
        .filter(|name| name.starts_with(b"{closure:"));
    let classes = unit
        .classes
        .iter()
        .filter(|class| !class.flags.is_anonymous)
        .map(|class| &class.name.0);
    closures
        .chain(classes)
        .filter(|name| name.iter().any(u8::is_ascii_uppercase))
        .map(|name| STRING_HEADER + name.len() + 1)
}

/// What `unit` declares laid in the order php's compile lays it, each
/// declaration at its place in the op array it is compiled in
/// ([`php_unit::Nested`]), which `main`, the main op array as it grows
/// where the request compiled the unit for itself, is one of: php
/// compiles the file's statements one by one (`zend_compile_top_stmt`,
/// `Z/Zend/zend_compile.c:11657`), a function where it stands
/// (`:11674`) and a class where it stands (`:11678`), its methods and
/// hooks inside it (`zend_compile_class_decl`'s `zend_compile_stmt`,
/// `:9351`); a function's `init_op_array` comes first
/// (`zend_compile_func_decl_ex`, `:8463`), then what
/// `zend_begin_func_decl` adds to the op array that declares it, a
/// pointer of its `dynamic_func_defs` (`zend_add_dynamic_func_def`,
/// `:8420`) and the `DECLARE_FUNCTION` or `DECLARE_LAMBDA_FUNCTION`
/// (`:8489`), then its body, every function and class written in it
/// compiled while its oplines stand (`:8604`), and its `pass_two` last
/// (`:8624`). So the variable and the constant a statement before a
/// function laid, and a method's 2048-byte oplines under the closure it
/// returns, stand at the declaration's `pass_two`, the compile's peak:
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3822-' . sprintf('%010d', getmypid()) . '.php';
/// foreach (['$abcdef = 1; function f() {} ', 'class C { function m() { return function () {}; } } ', '$abcdef = 1; class D { function m() { return function () {}; } } function g() {} '] as $code) {
///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
///     [$usage, $peak] = explode(' ', trim($fresh));
///     echo $peak - $usage, "\n";
/// }
/// unlink($f);
/// ```
/// ```expect process
/// 37664
/// 39376
/// 39616
/// ```
///
/// A function, class or dynamic definition no record reaches, as in a
/// unit built by hand, is laid after, in the unit's order.
fn in_order(
    req: &mut Request,
    unit: &Unit,
    main: Option<(&mut Growing, &mut Vec<Emalloc>)>,
) -> Result<Laid, Thrown> {
    let (main, hook_names) = match main {
        Some((main, names)) => (Some(main), names),
        None => (None, &mut Vec::new()),
    };
    let mut walk = Walk {
        unit,
        laid: Laid {
            classes: unit.classes.iter().map(|_| None).collect(),
            dynamic: declarers(unit)
                .into_iter()
                .map(|declarer| (Blocks::default(), declarer))
                .collect(),
            ..Laid::default()
        },
        functions: vec![false; unit.functions.len()],
        methods: places(unit.methods.iter().map(|m| (m.class, m.method, 0))),
        hooks: places(
            unit.hooks
                .iter()
                .map(|h| (h.class, h.property, hook_key(h.kind))),
        ),
        members: places(unit.classes.iter().zip(0..).flat_map(|(class, at)| {
            let count = |members: usize| 0..u32::try_from(members).unwrap_or(u32::MAX);
            (count(class.constants.len()).map(move |c| (at, c, 0)))
                .chain(count(class.properties.len()).map(move |p| (at, p, 1)))
        })),
        dynamic: vec![false; unit.dynamic.len()],
        hook_names: std::mem::take(hook_names),
        keys: main.is_some().then(Vec::new),
        interface_names: unit.classes.iter().map(|_| None).collect(),
    };
    let made = walk
        .nested(req, &unit.main, main)
        .and_then(|()| walk.rest(req));
    *hook_names = walk.hook_names;
    hook_names.extend(walk.keys.into_iter().flatten());
    hook_names.extend(walk.interface_names.into_iter().flatten());
    match made {
        Ok(()) => Ok(walk.laid),
        Err(thrown) => {
            walk.laid.free(req);
            Err(thrown)
        }
    }
}

/// Each dynamic definition's declarer: the main op array, another
/// dynamic definition, or else what the request's tables hold.
fn declarers(unit: &Unit) -> Vec<Declarer> {
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
    declarers
}

/// [`in_order`]'s walk: what it laid, and which functions, methods,
/// hooks and dynamic definitions it reached, a class being reached where
/// its slot is laid.
struct Walk<'u> {
    unit: &'u Unit,
    laid: Laid,
    functions: Vec<bool>,
    methods: Places,
    hooks: Places,
    /// The classes' constants, keyed 0, and properties, keyed 1.
    members: Places,
    dynamic: Vec<bool>,
    /// The strings of the names written for the unit's hooks, where the
    /// request compiled it, one given back as each hook has compiled.
    hook_names: Vec<Emalloc>,
    /// The keys of the top-level functions and the methods laid as each
    /// begins, where the request compiled the unit ([`Walk::key`]).
    keys: Option<Vec<Emalloc>>,
    /// Each class's `ce->interface_names` ([`Walk::interfaces`]).
    interface_names: Vec<Option<Emalloc>>,
}

/// A method's or a hook's place in its unit's table by its class, its
/// method or property and its kind, sorted, and whether it was reached.
type Places = Vec<((u32, u32, u8), usize, bool)>;

/// `keys`, each at its place, sorted.
fn places(keys: impl Iterator<Item = (u32, u32, u8)>) -> Places {
    let mut places: Places = keys.enumerate().map(|(at, key)| (key, at, false)).collect();
    places.sort_unstable();
    places
}

/// The member keyed `key`'s place, the first time it is asked for.
fn reach(places: &mut Places, key: (u32, u32, u8)) -> Option<usize> {
    let found = places.binary_search_by_key(&key, |(key, _, _)| *key).ok()?;
    let (_, at, reached) = places.get_mut(found)?;
    (!std::mem::replace(reached, true)).then_some(*at)
}

/// The member at `place` of `places`, where nothing reached it yet.
fn unreached(places: &mut Places, place: usize) -> Option<usize> {
    let (_, at, reached) = places.get_mut(place)?;
    (!std::mem::replace(reached, true)).then_some(*at)
}

/// A hook's kind as [`Places`] keys it.
fn hook_key(kind: php_unit::decl::HookKind) -> u8 {
    u8::from(kind == php_unit::decl::HookKind::Set)
}

/// Where an op array's blocks are held.
#[derive(Clone, Copy)]
enum Holder {
    Function,
    Member,
    Dynamic(usize),
}

impl Walk<'_> {
    /// What `code` declares, each at its place: `declarer`, `code` as it
    /// grows where it is laid, grown to where it stood as the
    /// declaration began.
    fn nested(
        &mut self,
        req: &mut Request,
        code: &OpArray,
        mut declarer: Option<&mut Growing>,
    ) -> Result<(), Thrown> {
        for nested in &code.nested {
            let stood = declarer
                .as_deref_mut()
                .map(|declarer| (declarer, nested.grown));
            match nested.declares {
                Declares::Class(at) => {
                    if let Some((declarer, grown)) = stood {
                        declarer.to(req, grown)?;
                    }
                    self.class(req, at as usize)?;
                }
                Declares::Function(at) => self.function(req, at as usize, stood)?,
                Declares::Method { class, method } => {
                    if let Some(at) = reach(&mut self.methods, (class, method, 0)) {
                        self.method(req, at, stood)?;
                    }
                }
                Declares::Hook {
                    class,
                    property,
                    kind,
                } => {
                    if let Some(at) = reach(&mut self.hooks, (class, property, hook_key(kind))) {
                        self.hook(req, at, stood)?;
                    }
                }
                Declares::Dynamic(at) => self.dynamic(req, at as usize, stood)?,
                Declares::Constant {
                    class,
                    constant: at,
                } => self.member(req, (class, at, 0), stood)?,
                Declares::Property {
                    class,
                    property: at,
                } => self.member(req, (class, at, 1), stood)?,
            }
        }
        Ok(())
    }

    /// What no record reached, in the unit's order.
    fn rest(&mut self, req: &mut Request) -> Result<(), Thrown> {
        for at in 0..self.unit.functions.len() {
            self.function(req, at, None)?;
        }
        for at in 0..self.unit.classes.len() {
            self.class(req, at)?;
        }
        for place in 0..self.methods.len() {
            if let Some(at) = unreached(&mut self.methods, place) {
                self.method(req, at, None)?;
            }
        }
        for place in 0..self.hooks.len() {
            if let Some(at) = unreached(&mut self.hooks, place) {
                self.hook(req, at, None)?;
            }
        }
        let members: Vec<_> = self.members.iter().map(|(key, _, _)| *key).collect();
        for key in members {
            self.member(req, key, None)?;
        }
        for at in 0..self.unit.dynamic.len() {
            self.dynamic(req, at, None)?;
        }
        Ok(())
    }

    /// The top-level function at `at`, once, its key laid as it begins
    /// ([`Walk::key`]).
    fn function(
        &mut self,
        req: &mut Request,
        at: usize,
        stood: Option<(&mut Growing, Grown)>,
    ) -> Result<(), Thrown> {
        let unit = self.unit;
        let (Some((decl, code)), Some(reached)) =
            (unit.functions.get(at), self.functions.get_mut(at))
        else {
            return Ok(());
        };
        if std::mem::replace(reached, true) {
            return Ok(());
        }
        self.key(req, &decl.name.0, false)?;
        self.body(req, code, Some((decl, false)), stood, Holder::Function)
    }

    /// The dynamic definition at `at`, once.
    fn dynamic(
        &mut self,
        req: &mut Request,
        at: usize,
        stood: Option<(&mut Growing, Grown)>,
    ) -> Result<(), Thrown> {
        let unit = self.unit;
        let (Some((decl, code)), Some(reached)) = (unit.dynamic.get(at), self.dynamic.get_mut(at))
        else {
            return Ok(());
        };
        if std::mem::replace(reached, true) {
            return Ok(());
        }
        self.body(req, code, Some((decl, false)), stood, Holder::Dynamic(at))
    }

    /// The class at `at`, once: its tables ([`class`]), [`Walk::interfaces`],
    /// the methods and hooks its declaration compiles recorded after it.
    fn class(&mut self, req: &mut Request, at: usize) -> Result<(), Thrown> {
        let Some(decl) = self.unit.classes.get(at) else {
            return Ok(());
        };
        if !matches!(self.laid.classes.get(at), Some(None)) {
            return Ok(());
        }
        let bodies = self.unit.methods.iter().filter(|m| m.class as usize == at);
        let blocks = class(req, decl, bodies.count())?;
        if let Some(slot) = self.laid.classes.get_mut(at) {
            *slot = Some(blocks);
        }
        let begun = decl.interfaces.len() - usize::from(decl.flags.stringable_added);
        self.interfaces(req, at, begun)
    }

    /// `ce->interface_names` of the class at `at` grown to its first `to`
    /// interfaces where the request compiled the unit, and the strings made
    /// for each: a written one's lowered name where it holds an upper-case
    /// letter (`zend_compile_implements`, `Z/Zend/zend_compile.c:9197`), an
    /// enum's `unitenum` and `backedenum` (`Z/Zend/zend_enum.c:184`, whose
    /// second `erealloc` of the names no peak sees, so it is one step
    /// here), and both of the `Stringable` a `__toString` adds as it begins
    /// (`add_stringable_interface`, `:8242`). The link gives them back
    /// (`Z/Zend/zend_inheritance.c:2302`), so they stand at the peak alone:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3901-' . sprintf('%010d', getmypid()) . '.php';
    /// foreach (['interface I {} class C implements I {} ', 'interface i {} class C implements i {} ', 'interface I {} interface J {} class C implements I, J {} ', 'enum E: int implements Countable { case A = 1; function count(): int { return 1; } } ', 'class C { function __toString(): string { return ""; } } '] as $code) {
    ///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
    ///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
    ///     [$usage, $peak] = explode(' ', trim($fresh));
    ///     echo $peak - $usage, "\n";
    /// }
    /// unlink($f);
    /// ```
    /// ```expect process
    /// 35792
    /// 35728
    /// 35872
    /// 37456
    /// 37328
    /// ```
    fn interfaces(&mut self, req: &mut Request, at: usize, to: usize) -> Result<(), Thrown> {
        let (Some(keys), Some(block), Some(decl)) = (
            &mut self.keys,
            self.interface_names.get_mut(at),
            self.unit.classes.get(at),
        ) else {
            return Ok(());
        };
        let from = block.as_ref().map_or(0, Emalloc::len) / CLASS_NAME;
        let to = to.min(decl.interfaces.len());
        let named = usize::from(to == decl.interfaces.len() && decl.flags.stringable_added);
        if to > from {
            regrow(req, block, to * CLASS_NAME)?;
        }
        for name in decl.interfaces.get(from..to).unwrap_or_default() {
            for _ in 0..named + usize::from(name.0.iter().any(u8::is_ascii_uppercase)) {
                keys.push(Emalloc::new(req, STRING_HEADER + name.0.len() + 1)?);
            }
        }
        Ok(())
    }

    /// The method at `at` of the unit's methods, and its row of its
    /// class's methods' table, which `zend_begin_method_decl` adds as the
    /// method begins (`Z/Zend/zend_compile.c:8318`): php lays the table
    /// at the first method, not with the class
    /// (`zend_initialize_class_data` lays no table's data), so a hook
    /// written before it compiles without it. A hooked property before a
    /// method, then after it:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3838-' . sprintf('%010d', getmypid()) . '.php';
    /// foreach (['class C { public $p { set { } } function m() {} } ', 'class C { function m() {} public $p { set { } } } '] as $code) {
    ///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
    ///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
    ///     [$usage, $peak] = explode(' ', trim($fresh));
    ///     echo $peak - $usage, "\n";
    /// }
    /// unlink($f);
    /// ```
    /// ```expect process
    /// 37392
    /// 37472
    /// ```
    fn method(
        &mut self,
        req: &mut Request,
        at: usize,
        stood: Option<(&mut Growing, Grown)>,
    ) -> Result<(), Thrown> {
        let unit = self.unit;
        let Some(method) = unit.methods.get(at) else {
            return Ok(());
        };
        self.class(req, method.class as usize)?;
        let declared = (unit.classes.get(method.class as usize))
            .and_then(|class| class.methods.get(method.method as usize))
            .map(|decl| (&decl.function, true));
        if let Some((decl, _)) = declared {
            self.key(req, &decl.name.0, true)?;
        }
        if let Some(Some(class)) = self.laid.classes.get(method.class as usize) {
            add_rows(req, class.table(Table::Methods), 1)?;
        }
        if declared.is_some_and(|(decl, _)| decl.name.0.eq_ignore_ascii_case(b"__tostring")) {
            self.interfaces(req, method.class as usize, usize::MAX)?;
        }
        self.body(req, &method.body, declared, stood, Holder::Member)
    }

    /// The key `name` is lowered to as its function or method begins,
    /// where the request compiled the unit and the name holds an
    /// upper-case letter, standing until the compile is over: a
    /// function's, a namespaced one's among them, made whatever the cache
    /// holds (`zend_begin_func_decl`'s `zend_string_tolower`,
    /// `Z/Zend/zend_compile.c:8395`), and a method's, abstract or not,
    /// where the engine's table holds no string of its bytes
    /// (`zend_begin_method_decl`'s, `:8315`), each the unit's own string
    /// only after the compile ([`Request::hold_unit_names`]):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3900-' . sprintf('%010d', getmypid()) . '.php';
    /// foreach (['function foo() {} ', 'function Foo() {} ', 'class C { function doit() {} } ', 'class C { function doIt() {} } ', 'class C { function Count() {} } ', 'interface I { function doIt(); } '] as $code) {
    ///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
    ///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
    ///     [$usage, $peak] = explode(' ', trim($fresh));
    ///     echo $peak - $usage, "\n";
    /// }
    /// unlink($f);
    /// ```
    /// ```expect process
    /// 37360
    /// 37360
    /// 37392
    /// 37392
    /// 37392
    /// 37392
    /// ```
    fn key(&mut self, req: &mut Request, name: &[u8], interned: bool) -> Result<(), Thrown> {
        if let Some(keys) = &mut self.keys
            && name.iter().any(u8::is_ascii_uppercase)
            && !(interned
                && req
                    .shared
                    .existing_interned(&name.to_ascii_lowercase())
                    .is_some())
        {
            keys.push(Emalloc::new(req, STRING_HEADER + name.len() + 1)?);
        }
        Ok(())
    }

    /// The hook at `at` of the unit's hooks.
    fn hook(
        &mut self,
        req: &mut Request,
        at: usize,
        stood: Option<(&mut Growing, Grown)>,
    ) -> Result<(), Thrown> {
        let unit = self.unit;
        let Some(hook) = unit.hooks.get(at) else {
            return Ok(());
        };
        let declared = (unit.classes.get(hook.class as usize))
            .and_then(|class| class.properties.get(hook.property as usize))
            .and_then(|property| property.hooks.as_deref())
            .and_then(|hooks| hooks.of(hook.kind))
            .map(|decl| (&decl.function, true));
        let made = self.body(req, &hook.body, declared, stood, Holder::Member);
        if let Some(name) = self.hook_names.pop() {
            name.free(req);
        }
        made
    }

    /// The constant, keyed 0, or the property, keyed 1, at `at` of the
    /// class at `class`, once, as php declares it where it is written, the
    /// declarer grown to where it `stood`: its class's `constants_table`
    /// and `properties_info` are laid at their first row
    /// (`zend_initialize_class_data`, `Z/Zend/zend_compile.c:2050`, lays
    /// neither's data), so a method written before them compiles without
    /// them. A constant's, a case's and a property's place, written after
    /// a method and before it, and an enum's:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3907-' . sprintf('%010d', getmypid()) . '.php';
    /// foreach (['class C { const K = 1; public $p = 1; function m() {} } ', 'class C { function m() {} const K = 1; public $p = 1; } ', 'class C { function m() {} public static $s = 1; } ', 'enum E { function m() {} case A; const K = self::A; } ', 'enum E { case A; const K = 1; function m() {} } '] as $code) {
    ///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
    ///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
    ///     [$usage, $peak] = explode(' ', trim($fresh));
    ///     echo $peak - $usage, "\n";
    /// }
    /// unlink($f);
    /// ```
    /// ```expect process
    /// 37392
    /// 36736
    /// 37056
    /// 36760
    /// 37256
    /// ```
    fn member(
        &mut self,
        req: &mut Request,
        (class, at, table): (u32, u32, u8),
        stood: Option<(&mut Growing, Grown)>,
    ) -> Result<(), Thrown> {
        if reach(&mut self.members, (class, at, table)).is_none() {
            return Ok(());
        }
        if let Some((declarer, grown)) = stood {
            declarer.to(req, grown)?;
        }
        self.class(req, class as usize)?;
        let unit = self.unit;
        let (Some(decl), Some(Some(blocks))) = (
            unit.classes.get(class as usize),
            self.laid.classes.get_mut(class as usize),
        ) else {
            return Ok(());
        };
        match table {
            0 => constant(req, blocks, decl, at as usize),
            _ => property(req, blocks, decl, at as usize),
        }
    }

    /// One op array laid into `holder`: [`init_op_array`], the op array
    /// that declares it grown to where it `stood` ([`begun`]), what it
    /// declares, and [`compiled`].
    fn body(
        &mut self,
        req: &mut Request,
        code: &OpArray,
        declared: Option<(&FunctionDecl, bool)>,
        stood: Option<(&mut Growing, Grown)>,
        holder: Holder,
    ) -> Result<(), Thrown> {
        let mut blocks = Blocks::default();
        let made = init_op_array(req, &mut blocks, code, declared).and_then(|mut growing| {
            begun(req, code, declared, stood, &mut growing)?;
            self.nested(req, code, Some(&mut growing))?;
            compiled(req, &mut blocks, code, growing)
        });
        match holder {
            Holder::Function => self.laid.tables.take(blocks),
            Holder::Member => self.laid.members.take(blocks),
            Holder::Dynamic(at) => match self.laid.dynamic.get_mut(at) {
                Some((held, _)) => held.take(blocks),
                None => blocks.free(req),
            },
        }
        made
    }
}

/// What stands between an op array's [`init_op_array`] and its body:
/// the op array declaring it grown to where it `stood`, and, for an
/// arrow function whose body reads a variable, `find_implicit_binds`'
/// `info.uses` (`Z/Zend/zend_compile.c:8151`), sized for its parameters
/// and laid at the first name it adds. The table stands over the
/// declarer's `BIND_LEXICAL`s (`compile_implicit_lexical_binds`,
/// `:8492`) and over the arrow function's parameters and binds, its
/// variables and its defaults' constants grown to hold them
/// (`zend_compile_params`, `zend_compile_implicit_closure_uses`,
/// `:8562`), and is given back before its body
/// (`zend_hash_destroy(&info.uses)`, `:8563`):
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3837-' . sprintf('%010d', getmypid()) . '.php';
/// foreach (['$f = fn () => $b; ', 'function f($a) { return fn () => $a; } ', '$f = fn ($a) => $a; ', '$f = fn ($a = 5) => $b; '] as $code) {
///     file_put_contents($f, "<?php {$code}echo memory_get_usage(), ' ', memory_get_peak_usage(), \"\\n\";");
///     $fresh = shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 " . escapeshellarg($f));
///     [$usage, $peak] = explode(' ', trim($fresh));
///     echo $peak - $usage, "\n";
/// }
/// unlink($f);
/// ```
/// ```expect process
/// 36568
/// 39376
/// 37024
/// 36776
/// ```
fn begun(
    req: &mut Request,
    code: &OpArray,
    declared: Option<(&FunctionDecl, bool)>,
    stood: Option<(&mut Growing, Grown)>,
    growing: &mut Growing,
) -> Result<(), Thrown> {
    let parameters = declared.map_or(&[][..], |(decl, _)| &decl.parameters[..]);
    let mut uses = Blocks::default();
    if code.implicit_uses > 0 {
        let size = u32::try_from(parameters.len()).unwrap_or(u32::MAX);
        uses.embedded.push(php_heap::EmbeddedArr::new(size));
    }
    let mut made = match uses.embedded.first() {
        Some(table) => add_rows(req, table.arr(), code.implicit_uses as usize),
        None => Ok(()),
    };
    if let (Ok(()), Some((declarer, grown))) = (&made, stood) {
        made = declarer.to(req, grown);
    }
    if made.is_ok() && !uses.is_empty() {
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let defaults = parameters.iter().filter(|p| p.default.expr().is_some());
        let prologue = Grown {
            constants: count(defaults.count()),
            vars: count(parameters.len() + code.bound_vars.len()),
            ..growing.grown
        };
        made = growing.to(req, prologue);
    }
    uses.free(req);
    made
}

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
    /// itself declared it, and where a file the cache keeps did, over
    /// `zend_lazy_class_load`'s copy ([`lazy_copy`]), which the request
    /// holds to its end; the request binds no other class of its own. The
    /// `Err` is the memory limit's fatal.
    pub(super) fn link_blocks(&mut self, class: ClassId) -> Result<(), Thrown> {
        let Some((unit, at)) = self.declared_row(class).map(|row| {
            (
                php_unit::PublishedUnit::as_ptr(row.unit()),
                row.at() as usize,
            )
        }) else {
            return Ok(());
        };
        let cached = !self.unit_strings.holds(unit);
        let mut blocks = match self.unit_strings.class_blocks(unit, at) {
            Some(blocks) => blocks,
            // The class as its unit wrote it, before its traits composed it.
            None if cached => match (self.declared_row(class))
                .and_then(|row| row.unit().classes.get(row.at() as usize))
                .map(Compiled::of)
            {
                Some(compiled) => lazy_copy(self, compiled)?,
                None => return Ok(()),
            },
            None => return Ok(()),
        };
        let linked = match self.linking(class) {
            Some(linking) => blocks.link(self, &linking),
            None => Ok(()),
        };
        match self.unit_strings.class_slot(unit, at) {
            Some(slot) => *slot = Some(blocks),
            None if cached => self.unit_strings.keep_copied(blocks.into_blocks()),
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
            let written = row.unit().classes.get(row.at() as usize)?;
            let constant = (written.constants.iter()).position(|c| *c.name.0 == *name)?;
            Some((
                php_unit::PublishedUnit::as_ptr(row.unit()),
                row.at() as usize,
                constant,
            ))
        }) else {
            return;
        };
        self.copy_released(unit, at, CopyOf::Constant(constant));
    }

    /// One holder fewer of the copy of what `of` names in the class at
    /// `at` of `unit`, and its blocks given back with the last.
    fn copy_released(&mut self, unit: *const Unit, at: usize, of: CopyOf) {
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
            let written = row.unit().classes.get(row.at() as usize)?;
            (written.properties.get(info.at as usize)).filter(|p| p.is_instance)?;
            Some((
                php_unit::PublishedUnit::as_ptr(row.unit()),
                row.at() as usize,
            ))
        }) else {
            return;
        };
        self.copy_released(unit, at, CopyOf::Default(info.at as usize));
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
                    php_unit::PublishedUnit::as_ptr(row.unit()),
                    row.at() as usize,
                    info.at as usize,
                ))
            })
            .collect();
        for (unit, at, property) in inherited {
            let Some(blocks) = (self.unit_strings.class_slot(unit, at)).and_then(Option::as_mut)
            else {
                continue;
            };
            if let Some(copied) =
                (blocks.copies.iter_mut()).find(|c| c.of == CopyOf::Default(property))
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
        let Some((decl, written)) = row.decl().zip(row.unit().classes.get(row.at() as usize))
        else {
            return Ok(());
        };
        let unit = php_unit::PublishedUnit::as_ptr(row.unit());
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
        let written = row.unit().classes.get(row.at() as usize)?;
        let decl = row.decl()?;
        let record = row.record()?;
        let abstracts = row
            .composed()
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
            methods: (record.methods().rows().len()).saturating_sub(composed + taken.methods),
            constants: (record.constants().rows().len())
                .saturating_sub(decl.constants.len() + taken.constants),
            properties: (record.properties().rows().len())
                .saturating_sub(decl.properties.len() + taken.properties),
        };
        let interfaces = self.interface_sizes(decl, written);
        let above = (decl.parent.as_ref()).and_then(|name| self.class_named(name.into()));
        let backed_above = |name: &[u8]| {
            let above = above.map_or(super::PlacedSlots::NONE, |above| {
                self.placed_properties(above)
            });
            (above.iter().filter_map(|info| self.property_decl(info))).any(|property| {
                *property.name.0 == *name
                    && property.visibility != Visibility::Private
                    && property.hooks.is_none()
            })
        };
        let hooked_slots = (written.properties.iter())
            .filter(|property| property.is_instance && property.is_virtual)
            .filter(|property| property.hooks.is_some() && backed_above(&property.name.0))
            .count();
        Some(Linking {
            first,
            trait_slots,
            parent,
            hooked_slots,
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
                methods: theirs.methods().rows().len(),
                constants: theirs.constants().rows().len(),
                properties: theirs.properties().rows().len(),
            },
            rows: Members {
                methods: self.taken(
                    pair,
                    Table::Methods,
                    (ours.methods(), theirs.methods()),
                    methods,
                ),
                constants: self.taken(
                    pair,
                    Table::Constants,
                    (ours.constants(), theirs.constants()),
                    constants,
                ),
                properties: self.taken(
                    pair,
                    Table::Properties,
                    (ours.properties(), theirs.properties()),
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
        (ours, theirs): (super::RowsOf<'_, C>, super::RowsOf<'_, C>),
        own: usize,
    ) -> usize {
        (theirs.iter())
            .filter(|row| {
                let Some(name) = self.row_name(parent, *row, table) else {
                    return false;
                };
                ours.find_at(name, |row| {
                    self.row_name(class, row, table).unwrap_or_default()
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
        let interfaces_of = |class: ClassId| -> Vec<ClassId> {
            (self.linked_class(class)).map_or_else(Vec::new, |record| record.interfaces().collect())
        };
        let theirs = (decl.parent.as_ref())
            .and_then(|name| self.class_named(name.into()))
            .map_or_else(Vec::new, interfaces_of);
        let own = written.interfaces.len().min(decl.interfaces.len());
        let mut sizes = Vec::new();
        let mut placed: Vec<ClassId> = theirs.clone();
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
                for above in extended {
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
