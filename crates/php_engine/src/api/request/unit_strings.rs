//! The strings a request-only unit owns, held as php's op arrays hold
//! theirs. A file's unit has its constants and names interned into the
//! engine's table as it is published ([`php_compile::intern_names`]), as
//! opcache's persist interns them. A unit compiled for the request alone,
//! an `eval` (`-r`'s among them), an uncached include or a program the
//! host handed over as its source, piped, is the request's: under opcache its compile's `zend_new_interned_string` is
//! `accel_new_interned_string_for_php` (`Z/ext/opcache/ZendAccelerator.c:577`),
//! which hands back the string it was given unless opcache's buffer holds
//! the bytes, so each constant is a plain string its op array owns and
//! `destroy_op_array` frees. And for an `eval` php's compile makes its
//! file's name and its functions', closures' and classes' names as strings
//! of the request, released with the op array the same way.
//!
//! So here the compile's home ([`super::memory::RequestStrings`]) makes
//! each constant a string of the request's with one owner, the unit's,
//! one per time the compile asks; an `eval`'s file name is made before
//! its compile, as php makes it, and its other names when the unit is
//! noted. The main op array's constants are given up when its
//! execution ends, as `destroy_op_array` frees an `eval`'s at its
//! return; a dynamic definition's, a closure's name, constants and
//! variables' names, with its op array at its last count
//! ([`OpArrayStrings`]); and the rest, what a declared function, method
//! or class carries, and the file's name, once nothing holds the unit.
//! A value copied out of a
//! constant takes an owner of its own (`ZVAL_COPY`), as a trace row or
//! a diagnostic that copies a name does (`ZVAL_STR_COPY`), so nothing a
//! unit made is held to the request's end for being made.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use php_heap::{Str, Zval};
use php_link::NameHasher;
use php_unit::decl::{FunctionDecl, TraitAdaptation};
use php_unit::{PublishedUnit, Unit, WeakUnit};

use super::Request;
use super::declared_bytes::{self, Blocks, ClassBlocks, Declared, Gone};
use crate::api::raising::Thrown;
use crate::api::values::{PhpString, Value};

/// The most names held already a unit's names are compared with one by
/// one, past which a set of them is made.
const SCANNED_NAMES: usize = 16;

/// The strings of the request-only units the request holds: each unit's,
/// one owner apiece, beside the unit they go with, the newest name string
/// held for each name, where a copy finds it, and the constants of the
/// compile under way. A name no living unit holds keeps its key with no
/// string, so the next unit of that name, an `eval` in a loop's, makes no
/// key of its own; and the vectors a unit's strings stood in are kept for
/// the next unit's.
#[derive(Default)]
pub(crate) struct UnitStrings {
    units: Vec<Held>,
    by_bytes: HashMap<Box<[u8]>, Option<PhpString>, NameHasher>,
    spare: Vec<Spare>,
    dead: Vec<PhpString>,
    dead_constants: Vec<Zval>,
    dead_found: Vec<PhpString>,
    /// The strings kept to the request's end for names no unit holds,
    /// one owner each ([`Request::kept_string`]).
    kept: HashMap<Box<[u8]>, PhpString, NameHasher>,
    /// How many living units hold their file's name as [`Held::file`],
    /// which `by_bytes` has no entry for.
    files: usize,
    dead_files: Vec<PhpString>,
    /// The blocks of what units declared that are over, for
    /// [`Request::release_unit_strings`] to give back.
    dead_blocks: Vec<Blocks>,
    /// What op arrays that are over gave back and [`UnitStrings::take`]
    /// has not yet taken: a dynamic definition's, at its last count.
    gone: Vec<Gone>,
    compiling: Compiling,
}

/// What the request-only units the request holds hold, read apart from
/// the compile under way: which units they are, and the strings a copy
/// of a name finds ([`UnitStrings::find`]).
#[derive(Clone, Copy)]
pub(super) struct UnitsHeld<'a> {
    units: &'a [Held],
    by_bytes: &'a HashMap<Box<[u8]>, Option<PhpString>, NameHasher>,
    kept: &'a HashMap<Box<[u8]>, PhpString, NameHasher>,
    files: usize,
}

impl<'a> UnitsHeld<'a> {
    /// The view over the fields of [`UnitStrings`] it reads.
    fn of(
        units: &'a [Held],
        by_bytes: &'a HashMap<Box<[u8]>, Option<PhpString>, NameHasher>,
        kept: &'a HashMap<Box<[u8]>, PhpString, NameHasher>,
        files: usize,
    ) -> UnitsHeld<'a> {
        UnitsHeld {
            units,
            by_bytes,
            kept,
            files,
        }
    }

    /// The string a request-only unit that lives holds for the name
    /// `bytes`, else the one the request keeps for it to its end; a
    /// view, no owner taken.
    pub(super) fn find(self, bytes: &[u8]) -> Option<PhpString> {
        (self.by_bytes.get(bytes).copied().flatten())
            .or_else(|| self.kept.get(bytes).copied())
            .or_else(|| {
                (self.files > 0)
                    .then(|| {
                        (self.units.iter().rev())
                            .find_map(|held| held.file.filter(|file| file.as_bytes() == bytes))
                    })
                    .flatten()
            })
    }

    /// Whether `unit` is one the request compiled for itself and holds,
    /// no cache keeping it: php's class of such a unit is mutable, its
    /// constants updated in place as they are evaluated.
    pub(super) fn holds(self, unit: *const Unit) -> bool {
        self.units
            .iter()
            .any(|held| std::ptr::eq(held.unit.as_ptr(), unit))
    }
}

/// The vectors one unit's strings and arrays stood in, emptied, kept
/// together for the next unit's: an `eval` in a loop takes them back
/// with one pop.
#[derive(Default)]
struct Spare {
    main: Vec<Zval>,
    constants: Vec<Zval>,
    names: Vec<PhpString>,
    found_main: Vec<PhpString>,
    found: Vec<PhpString>,
}

/// The strings one op array a request-only unit declared owns apart
/// from the unit, a dynamic definition's, which `destroy_op_array`
/// releases with it: its name (`op_array->function_name`) and its
/// lowered key where it is bound under one, its constants and its
/// variables' names, and the views of those `by_bytes` may point at.
#[derive(Default)]
pub(super) struct OpArrayStrings {
    names: Vec<PhpString>,
    constants: Vec<Zval>,
    found: Vec<PhpString>,
}

impl OpArrayStrings {
    /// Its names and the views of its constants a copy may find.
    fn names(&self) -> impl Iterator<Item = &PhpString> {
        self.names.iter().chain(&self.found)
    }
}

/// One request-only unit and the strings it owns.
struct Held {
    unit: WeakUnit,
    /// Whether it declared a function, a closure or a class, whose
    /// constants are apart from its main op array's.
    declares: bool,
    /// What holds what it declared past its main op array's end.
    past: Past,
    /// Whether its main op array's execution has ended, its constants
    /// given up.
    over: bool,
    /// The constant strings and arrays of its main op array,
    /// `op_array->literals` and the names `zend_compile` keeps beside
    /// them, given up when the main op array's execution ends
    /// ([`main_constants`]).
    main: Vec<Zval>,
    /// The constant strings and arrays of what it declared, given up
    /// once nothing holds it.
    constants: Vec<Zval>,
    /// Its op arrays' names ([`php_compile::op_array_names`]): those a
    /// file's compile made, and an `eval`'s, made as it is noted.
    names: Vec<PhpString>,
    /// Whether the names were made, which a unit's noting does once.
    named: bool,
    /// The file's name of an `eval` that declares nothing, the string its
    /// name was made as before its compile ([`Request::name_unit`]),
    /// held to the main op array's end with no entry of `by_bytes`, so
    /// the `eval` hashes nothing for it: a copy of it is found by
    /// [`UnitStrings::find`] looking over the few that stand.
    file: Option<PhpString>,
    /// The strings of `main` that stand for a name a declaration is read
    /// by, its variables', which [`UnitStrings::by_bytes`] may point at
    /// until the main op array ends: views, the owners being `main`'s.
    found_main: Vec<PhpString>,
    /// The strings of `constants` that stand for such a name, a declared
    /// function's variables' and a declared property's, pointed at while
    /// the unit lives: views, the owners being `constants`'.
    found: Vec<PhpString>,
    /// php's blocks for the op arrays and classes it declared
    /// ([`super::declared_bytes`]).
    declared: Declared,
}

/// What holds a request-only unit's declarations, and so its strings,
/// past its main op array's end.
enum Past {
    /// The closures over a function it declared that stand, each one
    /// more count of the op array, `(*op_array->refcount)++` in
    /// `zend_create_closure_ex`, given back by `zend_closure_free_storage`;
    /// and each function a `DECLARE_FUNCTION` bound, one count the
    /// function table keeps to the request's end
    /// ([`Request::bind_dynamic`]).
    /// A closure's function is in the request's table from its first
    /// closure on, and the loop keeps a share of the unit it last
    /// entered, so neither holds the unit's strings: only a closure does.
    Closures(usize),
    /// The request's tables, which hold a function or a class it
    /// declared, and the unit with it, while the unit lives.
    Tables,
}

/// The constant strings and arrays the compile under way has made, one
/// owner each, which become the unit's when the compile hands it over
/// ([`Request::hold_compiled`]) and are given up when it refuses
/// ([`Request::release_compiled`]).
#[derive(Default)]
pub(crate) struct Compiling {
    made: Vec<Zval>,
}

impl Compiling {
    /// `z`, a string or an array this compile made with one owner, kept
    /// for the unit.
    pub(super) fn add(&mut self, z: Zval) {
        self.made.push(z);
    }

    /// Whether `z` is one this compile made, its owner taken out to be
    /// the caller's: a constant array moving a string or an array the
    /// compile made into itself. The newest are looked at first, which
    /// are an array's own entries.
    pub(super) fn take(&mut self, z: Zval) -> bool {
        if !z.is_refcounted() {
            return false;
        }
        match self.made.iter().rposition(|made| made.is_same_value(z)) {
            Some(at) => {
                self.made.swap_remove(at);
                true
            }
            None => false,
        }
    }
}

/// The op array a string or an array a compile made is held by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// The main op array, given up as its execution ends.
    Main,
    /// A function, a method, a hook or a class, held while the unit
    /// lives.
    Tables,
    /// The dynamic definition at its place, given up with its op array.
    Definition(usize),
}

/// `made`, the strings and arrays a compile of `unit` made, each beside
/// the op array that holds it, as php's compile leaves each in the op
/// array whose constant or name it is. A string or an array an op
/// array's constants (`op_array->literals`) or static variables hold is
/// that op array's, and so is a string written as one of its variables'
/// names, which `zend_compile` keeps in `op_array->vars`, once for each
/// op array that has the variable; a declared property's or constant's
/// name is its class's; every other is the main op array's, its
/// constants, its variables' names and what a fold made on the way,
/// which php's compile frees as it goes. A constant is told by its
/// allocation, not its bytes, since two written alike are two strings.
///
/// An array a declaration keeps inside a tree that did not fold whole is
/// held where the tree is: the op array's constant expression for a
/// parameter's default, a promoted one's and an abstract method's among
/// them (`OpArray::const_exprs`), and the declaration for a class
/// constant, a default, a static, an attribute's argument and a `const`
/// ([`php_compile::arrays::declared_arrays`]). Each outlives the `eval` that declared it, read
/// after the heap is churned:
///
/// ```php
/// eval('#[Attribute(Attribute::TARGET_ALL)] class At { function __construct(public $v) {} } class Foo { function __construct(public $a) {} }');
/// eval('function f1($p = [[1, 2], Z]) { return $p; }');
/// eval('function f2($p = new Foo([3, 4])) { return $p->a; }');
/// eval('$GLOBALS["c1"] = function ($p = [[5], Z]) { return $p; };');
/// eval('class M { function m($p = [[6], Z]) { return $p; } static function s($p = new Foo([7])) { return $p->a; } function __construct(public $q = [[8], Z]) {} }');
/// eval('abstract class AB { abstract function a($p = [[9], Z]); } interface IF1 { const IC = [[10], Z]; function i($p = [[11], Z]); }');
/// eval('class D { const C = [[12], Z]; public $p = [[13], Z]; public static $s = [[14], Z]; }');
/// eval('trait T { public $tp = [[15], Z]; public static $ts = [[16], Z]; } class UT { use T; }');
/// eval('function f3() { static $s = [[17], Z]; return $s; }');
/// eval('#[At([[18], Z])] class AC { #[At([[19], Z])] const K = 1; #[At([[20], Z])] public $p; #[At([[21], Z])] function m(#[At([[22], Z])] $a) {} public $h { #[At([[23], Z])] get => 1; } }');
/// eval('enum EN { #[At([[24], Z])] case A; const L = [[25], Z]; }');
/// eval('#[At([[26], Z])] function f4() {}');
/// eval('$GLOBALS["c2"] = #[At([[27], Z])] fn () => 1;');
/// eval('define("Z3", 3); #[At([[28], Z])] const Y1 = [[29], Z3];');
/// eval('define("Z4", 4); const Y2 = [[30], Z4];');
/// define('Z', 'z');
/// $junk = array_fill(0, 2000, str_repeat("x", 100));
/// $o = [];
/// $o[] = f1(); $o[] = f2(); $o[] = $c1(); $o[] = (new M)->m(); $o[] = M::s(); $o[] = (new M)->q;
/// $o[] = (new ReflectionMethod('AB', 'a'))->getParameters()[0]->getDefaultValue();
/// $o[] = IF1::IC; $o[] = (new ReflectionMethod('IF1', 'i'))->getParameters()[0]->getDefaultValue();
/// $o[] = D::C; $o[] = (new D)->p; $o[] = D::$s;
/// $o[] = (new UT)->tp; $o[] = UT::$ts; $o[] = f3();
/// $at = fn ($r) => $r->getAttributes()[0]->newInstance()->v;
/// $o[] = $at(new ReflectionClass('AC')); $o[] = $at(new ReflectionClassConstant('AC', 'K')); $o[] = $at(new ReflectionProperty('AC', 'p'));
/// $o[] = $at(new ReflectionMethod('AC', 'm')); $o[] = $at((new ReflectionMethod('AC', 'm'))->getParameters()[0]);
/// $o[] = $at((new ReflectionProperty('AC', 'h'))->getHook(PropertyHookType::Get));
/// $o[] = $at(new ReflectionEnumUnitCase('EN', 'A')); $o[] = EN::L;
/// $o[] = $at(new ReflectionFunction('f4')); $o[] = $at(new ReflectionFunction($c2));
/// $o[] = $at(new ReflectionConstant('Y1')); $o[] = Y1; $o[] = Y2;
/// echo json_encode($o), "\n";
/// ```
/// ```expect regime=jit-off
/// [[[1,2],"z"],[3,4],[[5],"z"],[[6],"z"],[7],[[8],"z"],[[9],"z"],[[10],"z"],[[11],"z"],[[12],"z"],[[13],"z"],[[14],"z"],[[15],"z"],[[16],"z"],[[17],"z"],[[18],"z"],[[19],"z"],[[20],"z"],[[21],"z"],[[22],"z"],[[23],"z"],[[24],"z"],[[25],"z"],[[26],"z"],[[27],"z"],[[28],"z"],[[29],3],[[30],4]]
/// ```
fn main_constants(unit: &Unit, made: &mut Vec<Zval>) -> Vec<(Owner, Zval)> {
    let owner_of = |at: usize| function_owner(unit, at);
    let bodies = || {
        (unit.functions.iter().chain(&unit.dynamic))
            .map(|(_, body)| body)
            .chain(unit.methods.iter().map(|method| &method.body))
            .chain(unit.hooks.iter().map(|hook| &hook.body))
            .enumerate()
            .map(move |(at, body)| (owner_of(at), body))
    };
    let constants = || {
        bodies().flat_map(|(owner, body)| {
            (body.literals.iter().copied())
                .chain(body.bound_vars.iter().map(|bound| bound.value))
                .map(move |z| (owner, z))
        })
    };
    let mut held: Vec<(Str, Owner)> = constants()
        .filter_map(|(owner, z)| Some((z.as_str()?, owner)))
        .collect();
    held.sort_unstable_by(|a, b| a.0.bytes().cmp(b.0.bytes()));
    let mut arrays: Vec<(Zval, Owner)> = constants()
        .filter(|(_, z)| z.as_arr().is_some())
        .map(|(owner, z)| (z, owner))
        .collect();
    // And an array a constant expression the op array keeps whole holds
    // inside its tree (`IS_CONSTANT_AST`), a parameter's default that did
    // not fold among them, whose `RECV_INIT` holds the tree.
    for (owner, body) in bodies() {
        for expr in &body.const_exprs {
            php_compile::arrays::unit_arrays(expr, &mut |z| arrays.push((z, owner)));
        }
    }
    let mut declared: Vec<(Zval, Owner)> = (php_compile::arrays::declared_arrays(unit).into_iter())
        .map(|(z, holder)| match holder {
            php_compile::arrays::Holder::Unit => (z, Owner::Tables),
            php_compile::arrays::Holder::Dynamic(at) => (z, Owner::Definition(at)),
        })
        .collect();
    let names = declared_names(unit);
    let mut values = class_constant_values(unit);
    // Each dynamic definition's variables, one string each it takes, the
    // last definition's first, to be taken from the end.
    let mut vars: HashMap<&[u8], Vec<usize>, NameHasher> = HashMap::with_hasher(NameHasher);
    for (at, (_, body)) in unit.dynamic.iter().enumerate().rev() {
        for var in &body.vars {
            vars.entry(&*var.0).or_default().push(at);
        }
    }
    made.drain(..)
        .map(|z| {
            let owner = match z.as_str() {
                Some(s) => {
                    let bytes = s.bytes();
                    let from = held.partition_point(|(held, _)| held.bytes() < bytes);
                    let literal = (held.get(from..).unwrap_or_default().iter())
                        .take_while(|(held, _)| held.bytes() == bytes)
                        .find(|(held, _)| held.is_same(s))
                        .map(|(_, owner)| *owner);
                    literal.unwrap_or_else(|| {
                        let var = vars.get_mut(bytes).and_then(Vec::pop);
                        match var {
                            Some(at) => Owner::Definition(at),
                            None if names.contains(bytes) => Owner::Tables,
                            None => match values.get_mut(bytes) {
                                Some(left) if *left > 0 => {
                                    *left -= 1;
                                    Owner::Tables
                                }
                                _ => Owner::Main,
                            },
                        }
                    })
                }
                // A declaration's, one owner for each slot that holds
                // it, ahead of an op array's constant's.
                None => match (declared.iter()).position(|(held, _)| held.is_same_value(z)) {
                    Some(at) => declared.swap_remove(at).1,
                    None => (arrays.iter())
                        .find(|(held, _)| held.is_same_value(z))
                        .map_or(Owner::Main, |(_, owner)| *owner),
                },
            };
            (owner, z)
        })
        .collect()
}

/// The names `unit`'s declarations are read by
/// ([`php_compile::declared_names`]), as a set; the main op array's
/// variables are not among them.
fn declared_names(unit: &Unit) -> HashSet<Vec<u8>, NameHasher> {
    php_compile::declared_names(unit)
        .map(Cow::into_owned)
        .collect()
}

/// The string values `unit`'s classes declare their constants with, each
/// beside how many constants hold it: the string the compile made for
/// one, written or folded, is the class's from its declaration, as
/// php's `constants_table` holds the zval `zend_compile_class_const_decl`
/// put there, and its first read finds it ([`Request::declared_string`])
/// rather than make one. A value is told by its bytes, so the compile's
/// strings of those bytes are taken for it in the order made, as many as
/// constants hold it. An enum case's is not among them, its value
/// standing in the case's copied expression ([`declared_bytes::AstCopy`]).
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3263-class-values.inc';
/// file_put_contents($f, <<<'PHP'
/// <?php
/// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
/// m('class W {}');
/// foreach ([
///     'class S1 { const K = "abcdefghij5"; }',
///     '$GLOBALS["a"] = S1::K;',
///     'class S2 { const K = "abcdefghij" . 5; }',
///     '$GLOBALS["a"] = S2::K;',
///     'interface S3 { const K = "abcdefghij" . "5"; }',
///     '$GLOBALS["a"] = S3::K;',
/// ] as $c) echo m($c), "  ", $c, "\n";
/// PHP);
/// include $f;
/// unlink($f);
/// ```
/// ```expect regime=jit-off
/// 536  class S1 { const K = "abcdefghij5"; }
/// 888  $GLOBALS["a"] = S1::K;
/// 536  class S2 { const K = "abcdefghij" . 5; }
/// 0  $GLOBALS["a"] = S2::K;
/// 536  interface S3 { const K = "abcdefghij" . "5"; }
/// 0  $GLOBALS["a"] = S3::K;
/// ```
///
/// A class name a constant resolves to is such a value: `B10::class` and
/// `Bxyz::class` a string of their own, as php's scanner made one for the
/// name written, even where it names the class itself, whose own name is
/// another string ([`Request::hold_compiled`] leaves the value to the
/// constant); `self::class` the class's name itself (`ZVAL_STR_COPY`):
///
/// ```php
/// $f = sys_get_temp_dir() . '/w3268-class-names.inc';
/// file_put_contents($f, <<<'PHP'
/// <?php
/// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
/// m('class W {}'); m('$q0 = 1;');
/// foreach ([
///     'class B9 { const K = "abcdefghij"; }',
///     'class B10 { const K = B10::class; }',
///     'class B11 { const K = Bxyz::class; }',
///     'class B12 { const K = self::class; }',
///     '$GLOBALS["a"] = B10::K;',
///     '$GLOBALS["b"] = [B9::K, B11::K, B12::K];',
/// ] as $c) echo m($c), "  ", $c, "\n";
/// PHP);
/// include $f;
/// unlink($f);
/// ```
/// ```expect regime=jit-off
/// 536  class B9 { const K = "abcdefghij"; }
/// 528  class B10 { const K = B10::class; }
/// 528  class B11 { const K = Bxyz::class; }
/// 496  class B12 { const K = self::class; }
/// 512  $GLOBALS["a"] = B10::K;
/// 216  $GLOBALS["b"] = [B9::K, B11::K, B12::K];
/// ```
fn class_constant_values(unit: &Unit) -> HashMap<&[u8], usize, NameHasher> {
    let mut values = HashMap::with_hasher(NameHasher);
    let constants = (unit.classes.iter())
        .flat_map(|class| &class.constants)
        .filter(|constant| !constant.is_case);
    for constant in constants {
        if let php_unit::decl::ConstExpr::Str(value) = &constant.value {
            *values.entry(&**value).or_insert(0) += 1;
        }
    }
    values
}

/// `by_bytes`'s entry for `bytes` pointed at `s`.
fn point(by_bytes: &mut HashMap<Box<[u8]>, Option<PhpString>, NameHasher>, s: PhpString) {
    match by_bytes.get_mut(s.as_bytes()) {
        Some(named) => *named = Some(s),
        None => {
            by_bytes.insert(s.as_bytes().into(), Some(s));
        }
    }
}

/// The op array the name at `index` of [`php_compile::op_array_names`]
/// of `unit` is the name of, in its order: the file's, each function's
/// and each dynamic definition's, then each method's, hook's and
/// class's.
fn name_owner(unit: &Unit, index: usize) -> Owner {
    index
        .checked_sub(1)
        .map_or(Owner::Tables, |at| function_owner(unit, at))
}

/// The op array that is the function at `at` of the unit's functions
/// followed by its dynamic definitions: a definition's own, or the
/// unit's tables for a function and for any place past them.
fn function_owner(unit: &Unit, at: usize) -> Owner {
    match at.checked_sub(unit.functions.len()) {
        Some(at) if at < unit.dynamic.len() => Owner::Definition(at),
        _ => Owner::Tables,
    }
}

impl Held {
    /// The names `owner` holds: a dynamic definition's own, the unit's
    /// for any other, a function's, a method's or a class's.
    fn names_of(&mut self, owner: Owner) -> &mut Vec<PhpString> {
        match owner {
            Owner::Definition(at) => match self.declared.definition_strings(at) {
                Some(own) => &mut own.names,
                None => &mut self.names,
            },
            Owner::Main | Owner::Tables => &mut self.names,
        }
    }
}

impl UnitStrings {
    /// The string `unit`, a unit the request compiled for itself, holds
    /// as its file's name `file`: [`Held::file`], or its name among the
    /// names of one that declares.
    fn file_of(&self, unit: *const Unit, file: &[u8]) -> Option<PhpString> {
        let held = (self.units.iter()).find(|held| std::ptr::eq(held.unit.as_ptr(), unit))?;
        (held.file.into_iter().chain(held.names.iter().copied()))
            .find(|name| name.as_bytes() == file)
    }

    /// The string a request-only unit that lives holds for the name
    /// `bytes`, else the one the request keeps for it to its end
    /// ([`Request::kept_string`]); a view, no owner taken.
    pub(super) fn find(&self, bytes: &[u8]) -> Option<PhpString> {
        self.units_held().find(bytes)
    }

    /// What the request's units hold, read apart from the compile under
    /// way ([`UnitsHeld`]).
    fn units_held(&self) -> UnitsHeld<'_> {
        UnitsHeld::of(&self.units, &self.by_bytes, &self.kept, self.files)
    }

    /// The compile under way, which the home its constants are made in
    /// keeps them in, beside what the request's units hold, borrowed apart
    /// so a compile can read a class's constant strings while it makes
    /// its own ([`super::Request::compiling`]).
    pub(super) fn compiling_beside(&mut self) -> (&mut Compiling, UnitsHeld<'_>) {
        let held = UnitsHeld::of(&self.units, &self.by_bytes, &self.kept, self.files);
        (&mut self.compiling, held)
    }

    /// `s`, a string of the request's with one owner, the kept string
    /// for `bytes` to the request's end ([`Request::kept_string`]).
    pub(super) fn keep(&mut self, bytes: &[u8], s: PhpString) {
        self.kept.insert(bytes.into(), s);
    }

    /// The strings that are over taken out: those of each unit nothing
    /// holds any longer, and those of `ended`, a unit whose main op
    /// array has finished, whatever share of it the loop still holds for
    /// the opline that returned: its main op array's constants, and,
    /// where it bound no function or class and no closure over what it
    /// declared stands, the rest.
    /// Each name one of those strings stood for is pointed at an equal
    /// string a living unit holds, or at none.
    fn take(&mut self, ended: Option<*const Unit>) -> (Vec<PhpString>, Vec<Zval>) {
        let mut dead = std::mem::take(&mut self.dead);
        let mut constants = std::mem::take(&mut self.dead_constants);
        let mut found = std::mem::take(&mut self.dead_found);
        let mut files = std::mem::take(&mut self.dead_files);
        let spare = &mut self.spare;
        let gone = &mut self.gone;
        self.units.retain_mut(|held| {
            let alive = held.unit.strong_count() > 0;
            let ends = ended == Some(held.unit.as_ptr());
            if alive && !ends && !held.over {
                return true;
            }
            if !std::mem::replace(&mut held.over, true) && held.declared.laid() {
                held.declared.main_over(gone);
            }
            constants.append(&mut held.main);
            found.append(&mut held.found_main);
            if alive && !matches!(held.past, Past::Closures(0)) {
                return true;
            }
            std::mem::take(&mut held.declared).all(gone);
            dead.append(&mut held.names);
            files.extend(held.file.take());
            constants.append(&mut held.constants);
            found.append(&mut held.found);
            spare.push(Spare {
                main: std::mem::take(&mut held.main),
                constants: std::mem::take(&mut held.constants),
                names: std::mem::take(&mut held.names),
                found_main: std::mem::take(&mut held.found_main),
                found: std::mem::take(&mut held.found),
            });
            false
        });
        // What the op arrays that are over owned: their blocks, given
        // back by the caller, and their strings.
        for Gone { blocks, strings } in self.gone.drain(..) {
            self.dead_blocks.push(blocks);
            let OpArrayStrings {
                mut names,
                constants: mut owned,
                found: mut views,
            } = strings;
            dead.append(&mut names);
            constants.append(&mut owned);
            found.append(&mut views);
        }
        for name in dead.iter().chain(&found) {
            let bytes = name.as_bytes();
            let Some(held) = self.by_bytes.get_mut(bytes) else {
                continue;
            };
            if held.is_some_and(|held| held.heap().is_same(name.heap())) {
                *held = self
                    .units
                    .iter()
                    .rev()
                    .flat_map(|held| {
                        (held.names.iter().chain(&held.found).chain(&held.found_main))
                            .chain(held.declared.strings().flat_map(OpArrayStrings::names))
                    })
                    .find(|held| held.as_bytes() == bytes)
                    .copied();
            }
        }
        found.clear();
        self.dead_found = found;
        // A file's name `by_bytes` has no entry for is given up with no
        // name to point anew.
        self.files = self.files.saturating_sub(files.len());
        dead.append(&mut files);
        self.dead_files = files;
        (dead, constants)
    }

    /// Whether the request holds no request-only unit.
    pub(super) fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// The blocks of the class at `at` of the unit at `unit`'s classes,
    /// taken out while it links, where the unit laid them.
    pub(super) fn class_blocks(&mut self, unit: *const Unit, at: usize) -> Option<ClassBlocks> {
        self.class_slot(unit, at)?.take()
    }

    /// The place the blocks of the class at `at` of the unit at `unit`'s
    /// classes stand in.
    pub(super) fn class_slot(
        &mut self,
        unit: *const Unit,
        at: usize,
    ) -> Option<&mut Option<ClassBlocks>> {
        self.held_mut(unit)?.declared.class_slot(at)
    }

    /// The entry for the unit at `unit`, where the request holds one.
    fn held_mut(&mut self, unit: *const Unit) -> Option<&mut Held> {
        let at = self.position(unit)?;
        self.units.get_mut(at)
    }

    /// The place of the entry for the unit at `unit`, where the request
    /// holds one.
    fn position(&self, unit: *const Unit) -> Option<usize> {
        (self.units.iter()).position(|held| std::ptr::eq(held.unit.as_ptr(), unit))
    }

    /// The entry for `unit`, laid down where there is none.
    fn held(&mut self, unit: &PublishedUnit) -> usize {
        if let Some(at) = self
            .units
            .iter()
            .position(|held| std::ptr::eq(held.unit.as_ptr(), PublishedUnit::as_ptr(unit)))
        {
            return at;
        }
        let spare = self.spare.pop().unwrap_or_default();
        self.units.push(Held {
            unit: PublishedUnit::downgrade(unit),
            declares: !(unit.functions.is_empty()
                && unit.dynamic.is_empty()
                && unit.classes.is_empty()),
            past: if unit.functions.is_empty() && unit.classes.is_empty() {
                Past::Closures(0)
            } else {
                Past::Tables
            },
            over: false,
            main: spare.main,
            constants: spare.constants,
            names: spare.names,
            named: false,
            file: None,
            found_main: spare.found_main,
            found: spare.found,
            declared: Declared::default(),
        });
        self.units.len() - 1
    }
}

impl Request {
    /// Whether `class` is one a unit compiled for this request alone
    /// declared, an `eval`'s or an uncached include's, whose constant
    /// strings are its own rather than the engine's.
    pub(crate) fn declared_for_the_request(&self, class: crate::api::values::ClassId) -> bool {
        (self.class_lookup()).declared_for_the_request(self.unit_strings.units_held(), class)
    }

    /// `zend_get_executed_filename_ex()` for `file`, the bytes the
    /// executing frame's file is named by, with one more owner: the
    /// string the innermost frame's unit holds as its file's name where
    /// the request compiled that unit for itself, as php's
    /// `op_array->filename` is that unit's own string, so what keeps it
    /// keeps that `eval`'s name and no other's of the same bytes; else
    /// [`Request::declared_string`]'s. Where the body started and a
    /// constant an `eval` defines each keep the `eval`'s own name, though
    /// a class an `eval` of the same line declared holds one of the
    /// same bytes (the file under the recipe's `TMPDIR`):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3259-own-name.inc';
    /// file_put_contents($f, "<?php\neval('class Kh {}'); \$m = memory_get_usage(); eval('echo \"a\";'); \$d = memory_get_usage() - \$m; eval('class Kc {}'); \$n = memory_get_usage(); eval('const KC = 1;'); \$e = memory_get_usage() - \$n; echo \"\\n\$d \$e\\n\";\n");
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// a
    /// 144 184
    /// ```
    pub(crate) fn executed_file_string(
        &mut self,
        file: &[u8],
    ) -> Result<crate::api::values::Owner<PhpString>, Thrown> {
        let unit = (self.innermost_frame().and_then(crate::exec::FrameAt::unit))
            .map(|(unit, _)| PublishedUnit::as_ptr(unit));
        match unit.and_then(|unit| self.unit_strings.file_of(unit, file)) {
            Some(own) => Ok(crate::api::values::Owner::take(self, own)),
            None => self.declared_string(file),
        }
    }

    /// The constant strings the compile just made, given to `unit`, the
    /// unit it made: its main op array's held while it runs, the rest
    /// while the unit lives ([`main_constants`]). So an `eval` that
    /// declares a function or returns a closure costs what one that
    /// reads no constant costs, as php's main op array's constants go
    /// with it at the `eval`'s return:
    ///
    /// ```php
    /// eval('function w1() {} function w2() {}');
    /// $m = memory_get_usage(); eval('function f1() {} strlen("abcdefghijklmnopqrstuvwxyz");'); $a = memory_get_usage() - $m;
    /// $m = memory_get_usage(); eval('function f2() {}'); $b = memory_get_usage() - $m;
    /// $m = memory_get_usage(); $g = eval('strlen("abcdefghijklmnopqrstuvwxyz"); return function () { return 1; };'); $c = memory_get_usage() - $m;
    /// $m = memory_get_usage(); $h = eval('return function () { return 1; };'); $d = memory_get_usage() - $m;
    /// echo $a - $b, " ", $c - $d, "\n";
    /// echo f1(), $g(), eval('function f3() { return "abcdefghijklmnopq"; } return "rstuvwxyz";'), f3(), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 0 0
    /// 1rstuvwxyzabcdefghijklmnopq
    /// ```
    pub(crate) fn hold_compiled(&mut self, unit: &PublishedUnit) {
        let strings = &mut self.unit_strings;
        let at = strings.held(unit);
        let made = &mut strings.compiling.made;
        let Some(held) = strings.units.get_mut(at) else {
            return;
        };
        // A file's compile made its op arrays' names
        // ([`php_compile::intern_names`]), which a copy of one finds
        // ([`Request::declared_string`]) while the unit lives; an
        // `eval`'s made none, and they are made as it is noted
        // ([`Request::hold_unit_names`]). A compile that made no string
        // has none of them to find.
        let values = class_constant_values(unit);
        if !made.is_empty() {
            let mut seen = HashSet::with_hasher(NameHasher);
            for (index, name) in php_compile::op_array_names(unit).enumerate() {
                let of_name = |z: &Zval| z.as_str().is_some_and(|s| s.bytes() == name);
                // A class constant's value of the name's bytes is its
                // class's, never the name: `class B10 { const K =
                // B10::class; }` holds two strings, as php's scanner made
                // one for each token.
                let reserved = values.get(name).copied().unwrap_or(0);
                if made.iter().filter(|z| of_name(z)).count() <= reserved {
                    continue;
                }
                let Some(at) = made.iter().rposition(of_name) else {
                    continue;
                };
                if !seen.insert(name) {
                    continue;
                }
                let Some(s) = made.swap_remove(at).as_str().map(PhpString::of) else {
                    continue;
                };
                held.names_of(name_owner(unit, index)).push(s);
                point(&mut strings.by_bytes, s);
            }
        }
        // The strings that stand for a name a declaration is read by,
        // which a copy of one finds while what holds them lives: the main
        // op array's variables' until it ends, the declarations' while
        // the unit lives, a dynamic definition's while its op array does.
        let main_vars: HashSet<&[u8], NameHasher> =
            unit.main.vars.iter().map(|var| &*var.0).collect();
        let owned = if held.declares {
            main_constants(unit, made)
        } else {
            made.drain(..).map(|z| (Owner::Main, z)).collect()
        };
        let names = if owned.iter().any(|(owner, _)| *owner != Owner::Main) {
            declared_names(unit)
        } else {
            HashSet::default()
        };
        // What a declaration finds by its bytes: a name it is read by, or
        // a class constant's value.
        let found =
            |s: &PhpString| names.contains(s.as_bytes()) || values.contains_key(s.as_bytes());
        for (owner, z) in owned {
            let name = z.as_str().map(PhpString::of);
            match owner {
                Owner::Main => {
                    held.main.push(z);
                    if let Some(s) = name.filter(|s| main_vars.contains(s.as_bytes())) {
                        held.found_main.push(s);
                        point(&mut strings.by_bytes, s);
                    }
                }
                Owner::Tables => {
                    held.constants.push(z);
                    if let Some(s) = name.filter(found) {
                        held.found.push(s);
                        point(&mut strings.by_bytes, s);
                    }
                }
                Owner::Definition(at) => {
                    let Some(own) = held.declared.definition_strings(at) else {
                        held.constants.push(z);
                        continue;
                    };
                    own.constants.push(z);
                    if let Some(s) = name.filter(|s| names.contains(s.as_bytes())) {
                        own.found.push(s);
                        point(&mut strings.by_bytes, s);
                    }
                }
            }
        }
    }

    /// The constant strings a compile the compiler refused made, given up
    /// as `zend_compile`'s refusal destroys the op array it was making.
    pub(crate) fn release_compiled(&mut self) {
        let mut made = std::mem::take(&mut self.unit_strings.compiling.made);
        for z in made.drain(..) {
            self.release(Value::of_slot(z));
        }
        self.unit_strings.compiling.made = made;
    }

    /// The name an `eval`'s string was compiled under
    /// ([`Request::eval_string`]), made before its compile as
    /// `zend_make_compiled_string_description` makes it, held by `unit`
    /// as its file's name from here, one owner given up with the unit's
    /// other names, as `destroy_op_array` releases `op_array->filename`:
    /// for a unit that declares nothing, which has no other name, as its
    /// [`Held::file`], and for one that declares, among its names. A unit
    /// that holds a name of those bytes already leaves the string to be
    /// given up now.
    pub(crate) fn name_unit(&mut self, unit: &PublishedUnit, name: PhpString) {
        let strings = &mut self.unit_strings;
        let at = strings.held(unit);
        let Some(held) = strings.units.get_mut(at).filter(|held| {
            held.file.is_none()
                && !(held.names.iter()).any(|held| held.as_bytes() == name.as_bytes())
        }) else {
            self.release(Value::string(name));
            return;
        };
        if held.declares {
            held.names.push(name);
            point(&mut strings.by_bytes, name);
        } else {
            held.file = Some(name);
            held.named = true;
            strings.files += 1;
        }
    }

    /// `zend_compile`'s names for a request-only unit: the file's name
    /// and each name [`php_compile::op_array_names`] lists, made as
    /// strings of the request and held while the unit lives. The `Err`
    /// is the memory limit. An enum's `cases`, `from` and `tryFrom` are
    /// php's internal functions, registered as it links under php's
    /// known strings, so none of their names is made:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W { function __toString() { return ""; } }');
    /// echo m('enum E1 { case A; case B; }'), "\n";
    /// echo m('enum E2: int { case A = 1; }'), "\n";
    /// echo m('enum E3: string { case A = "a"; function Label() { return 1; } }'), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 1344
    /// 1288
    /// 1464
    /// ```
    ///
    /// A class's name is interned where opcache holds its bytes, a file
    /// the cache keeps naming the class (`zend_new_interned_string` in
    /// `zend_compile_class_decl`), so the `eval` that declares it makes
    /// no string for it; a function's name is made whatever the cache
    /// holds:
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('w3259-interned-name.inc', <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// echo m('class Fooq {}'), "\n";
    /// echo m('class Barq {}'), "\n";
    /// $o = new Fooq;
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 112
    /// 176
    /// ```
    pub(crate) fn hold_unit_names(&mut self, unit: &PublishedUnit) -> Result<(), Thrown> {
        let at = self.unit_strings.held(unit);
        let Some(held) = self.unit_strings.units.get_mut(at) else {
            return Ok(());
        };
        if std::mem::replace(&mut held.named, true) {
            return Ok(());
        }
        // The names held already: the file's, made before the compile
        // ([`Request::name_unit`]) and shared by every op array
        // (`zend_string_copy`), and those the compile made as constants
        // ([`Request::hold_compiled`]), a bound function's lowered name
        // among them, which php's compile makes one string with its name.
        let held_before = held.names.len();
        let file = held.file;
        // A set of them where they are many, an `eval` of many functions.
        let before: Option<HashSet<Vec<u8>, NameHasher>> =
            (held_before > SCANNED_NAMES).then(|| {
                held.names
                    .iter()
                    .map(|made| made.as_bytes().to_vec())
                    .collect()
            });
        // A class's name, the last of them, is `zend_new_interned_string`'s
        // in `zend_compile_class_decl`: the interned string where opcache
        // holds the bytes, no string of the unit's.
        let first_class = php_compile::op_array_names(unit)
            .count()
            .saturating_sub(unit.classes.len());
        for (index, name) in php_compile::op_array_names(unit).enumerate() {
            let owner = name_owner(unit, index);
            let interned = index >= first_class && self.shared.existing_interned(name).is_some();
            let made = interned
                || file.is_some_and(|file| file.as_bytes() == name)
                || match owner {
                    // A definition's own, which its compile made with it.
                    Owner::Definition(_) => (self.unit_strings.units.get_mut(at))
                        .and_then(|held| held.names_of(owner).first())
                        .is_some_and(|made| made.as_bytes() == name),
                    Owner::Main | Owner::Tables => before.as_ref().map_or_else(
                        || {
                            (self.unit_strings.units.get(at))
                                .and_then(|held| held.names.get(..held_before))
                                .is_some_and(|names| {
                                    names.iter().any(|made| made.as_bytes() == name)
                                })
                        },
                        |before| before.contains(name),
                    ),
                };
            // Every other is made once for each op array or class: two
            // closures on one line are two `{closure:…}` strings.
            if !made {
                self.hold_name(at, owner, name)?;
            }
        }
        // And the lowered key each is bound under where it is not its
        // name (`zend_string_tolower`): a function's, one a
        // `DECLARE_FUNCTION` binds among them, a class's, which the class
        // table keeps however it was bound, and a method's, the class's
        // `function_table` key. A closure has none.
        let functions = (unit.functions.iter()).map(|(decl, _)| (Owner::Tables, &decl.name));
        let dynamic = (unit.dynamic.iter().enumerate())
            .map(
                |(at, (decl, _)): (usize, &(FunctionDecl, php_unit::OpArray))| {
                    (Owner::Definition(at), &decl.name)
                },
            )
            .filter(|(_, name)| !name.0.starts_with(b"{"));
        let lowered = functions
            .chain(dynamic)
            .chain(
                unit.classes
                    .iter()
                    .map(|class| (Owner::Tables, &class.name)),
            )
            .chain((unit.classes.iter()).flat_map(|class| {
                (php_unit::written_methods(class).iter())
                    .map(|method| (Owner::Tables, &method.function.name))
            }))
            .filter(|(_, name)| name.0.iter().any(u8::is_ascii_uppercase));
        // `zend_new_interned_string` hands back the interned string for a
        // lowered name opcache holds, `count` or `__tostring` or one a
        // cached file's constants hold, and the one made goes
        // (`accel_new_interned_string_for_php`).
        for (owner, name) in lowered {
            let lower = name.0.to_ascii_lowercase();
            if self.shared.existing_interned(&lower).is_none() {
                self.hold_name(at, owner, &lower)?;
            }
        }
        // And each trait a class uses, `zend_compile_use_trait`'s
        // `trait_names`: the name as it resolved, and its lowered form
        // where it is not its name (`zend_string_tolower`), neither
        // interned.
        for name in unit.classes.iter().flat_map(|class| &class.traits) {
            self.hold_name(at, Owner::Tables, &name.0)?;
            if name.0.iter().any(u8::is_ascii_uppercase) {
                self.hold_name(at, Owner::Tables, &name.0.to_ascii_lowercase())?;
            }
        }
        // And each name an adaptation writes, a string of its own for
        // each (`zend_compile_method_ref`, `zend_compile_trait_precedence`,
        // `zend_compile_trait_alias`).
        for adaptation in unit.classes.iter().flat_map(|class| &class.adaptations) {
            let names: Vec<&php_unit::decl::Name> = match adaptation {
                TraitAdaptation::Insteadof {
                    trait_name,
                    method,
                    excluded,
                } => [method, trait_name].into_iter().chain(excluded).collect(),
                TraitAdaptation::As { method, alias, .. } => std::iter::once(&method.method)
                    .chain(&method.trait_name)
                    .chain(alias)
                    .collect(),
            };
            for name in names {
                self.hold_name(at, Owner::Tables, &name.0)?;
            }
        }
        Ok(())
    }

    /// `name`, a name a class of the unit at `unit` takes as it links,
    /// made as a string of the request's and held with the unit's names,
    /// unless opcache holds the bytes (`zend_new_interned_string`).
    pub(super) fn hold_class_name(&mut self, unit: *const Unit, name: &[u8]) -> Result<(), Thrown> {
        if self.shared.existing_interned(name).is_some() {
            return Ok(());
        }
        match self.unit_strings.position(unit) {
            Some(at) => self.hold_name(at, Owner::Tables, name),
            None => Ok(()),
        }
    }

    /// `name` made as a string of the request's, held by the unit at
    /// `at` with its names, or with the names of its op array `owner`
    /// names.
    fn hold_name(&mut self, at: usize, owner: Owner, name: &[u8]) -> Result<(), Thrown> {
        let made = self.string(name)?;
        point(&mut self.unit_strings.by_bytes, made);
        if let Some(held) = self.unit_strings.units.get_mut(at) {
            held.names_of(owner).push(made);
        }
        Ok(())
    }

    /// `destroy_op_array`'s release of the strings of each request-only
    /// unit that is over, its constants and its names: those of `ended`'s
    /// main op array, whose frame was just popped, the rest of it too
    /// where it bound no function or class and no closure over what it
    /// declared stands (a closure holds the op array its function is,
    /// as php's holds the op array's refcount), and every unit nothing
    /// holds any longer.
    pub(crate) fn release_unit_strings(&mut self, ended: Option<&PublishedUnit>) {
        if self.unit_strings.units.is_empty() {
            return;
        }
        let ended = ended.map(PublishedUnit::as_ptr);
        let (mut dead, mut constants) = self.unit_strings.take(ended);
        // `destroy_op_array`'s and `destroy_zend_class`'s blocks.
        if !self.unit_strings.dead_blocks.is_empty() {
            let mut blocks = std::mem::take(&mut self.unit_strings.dead_blocks);
            for blocks in blocks.drain(..) {
                blocks.free(self);
            }
            self.unit_strings.dead_blocks = blocks;
        }
        // `zend_string_release`: a string is no possible root, and
        // destroying one runs nothing.
        for s in dead.drain(..) {
            self.heap.release_string(s.heap());
        }
        for z in constants.drain(..) {
            self.release(Value::of_slot(z));
        }
        self.unit_strings.dead = dead;
        self.unit_strings.dead_constants = constants;
    }

    /// `zend_create_closure_ex`'s `(*op_array->refcount)++`: one more
    /// closure over `function`, which holds the strings of the
    /// request-only unit that declared it.
    pub(crate) fn count_closure(&mut self, function: crate::api::call::Function) {
        let Some((unit, dynamic)) = self.closure_unit(function) else {
            return;
        };
        let Some(held) = self.unit_strings.units.get_mut(unit) else {
            return;
        };
        if let Some(at) = dynamic {
            held.declared.count_closure(at);
        }
        if let Past::Closures(count) = &mut held.past {
            *count += 1;
        }
    }

    /// `zend_closure_free_storage`'s `destroy_op_array`: a closure over
    /// `function` gone, and with the last one over a request-only unit
    /// whose main op array has ended and which bound nothing, the
    /// unit's strings, as php frees the op array at its last count. An
    /// `eval` whose closure goes before it returns, or after, costs
    /// nothing past it:
    ///
    /// ```php
    /// eval('while(0){}');
    /// $m = memory_get_usage(); eval('fn () => 1;'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$y = 2;'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('if (0) { fn () => 1; }'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $f = eval('return fn () => "abcdefghijklmnopqrstuvwxyz";'); $g = $f; unset($f, $g); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 0
    /// 0
    /// 0
    /// 0
    /// ```
    ///
    /// A closure's own strings, its `{closure:…}` name, its constants and
    /// its variables' names, go with its op array's last count
    /// (`destroy_op_array`), and the unit's file name with the last op
    /// array of the unit: the outer closure's, and then the inner's with
    /// the file's:
    ///
    /// ```php
    /// $o = eval('return function ($abcdef) { $long = "abcdefghijklmnopqrstuvwxyz"; return function () use ($abcdef) { return $abcdef; }; };');
    /// $i = $o(1);
    /// $m = memory_get_usage(); unset($o); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($i); echo memory_get_usage() - $m, "\n";
    /// $o = eval('return function () { return function () { return 2; }; };');
    /// $i = $o();
    /// $m = memory_get_usage(); unset($o); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($i); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// -936
    /// -1536
    /// -640
    /// -720
    /// ```
    pub(crate) fn uncount_closure(&mut self, function: crate::api::call::Function) {
        let Some((unit, dynamic)) = self.closure_unit(function) else {
            return;
        };
        let strings = &mut self.unit_strings;
        let Some(held) = strings.units.get_mut(unit) else {
            return;
        };
        let over = held.over;
        // `destroy_op_array` at the op array's last count: its blocks and
        // its strings.
        if let Some(at) = dynamic {
            held.declared.uncount_closure(at, over, &mut strings.gone);
        }
        let release = match &mut held.past {
            Past::Closures(count) => {
                *count = count.saturating_sub(1);
                *count == 0 && over
            }
            Past::Tables => false,
        };
        if release || !strings.gone.is_empty() {
            self.release_unit_strings(None);
        }
    }

    /// `do_bind_function`'s binding of the dynamic definition at `at` of
    /// `unit`, a function declared inside a block or a body: the
    /// function table holds its op array from here to the request's end,
    /// and with it the unit's strings, as it holds a closure's count that
    /// never goes.
    ///
    /// ```php
    /// eval('if (1) { function g() { return "abcdefghijklmnopqrstuvwxyz" . "0123456789"; } }');
    /// $x = str_repeat("q", 26);
    /// $m = memory_get_usage(); eval('if (1) { function g2() { return 1; } }'); echo memory_get_usage() - $m, "\n";
    /// echo g(), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 240
    /// abcdefghijklmnopqrstuvwxyz0123456789
    /// ```
    pub(crate) fn bind_dynamic(&mut self, unit: &PublishedUnit, at: u32) {
        let Some(held) = self.unit_strings.held_mut(PublishedUnit::as_ptr(unit)) else {
            return;
        };
        held.declared.bind(at as usize);
        if let Past::Closures(count) = &mut held.past {
            *count += 1;
        }
    }

    /// What php's compile `emalloc`s for each op array and class `unit`
    /// declares ([`super::declared_bytes`]), laid down once for a unit
    /// the request compiled for itself, before its main op array runs;
    /// nothing for another. The `Err` is the memory limit.
    ///
    /// ```php
    /// eval('while(0){}');
    /// $f0 = fn () => 0; unset($f0);
    /// $m = memory_get_usage(); $f = fn () => 1; echo memory_get_usage() - $m, "\n"; unset($f);
    /// $m = memory_get_usage(); $f = eval('return fn () => 1;'); echo memory_get_usage() - $m, "\n"; unset($f);
    /// $m = memory_get_usage(); eval('function fx() {}'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('class Cx {}'); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 384
    /// 704
    /// 192
    /// 160
    /// ```
    pub(crate) fn lay_declared(&mut self, unit: &PublishedUnit) -> Result<(), Thrown> {
        // An `eval` that declares nothing, the most of them, lays nothing.
        if unit.functions.is_empty() && unit.dynamic.is_empty() && unit.classes.is_empty() {
            return Ok(());
        }
        let unit_at = PublishedUnit::as_ptr(unit);
        let Some(held) = self.unit_strings.held_mut(unit_at) else {
            return Ok(());
        };
        if held.declared.lay() {
            return Ok(());
        }
        let laid = declared_bytes::of_unit(self, unit)?;
        match self.unit_strings.held_mut(unit_at) {
            Some(held) => held.declared.hold(laid),
            None => laid.free(self),
        }
        Ok(())
    }

    /// The request-only unit `function` is a function of, where it is one:
    /// none at once where the request holds no such unit, the common
    /// program's every closure, or where the function is not one of the
    /// request's own, whose unit is the engine's or the cache's.
    fn closure_unit(&self, function: crate::api::call::Function) -> Option<(usize, Option<usize>)> {
        if self.unit_strings.units.is_empty() {
            return None;
        }
        let user = self.user_functions.get(function.local_at()?)?;
        let unit = PublishedUnit::as_ptr(&user.unit);
        let dynamic = match user.body {
            crate::exec::Body::Dynamic(at) => Some(at as usize),
            _ => None,
        };
        Some((self.unit_strings.position(unit)?, dynamic))
    }

    /// Every request-only unit's strings given up at the request's end,
    /// as `shutdown_executor` destroys the op arrays that hold them.
    pub(crate) fn release_all_unit_strings(&mut self) {
        let mut held = std::mem::take(&mut self.unit_strings);
        for s in held.kept.into_values() {
            self.release(Value::string(s));
        }
        let mut gone = held.gone;
        for unit in &mut held.units {
            std::mem::take(&mut unit.declared).all(&mut gone);
        }
        let mut names = Vec::new();
        let mut constants = held.compiling.made;
        for Gone { blocks, strings } in gone {
            blocks.free(self);
            names.extend(strings.names);
            constants.extend(strings.constants);
        }
        for blocks in held.dead_blocks {
            blocks.free(self);
        }
        for held in held.units {
            names.extend(held.names.into_iter().chain(held.file));
            constants.extend(held.main);
            constants.extend(held.constants);
        }
        for s in names {
            self.release(Value::string(s));
        }
        for z in constants {
            self.release(Value::of_slot(z));
        }
    }
}
