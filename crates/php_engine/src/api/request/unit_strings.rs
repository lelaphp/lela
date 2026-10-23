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

use std::collections::HashMap;

use php_compile::Named;
use php_compile::values::Holder;
use php_heap::{Str, Zval};
use php_link::NameHasher;
use php_unit::decl::TraitAdaptation;
use php_unit::{PublishedUnit, Unit, WeakUnit};

use super::Request;
use super::declared_bytes::{self, Blocks, ClassBlocks, Declared, Gone};
use crate::api::raising::Thrown;
use crate::api::values::{PhpString, Value};

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
    handed: Vec<bool>,
}

/// The strings one op array a request-only unit declared owns apart
/// from the unit, a dynamic definition's, which `destroy_op_array`
/// releases with it: its name (`op_array->function_name`) and its
/// lowered key where it is bound under one, its variables' names, which
/// `by_bytes` may point at, and its constants.
#[derive(Default)]
pub(super) struct OpArrayStrings {
    names: Vec<PhpString>,
    constants: Vec<Zval>,
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
    /// The names its declarations own ([`php_compile::named`]): its op
    /// arrays' and classes', their keys and the names they are read by,
    /// those its compile made and an `eval`'s others, made as it is
    /// noted.
    names: Vec<PhpString>,
    /// Whether the names were made, which a unit's noting does once.
    named: bool,
    /// For each place of [`php_compile::named`] of a unit that declares,
    /// whether the compile handed its name over, or the file's was made
    /// before the compile ([`Request::name_unit`]): the names noting
    /// makes none of ([`Request::hold_unit_names`]).
    handed: Vec<bool>,
    /// The file's name of an `eval` that declares nothing, the string its
    /// name was made as before its compile ([`Request::name_unit`]),
    /// held to the main op array's end with no entry of `by_bytes`, so
    /// the `eval` hashes nothing for it: a copy of it is found by
    /// [`UnitStrings::find`] looking over the few that stand.
    file: Option<PhpString>,
    /// The strings of `main` that stand for its variables' names, which
    /// [`UnitStrings::by_bytes`] may point at until the main op array
    /// ends: views, the owners being `main`'s.
    found_main: Vec<PhpString>,
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
    /// What the compile handed over as its declarations' own names
    /// ([`php_heap::Intern::hand_name`]), one for each place of the walk
    /// that tells what each names ([`php_compile::named`]).
    names: Vec<Option<Str>>,
}

impl Compiling {
    /// `z`, a string or an array this compile made with one owner, kept
    /// for the unit.
    pub(super) fn add(&mut self, z: Zval) {
        self.made.push(z);
    }

    /// The next of the names the compile handed over.
    pub(super) fn hand(&mut self, name: Option<Str>) {
        self.names.push(name);
    }

    /// Whether `z` is one this compile made, its owner taken out to be
    /// the caller's ([`taken`]).
    pub(super) fn take(&mut self, z: Zval) -> bool {
        taken(&mut self.made, z)
    }
}

/// Whether `z` is among `made`, the values a compile made, one owner
/// each, its owner taken out to be the caller's: a constant array moving
/// a string or an array the compile made into itself, or a declaration
/// taking its name. The newest are looked at first, which are an
/// array's own entries.
fn taken(made: &mut Vec<Zval>, z: Zval) -> bool {
    if !z.is_refcounted() {
        return false;
    }
    let Some(at) = made.iter().rposition(|made| made.is_same_value(z)) else {
        return false;
    };
    made.swap_remove(at);
    true
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
/// ([`php_compile::values::declared_values`]). Each outlives the `eval` that declared it, read
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
    let body_owner = |at: usize| function_owner(unit, at);
    let bodies = || {
        (unit.functions.iter().chain(&unit.dynamic))
            .map(|(_, body)| body)
            .chain(unit.methods.iter().map(|method| &method.body))
            .chain(unit.hooks.iter().map(|hook| &hook.body))
            .enumerate()
            .map(move |(at, body)| (body_owner(at), body))
    };
    let mut literals: Vec<(Zval, Owner)> = bodies()
        .flat_map(|(owner, body)| {
            (body.literals.iter().copied())
                .chain(body.bound_vars.iter().map(|bound| bound.value))
                .map(move |z| (z, owner))
        })
        .collect();
    // And a value a constant expression the op array keeps whole holds
    // inside its tree (`IS_CONSTANT_AST`), a parameter's default that did
    // not fold among them, whose `RECV_INIT` holds the tree.
    for (owner, body) in bodies() {
        for expr in &body.const_exprs {
            php_compile::values::unit_values(expr, &mut |z| literals.push((z, owner)));
        }
    }
    let literals = ByIdentity::of(literals);
    let mut declared = ByIdentity::of(
        (php_compile::values::declared_values(unit).into_iter())
            .map(|(z, holder)| (z, owner_of(holder))),
    );
    // A declaration's, one owner for each slot that holds it, ahead of
    // an op array's constant's.
    made.drain(..)
        .map(|z| {
            let owner = (declared.take(z))
                .or_else(|| literals.find(z))
                .unwrap_or(Owner::Main);
            (owner, z)
        })
        .collect()
}

/// The op array that owns what `holder` holds ([`Holder`]).
fn owner_of(holder: Holder) -> Owner {
    match holder {
        Holder::Main => Owner::Main,
        Holder::Unit => Owner::Tables,
        Holder::Dynamic(at) => Owner::Definition(at),
    }
}

/// The holders of a compile's strings and arrays, each found by its
/// identity, never by its bytes: the strings sorted by their bytes, so a
/// string is looked for among those alike, and the arrays as listed.
struct ByIdentity {
    strings: Vec<(Str, Owner)>,
    arrays: Vec<(Zval, Owner)>,
}

impl ByIdentity {
    /// The counted values of `held`, each beside what holds it.
    fn of(held: impl IntoIterator<Item = (Zval, Owner)>) -> ByIdentity {
        let (mut strings, mut arrays) = (Vec::new(), Vec::new());
        for (z, owner) in held.into_iter().filter(|(z, _)| z.is_refcounted()) {
            match z.as_str() {
                Some(s) => strings.push((s, owner)),
                None => arrays.push((z, owner)),
            }
        }
        strings.sort_unstable_by(|a, b| a.0.bytes().cmp(b.0.bytes()));
        ByIdentity { strings, arrays }
    }

    /// Where `z` stands among the strings or among the arrays.
    fn place(&self, z: Zval) -> Option<Place> {
        let Some(s) = z.as_str() else {
            return (self
                .arrays
                .iter()
                .position(|(held, _)| held.is_same_value(z)))
            .map(Place::Array);
        };
        let from = self
            .strings
            .partition_point(|(held, _)| held.bytes() < s.bytes());
        (self.strings.get(from..).unwrap_or_default().iter())
            .take_while(|(held, _)| held.bytes() == s.bytes())
            .position(|(held, _)| held.is_same(s))
            .map(|at| Place::String(from + at))
    }

    /// What holds `z`, where something does.
    fn find(&self, z: Zval) -> Option<Owner> {
        match self.place(z)? {
            Place::String(at) => self.strings.get(at).map(|(_, owner)| *owner),
            Place::Array(at) => self.arrays.get(at).map(|(_, owner)| *owner),
        }
    }

    /// What holds `z`, taken out, so a second owner of it finds the next
    /// holder.
    fn take(&mut self, z: Zval) -> Option<Owner> {
        match self.place(z)? {
            Place::String(at) => Some(self.strings.remove(at).1),
            Place::Array(at) => Some(self.arrays.swap_remove(at).1),
        }
    }
}

/// Where a value stands in a [`ByIdentity`]: its place among the strings
/// or among the arrays.
#[derive(Clone, Copy)]
enum Place {
    String(usize),
    Array(usize),
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
        let held = self.units.get(self.position(unit)?)?;
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
            held.handed.clear();
            spare.push(Spare {
                main: std::mem::take(&mut held.main),
                constants: std::mem::take(&mut held.constants),
                names: std::mem::take(&mut held.names),
                found_main: std::mem::take(&mut held.found_main),
                handed: std::mem::take(&mut held.handed),
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
            } = strings;
            dead.append(&mut names);
            constants.append(&mut owned);
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
                        (held.names.iter().chain(&held.found_main))
                            .chain(held.declared.strings().flat_map(|own| &own.names))
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
        if let Some(at) = self.position(PublishedUnit::as_ptr(unit)) {
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
            handed: spare.handed,
            file: None,
            found_main: spare.found_main,
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
    ///
    /// A declaration's name is the string php's compile copies where
    /// the declaration names itself, and a token's of its own anywhere
    /// else, a call, a `new` or a static call in the same `eval` among
    /// them; a runtime declaration's lowered key is its `DECLARE_*`'s
    /// constant:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3286-own-names.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;'); m('interface I1 {}');
    /// foreach ([
    ///     'class Y5 {} function f13() { $o = new Y5; return 1; }',
    ///     'class y8 {} function f8() { $o = new y8; return 1; }',
    ///     'function fa1() { return __FUNCTION__; }',
    ///     'class Ca3 { function g() { return __CLASS__; } }',
    ///     'class Ca5 { function g() { return self::class; } }',
    ///     'function fa7() { function fa8() {} }',
    ///     'function fa9() { function Fa10() {} }',
    ///     'function fa13() { class Ca14 {} }',
    ///     'class Ca17 implements I1 {}',
    ///     'function fa18() {} function fa19() { fa18(); }',
    ///     'class Ca22 { function meth() {} function g() { $this->meth(); } }',
    ///     'class Ca23 { static function meth() {} function g() { Ca23::meth(); } }',
    ///     'function fa24() { return new class {}; }',
    /// ] as $c) echo m($c), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 544
    /// 480
    /// 256
    /// 608
    /// 608
    /// 368
    /// 400
    /// 464
    /// 192
    /// 416
    /// 800
    /// 896
    /// 496
    /// ```
    ///
    /// Each is given to what it names by its place in the compile's walk
    /// ([`php_compile::named`]), never by its bytes, so a dynamic
    /// definition's variable named like a function, a property named like
    /// its class and a class named like a function are each a string of
    /// their own, gone with what holds them (the file under the recipe's
    /// `TMPDIR`):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3311-colliding-names.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// echo m('function abcdefgi() { return 2; } $GLOBALS["d"] = function () { $abcdefgi = "w"; return $abcdefgi; };'), "\n";
    /// echo m('if (1) { function qwertyu() { $qwertyuiop = 1; return $qwertyuiop; } } function qwertyuiop() { return 3; }'), "\n";
    /// echo m('class Abcdefgk { public $abcdefgk = 1; protected $Abcdefgk = 2; }'), "\n";
    /// echo m('class abcdefgl { public $abcdefgl = 4; }'), "\n";
    /// echo m('class zxcvbnm {} function zxcvbnm() { return 5; }'), "\n";
    /// $m = memory_get_usage(); unset($d); echo memory_get_usage() - $m, "\n";
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo json_encode(eval('return [abcdefgi(), qwertyu(), qwertyuiop(), (array) new Abcdefgk, (new abcdefgl)->abcdefgl, zxcvbnm(), new zxcvbnm];')), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 1488
    /// 496
    /// 664
    /// 528
    /// 288
    /// -712
    /// [2,1,3,{"abcdefgk":1,"\u0000*\u0000Abcdefgk":2},4,5,{}]
    /// ```
    pub(crate) fn hold_compiled(&mut self, unit: &PublishedUnit) {
        let strings = &mut self.unit_strings;
        let at = strings.held(unit);
        let Some(held) = strings.units.get_mut(at) else {
            return;
        };
        // The strings the compile handed over as its declarations' own
        // ([`php_heap::Intern::hand_name`]), each given to what its place
        // in the walk names ([`php_compile::named`]): an op array's or a
        // class's name or key, which an `eval`'s noting makes no other of
        // ([`Request::hold_unit_names`]), or a name a declaration is read
        // by, a main op array's variable's until it ends. A copy of one
        // finds it ([`Request::declared_string`]) while what holds it
        // lives. Any other string of a name's bytes is a token's of its
        // own, as php's scanner made one for each.
        let Compiling { made, names } = &mut strings.compiling;
        held.handed.clear();
        for ((named, _), name) in php_compile::named(unit).zip(names.drain(..)) {
            let name = name.filter(|s| taken(made, Zval::string(*s)));
            if held.declares {
                held.handed.push(name.is_some());
            }
            let Some(s) = name.map(PhpString::of) else {
                continue;
            };
            match named {
                Named::Read(Holder::Main) => {
                    held.main.push(Zval::string(s.heap()));
                    held.found_main.push(s);
                }
                Named::File | Named::Class | Named::InternedKey => held.names.push(s),
                Named::Function(holder) | Named::Key(holder) | Named::Read(holder) => {
                    held.names_of(owner_of(holder)).push(s);
                }
            }
            point(&mut strings.by_bytes, s);
        }
        if !held.declares {
            held.main.append(made);
            return;
        }
        for (owner, z) in main_constants(unit, made) {
            match owner {
                Owner::Main => held.main.push(z),
                Owner::Tables => held.constants.push(z),
                Owner::Definition(at) => match held.declared.definition_strings(at) {
                    Some(own) => own.constants.push(z),
                    None => held.constants.push(z),
                },
            }
        }
    }

    /// The owner a request-only unit holds of `value` for a declaration
    /// written as a value, given up as the request's row takes the
    /// declaration's slot over: php's `constants_table` and
    /// `default_properties_table` are the declaration's slot, one owner
    /// where the engine keeps the declaration and the row apart. Nothing
    /// for a value no unit holds, an immutable one or one an evaluation
    /// made. So a request's class constant reads php's count, an
    /// object's default one more, the object's, and a static two, the
    /// default's and the slot's (`ZVAL_COPY_OR_DUP`); a parameter's
    /// default string is the `RECV_INIT` constant's, which Reflection
    /// copies. Each is told by its identity, so two constants of the
    /// same bytes hold two strings, as php's two zvals are:
    ///
    /// ```php
    /// eval('class K { const S = "abcdefghijk" . "x"; const T = "abcdefghijkx"; public $p = "abcdefghijk" . "y"; public static $s = "abcdefghijk" . "z"; }');
    /// eval('class KA { const C = [1, 2]; public $p = [3]; public static $s = [4]; }');
    /// eval('function f($p = "abcdefghijk" . "w", $q = [5]) {}');
    /// $k = new K;
    /// $ka = new KA;
    /// debug_zval_dump(K::S, $k->p, K::$s, KA::C, $ka->p, KA::$s, K::S, KA::C, K::T);
    /// foreach ((new ReflectionFunction('f'))->getParameters() as $p) {
    ///     debug_zval_dump($p->getDefaultValue());
    /// }
    /// ```
    /// ```expect regime=jit-off
    /// string(12) "abcdefghijkx" refcount(3)
    /// string(12) "abcdefghijky" refcount(3)
    /// string(12) "abcdefghijkz" refcount(3)
    /// array(2) packed refcount(3){
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// array(1) packed refcount(3){
    ///   [0]=>
    ///   int(3)
    /// }
    /// array(1) packed refcount(3){
    ///   [0]=>
    ///   int(4)
    /// }
    /// string(12) "abcdefghijkx" refcount(3)
    /// array(2) packed refcount(3){
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// string(12) "abcdefghijkx" refcount(2)
    /// string(12) "abcdefghijkw" refcount(2)
    /// array(1) packed refcount(2){
    ///   [0]=>
    ///   int(5)
    /// }
    /// ```
    pub(super) fn take_declaration_slot(&mut self, value: Value) {
        let z = value.to_zval();
        let held = (self.unit_strings.units.iter_mut()).any(|held| {
            let at = held.constants.iter().position(|c| c.is_same_value(z));
            at.map(|at| held.constants.swap_remove(at)).is_some()
        });
        if held {
            self.release(value);
        }
    }

    /// The constant strings a compile the compiler refused made, given up
    /// as `zend_compile`'s refusal destroys the op array it was making.
    pub(crate) fn release_compiled(&mut self) {
        self.unit_strings.compiling.names.clear();
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
    /// that holds its file's name already leaves the string to be given
    /// up now.
    pub(crate) fn name_unit(&mut self, unit: &PublishedUnit, name: PhpString) {
        let strings = &mut self.unit_strings;
        let at = strings.held(unit);
        let Some(held) = (strings.units.get_mut(at))
            .filter(|held| held.file.is_none() && held.handed.first() != Some(&true))
        else {
            self.release(Value::string(name));
            return;
        };
        if held.declares {
            held.names.push(name);
            // The walk's first place is the file's ([`php_compile::Named::File`]).
            if let Some(file) = held.handed.first_mut() {
                *file = true;
            }
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
    ///
    /// and so is a function's lowered key, the function table's, where
    /// php interns only a class's and a method's:
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('w3310-function-key.inc', <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// echo m('namespace NS1; class NQ { const K = self::class; } function fn1() { return NQ::K; }'), "\n";
    /// echo m('function Fqwertyu() { return 2; } class Cqwertyu { function Mqwertyu() { return 3; } }'), "\n";
    /// echo NS1\fn1(), Fqwertyu(), (new Cqwertyu)->Mqwertyu(), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 672
    /// 776
    /// NS1\NQ23
    /// ```
    pub(crate) fn hold_unit_names(&mut self, unit: &PublishedUnit) -> Result<(), Thrown> {
        let at = self.unit_strings.held(unit);
        let Some(held) = self.unit_strings.units.get_mut(at) else {
            return Ok(());
        };
        if std::mem::replace(&mut held.named, true) {
            return Ok(());
        }
        // The names held already, each by its place: the file's, made
        // before the compile ([`Request::name_unit`]) and shared by every
        // op array (`zend_string_copy`), and those the compile handed
        // over ([`Request::hold_compiled`]).
        let handed = std::mem::take(&mut held.handed);
        let names = php_compile::named(unit).zip(handed.iter().chain(std::iter::repeat(&false)));
        for ((named, name), &handed) in names {
            let (owner, made) = match named {
                _ if handed => continue,
                // Every op array's, made once for each: two closures on
                // one line are two `{closure:…}` strings, and a
                // function's name and a top-level one's key, the function
                // table's, are made whatever the cache holds
                // (`zend_begin_func_decl`).
                Named::File | Named::Key(Holder::Unit) => (Owner::Tables, true),
                Named::Function(holder) => (owner_of(holder), true),
                // A dynamic definition's key is its `DECLARE_FUNCTION`'s
                // constant, which the compile made, or the interned
                // string where opcache holds the bytes (`zend_insert_literal`).
                Named::Key(_) => continue,
                // A class's name and its and its methods' keys are
                // `zend_new_interned_string`'s: the interned string where
                // opcache holds the bytes, `count` or `__tostring` or one
                // a cached file's constants hold, and the one made goes
                // (`accel_new_interned_string_for_php`).
                Named::Class | Named::InternedKey => (
                    Owner::Tables,
                    self.shared.existing_interned(&name).is_none(),
                ),
                // The rest the compile made.
                Named::Read(_) => break,
            };
            if made {
                self.hold_name(at, owner, &name)?;
            }
        }
        if let Some(held) = self.unit_strings.units.get_mut(at) {
            held.handed = handed;
            held.handed.clear();
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
