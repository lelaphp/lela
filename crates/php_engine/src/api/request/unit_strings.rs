//! The strings a request-only unit owns, held as php's op arrays hold
//! theirs. A file's unit has its constants and names interned into the
//! engine's table as it is published ([`php_compile::intern_names`]), as
//! opcache's persist interns them. A unit compiled for the request alone,
//! an `eval` (`-r`'s among them) or an uncached include, is the
//! request's: under opcache its compile's `zend_new_interned_string` is
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
//! return, and the rest, what a declared function, closure, method or
//! class carries, once nothing holds the unit. A value copied out of a
//! constant takes an owner of its own (`ZVAL_COPY`), as a trace row or
//! a diagnostic that copies a name does (`ZVAL_STR_COPY`), so nothing a
//! unit made is held to the request's end for being made.

use std::collections::{HashMap, HashSet};

use php_heap::{Str, Zval};
use php_link::NameHasher;
use php_unit::{PublishedUnit, Unit, WeakUnit};

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::{PhpString, Value};

/// The most names a unit dedupes by comparing each with the ones it
/// already holds, past which a set of them is made.
const SCANNED_NAMES: usize = 16;

/// The strings of the request-only units the request holds: each unit's,
/// one owner apiece, beside the unit they go with, the newest name string
/// held for each name, where a copy finds it ([`Names`]), and the
/// constants of the compile under way. The units stand in the first
/// `live` places, in the order they were taken; past them stand the
/// places of units that are over, their vectors emptied and kept for
/// the next units to take.
#[derive(Default)]
pub(crate) struct UnitStrings {
    units: Vec<Held>,
    live: usize,
    names: Names,
    dead: Vec<PhpString>,
    compiling: Compiling,
}

/// Every name a request-only unit's string has stood for, each hashed
/// once, as the first unit of the name holds it, to the row the units
/// that hold a string of it keep beside the string ([`HeldName`]), so a
/// unit letting one go hashes nothing. A name no living unit holds keeps
/// its row with no string, so the next unit of that name makes no key of
/// its own. An `eval`'s file name is held with no row
/// ([`Names::hold_unindexed`]) and hashed only where a copy of it is
/// asked for while its unit stands ([`UnitStrings::newest`]), so an
/// `eval` in a loop hashes nothing.
#[derive(Default)]
struct Names {
    by_bytes: HashMap<Box<[u8]>, u32, NameHasher>,
    rows: Vec<Named>,
    /// How many names living units hold with no row.
    unindexed: usize,
}

/// One name's row: the newest string a living unit holds of it, where a
/// copy finds it, and how many living units hold one.
#[derive(Default)]
struct Named {
    newest: Option<PhpString>,
    holders: u32,
}

/// One of a unit's names: its string, one owner the unit's, and the row
/// of the name it stands for, [`UNINDEXED`] for none yet.
#[derive(Clone, Copy)]
struct HeldName {
    string: PhpString,
    row: u32,
}

/// The row of a name held with no row ([`Names::hold_unindexed`]).
const UNINDEXED: u32 = u32::MAX;

impl Names {
    /// `string` held by one more unit, the newest string of its name.
    fn hold(&mut self, string: PhpString) -> HeldName {
        let bytes = string.as_bytes();
        let row = if let Some(&row) = self.by_bytes.get(bytes) {
            row
        } else {
            let row = u32::try_from(self.rows.len()).unwrap_or(UNINDEXED - 1);
            self.rows.push(Named::default());
            self.by_bytes.insert(bytes.into(), row);
            row
        };
        if let Some(named) = self.rows.get_mut(row as usize) {
            named.newest = Some(string);
            named.holders = named.holders.saturating_add(1);
        }
        HeldName { string, row }
    }

    /// `string` held by one more unit with no row, which is given it
    /// where a copy of it is asked for.
    fn hold_unindexed(&mut self, string: PhpString) -> HeldName {
        self.unindexed += 1;
        HeldName {
            string,
            row: UNINDEXED,
        }
    }

    /// `name` let go by the unit that held it: whether its row's newest
    /// string was it while another living unit holds one, which is then
    /// to be found among theirs ([`UnitStrings::take`]).
    fn let_go(&mut self, name: HeldName) -> bool {
        if name.row == UNINDEXED {
            self.unindexed = self.unindexed.saturating_sub(1);
            return false;
        }
        let Some(named) = self.rows.get_mut(name.row as usize) else {
            return false;
        };
        named.holders = named.holders.saturating_sub(1);
        if !named
            .newest
            .is_some_and(|newest| newest.heap().is_same(name.string.heap()))
        {
            return false;
        }
        named.newest = None;
        named.holders > 0
    }

    /// The newest string a living unit holds of `bytes`.
    fn newest(&self, bytes: &[u8]) -> Option<PhpString> {
        let row = *self.by_bytes.get(bytes)?;
        self.rows.get(row as usize)?.newest
    }
}

/// One request-only unit and the strings it owns; a place no unit stands
/// in any longer, past the live ones, has none.
struct Held {
    unit: Option<WeakUnit>,
    /// Whether it declared a function, a closure or a class, which hold
    /// it past its main op array's end.
    declares: bool,
    /// The constant strings of its main op array, `op_array->literals`
    /// and the names `zend_compile` keeps beside them, given up when
    /// the main op array's execution ends ([`main_constants`]).
    main: Vec<PhpString>,
    /// The constant strings of what it declared, given up once nothing
    /// holds it.
    constants: Vec<PhpString>,
    /// Its op arrays' names ([`php_compile::op_array_names`]): those a
    /// file's compile made, and an `eval`'s, its file's made before its
    /// compile ([`Request::name_unit`]) and the rest as it is noted.
    names: Vec<HeldName>,
    /// Whether the names were made, which a unit's noting does once.
    named: bool,
}

/// The constant strings the compile under way has made, which become
/// the unit's when the compile hands it over ([`Request::hold_compiled`])
/// and are given up when it refuses ([`Request::release_compiled`]).
#[derive(Default)]
pub(crate) struct Compiling {
    made: Vec<PhpString>,
}

impl Compiling {
    /// `s`, a string this compile made with one owner, kept for the unit.
    pub(super) fn add(&mut self, s: Str) {
        self.made.push(PhpString::of(s));
    }
}

/// `made`, the strings a compile of `unit` made, split between its main
/// op array's, put in `main`, and what it declared, put in `declared`,
/// as php's compile leaves each string in the op array whose constant
/// or name it is. A string a function's, a closure's, a method's or a
/// hook's constants (`op_array->literals`) or static variables hold is
/// the declaration's, and so is one written as one of their variables'
/// names or a declared property's name, which `zend_compile` keeps in
/// `op_array->vars` and `prop_info->name`; every other string is the
/// main op array's, its constants, its variables' names and what a fold
/// made on the way, which php's compile frees as it goes. A constant is told by its
/// allocation, not its bytes, since two written alike are two
/// strings.
fn main_constants(
    unit: &Unit,
    made: &mut Vec<PhpString>,
    main: &mut Vec<PhpString>,
    declared: &mut Vec<PhpString>,
) {
    let bodies = || {
        (unit.functions.iter().chain(&unit.dynamic))
            .map(|(_, body)| body)
            .chain(unit.methods.iter().map(|method| &method.body))
            .chain(unit.hooks.iter().map(|hook| &hook.body))
    };
    let mut held: Vec<Str> = bodies()
        .flat_map(|body| {
            (body.literals.iter().copied()).chain(body.bound_vars.iter().map(|bound| bound.value))
        })
        .filter_map(Zval::as_str)
        .collect();
    held.sort_unstable_by(|a, b| a.bytes().cmp(b.bytes()));
    let mut names: HashSet<Vec<u8>, NameHasher> = HashSet::with_hasher(NameHasher);
    names.extend(bodies().flat_map(|body| body.vars.iter().map(|var| var.0.to_vec())));
    for class in &unit.classes {
        for property in &class.properties {
            names.insert(property.name.0.to_vec());
            if let Some(prefix) = php_compile::mangling(property.visibility, &class.name.0) {
                names.insert([prefix.as_slice(), &property.name.0].concat());
            }
        }
    }
    for s in made.drain(..) {
        let bytes = s.as_bytes();
        let from = held.partition_point(|held| held.bytes() < bytes);
        let theirs = (held.get(from..).unwrap_or_default().iter())
            .take_while(|held| held.bytes() == bytes)
            .any(|held| held.is_same(s.heap()))
            || names.contains(bytes);
        if theirs {
            declared.push(s);
        } else {
            main.push(s);
        }
    }
}

impl UnitStrings {
    /// The compile under way, which the home its constants are made in
    /// keeps them in ([`super::memory::RequestStrings`]).
    pub(super) fn compiling_mut(&mut self) -> &mut Compiling {
        &mut self.compiling
    }

    /// The strings that are over taken out: those of each unit nothing
    /// holds any longer, and those of `ended`, a unit whose main op
    /// array has finished, whatever share of it the loop still holds for
    /// the opline that returned: its main op array's constants, and,
    /// where it declared nothing a later call could reach, the rest.
    /// Each name one of those strings stood for is pointed at an equal
    /// string a living unit holds, or at none.
    fn take(&mut self, ended: Option<*const Unit>) -> Vec<PhpString> {
        let mut dead = std::mem::take(&mut self.dead);
        let mut stale = Vec::new();
        let mut kept = 0;
        for at in 0..self.live {
            let Some(held) = self.units.get_mut(at) else {
                break;
            };
            let unit = held.unit.as_ref();
            let alive = unit.is_some_and(|unit| unit.strong_count() > 0);
            let ends = unit.is_some_and(|unit| ended == Some(unit.as_ptr()));
            if (!alive || ends) && !held.main.is_empty() {
                dead.append(&mut held.main);
            }
            if alive && (!ends || held.declares) {
                self.units.swap(kept, at);
                kept += 1;
                continue;
            }
            held.unit = None;
            for name in held.names.drain(..) {
                if self.names.let_go(name) {
                    stale.push(name.row);
                }
                dead.push(name.string);
            }
            if !held.constants.is_empty() {
                dead.append(&mut held.constants);
            }
        }
        self.live = kept;
        for row in stale {
            let newest = (self.units.iter().take(self.live).rev())
                .flat_map(|held| &held.names)
                .find(|held| held.row == row)
                .map(|held| held.string);
            if let Some(named) = self.names.rows.get_mut(row as usize) {
                named.newest = newest;
            }
        }
        dead
    }

    /// The units standing, in the order they were taken.
    fn live(&self) -> &[Held] {
        self.units.get(..self.live).unwrap_or_default()
    }

    /// The newest string a living unit holds of `bytes`: the one its row
    /// names, else one held with no row, the newest unit's first, which
    /// is given its row here.
    fn newest(&mut self, bytes: &[u8]) -> Option<PhpString> {
        if let Some(held) = self.names.newest(bytes) {
            return Some(held);
        }
        if self.names.unindexed == 0 {
            return None;
        }
        let (unit, at) = (self.live().iter().enumerate().rev()).find_map(|(unit, held)| {
            let at = (held.names.iter())
                .position(|name| name.row == UNINDEXED && name.string.as_bytes() == bytes)?;
            Some((unit, at))
        })?;
        let name = self.units.get_mut(unit)?.names.get_mut(at)?;
        let string = name.string;
        self.names.unindexed = self.names.unindexed.saturating_sub(1);
        *name = self.names.hold(string);
        Some(string)
    }

    /// The entry for `unit`, laid down where there is none, in the first
    /// place past the live ones.
    fn held(&mut self, unit: &PublishedUnit) -> usize {
        let ptr = PublishedUnit::as_ptr(unit);
        if let Some(at) = (self.live().iter()).position(|held| {
            held.unit
                .as_ref()
                .is_some_and(|held| std::ptr::eq(held.as_ptr(), ptr))
        }) {
            return at;
        }
        let taken = Some(PublishedUnit::downgrade(unit));
        let declares =
            !(unit.functions.is_empty() && unit.dynamic.is_empty() && unit.classes.is_empty());
        match self.units.get_mut(self.live) {
            Some(place) => {
                place.unit = taken;
                place.declares = declares;
                place.named = false;
            }
            None => self.units.push(Held {
                unit: taken,
                declares,
                main: Vec::new(),
                constants: Vec::new(),
                names: Vec::new(),
                named: false,
            }),
        }
        self.live += 1;
        self.live - 1
    }
}

impl Request {
    /// Whether `class` is one a unit compiled for this request alone
    /// declared, an `eval`'s or an uncached include's, whose constant
    /// strings are its own rather than the engine's.
    pub(crate) fn declared_for_the_request(&self, class: crate::api::values::ClassId) -> bool {
        self.declared_row(class).is_some_and(|row| {
            let unit = PublishedUnit::as_ptr(&row.unit);
            (self.unit_strings.live().iter()).any(|held| {
                held.unit
                    .as_ref()
                    .is_some_and(|held| std::ptr::eq(held.as_ptr(), unit))
            })
        })
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
        // ([`Request::op_array_string`]) while the unit lives; an
        // `eval`'s made none, and they are made as it is noted
        // ([`Request::hold_unit_names`]).
        let mut seen = HashSet::with_hasher(NameHasher);
        for name in php_compile::op_array_names(unit) {
            let Some(at) = made.iter().rposition(|s| s.as_bytes() == name) else {
                continue;
            };
            if !seen.insert(name) {
                continue;
            }
            let s = made.swap_remove(at);
            held.names.push(strings.names.hold(s));
        }
        if held.declares {
            main_constants(unit, made, &mut held.main, &mut held.constants);
        } else {
            held.main.append(made);
        }
    }

    /// The constant strings a compile the compiler refused made, given up
    /// as `zend_compile`'s refusal destroys the op array it was making.
    pub(crate) fn release_compiled(&mut self) {
        let mut made = std::mem::take(&mut self.unit_strings.compiling.made);
        for s in made.drain(..) {
            self.release(Value::string(s));
        }
        self.unit_strings.compiling.made = made;
    }

    /// The name an `eval`'s string was compiled under
    /// ([`Request::eval_string`]), made before its compile as
    /// `zend_make_compiled_string_description` makes it, held by `unit`
    /// as its file's name from here, one owner given up with the unit's
    /// other names, as `destroy_op_array` releases `op_array->filename`.
    /// A unit that holds a name of those bytes already leaves the string
    /// to be given up now.
    pub(crate) fn name_unit(&mut self, unit: &PublishedUnit, name: PhpString) {
        let strings = &mut self.unit_strings;
        let at = strings.held(unit);
        let named = strings.units.get_mut(at).filter(|held| {
            !(held.names.iter()).any(|held| held.string.as_bytes() == name.as_bytes())
        });
        match named {
            Some(held) => {
                held.names.push(strings.names.hold_unindexed(name));
                // A unit that declares nothing has no name but its file's.
                held.named |=
                    unit.functions.is_empty() && unit.dynamic.is_empty() && unit.classes.is_empty();
            }
            None => self.release(Value::string(name)),
        }
    }

    /// `zend_compile`'s names for a request-only unit: the file's name
    /// and each name [`php_compile::op_array_names`] lists, made as
    /// strings of the request and held while the unit lives. The `Err`
    /// is the memory limit.
    pub(crate) fn hold_unit_names(&mut self, unit: &PublishedUnit) -> Result<(), Thrown> {
        let at = self.unit_strings.held(unit);
        let Some(held) = self.unit_strings.units.get_mut(at) else {
            return Ok(());
        };
        if std::mem::replace(&mut held.named, true) {
            return Ok(());
        }
        let mut seen = HashSet::with_hasher(NameHasher);
        for name in php_compile::op_array_names(unit) {
            let names = self
                .unit_strings
                .units
                .get(at)
                .map_or(&[][..], |held| &held.names[..]);
            let again = if names.len() < SCANNED_NAMES {
                names.iter().any(|made| made.string.as_bytes() == name)
            } else {
                if seen.is_empty() {
                    seen.extend(names.iter().map(|made| made.string.as_bytes().to_vec()));
                }
                !seen.insert(name.to_vec())
            };
            if again {
                continue;
            }
            let made = self.string(name)?;
            let strings = &mut self.unit_strings;
            let made = strings.names.hold(made);
            if let Some(held) = strings.units.get_mut(at) {
                held.names.push(made);
            }
        }
        Ok(())
    }

    /// `ZVAL_STR_COPY` of a string an op array carries, its file's name,
    /// a function's or a class's, with one more owner for the caller: the
    /// engine's interned string where it has one, else the string the
    /// request-only unit that carries the name holds, else the request's
    /// own interned one. A throwable made in an `eval`'d unit therefore
    /// costs nothing past the unit's end once it is gone, as php's
    /// costs nothing, and an `eval` that throws nothing costs what php's
    /// does:
    ///
    /// ```php
    /// $m = memory_get_usage(); eval('try { intdiv(1,0); } catch (Error $e) {} unset($e);'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('try { intdiv(1,0); } catch (Error $e) {} unset($e);'); echo memory_get_usage() - $m, "\n";
    /// $f = eval('return function () { try { intdiv(1, 0); } catch (Error $e) {} };');
    /// $m = memory_get_usage(); $f(); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$e = new Exception;'); unset($e); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$x = 1;'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); eval('$x = 1;'); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// ```
    pub(crate) fn op_array_string(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        if let Some(held) = self.shared.existing_interned(bytes) {
            return Ok(PhpString::of(held));
        }
        if let Some(held) = self.unit_strings.newest(bytes) {
            self.addref(&Value::string(held));
            return Ok(held);
        }
        self.declared_string(bytes)
    }

    /// `destroy_op_array`'s release of the strings of each request-only
    /// unit that is over, its constants and its names: those of `ended`'s
    /// main op array, whose frame was just popped, the rest of it too
    /// where it declared no function, closure or class (one that did is
    /// held by what it declared), and every unit nothing holds any
    /// longer.
    pub(crate) fn release_unit_strings(&mut self, ended: Option<&PublishedUnit>) {
        if self.unit_strings.live == 0 {
            return;
        }
        let ended = ended.map(PublishedUnit::as_ptr);
        let mut dead = self.unit_strings.take(ended);
        // `zend_string_release`: a string is no possible root, and
        // destroying one runs nothing.
        for s in dead.drain(..) {
            self.heap.release_string(s.heap());
        }
        self.unit_strings.dead = dead;
    }

    /// Every request-only unit's strings given up at the request's end,
    /// as `shutdown_executor` destroys the op arrays that hold them.
    pub(crate) fn release_all_unit_strings(&mut self) {
        let held = std::mem::take(&mut self.unit_strings);
        let strings = held
            .units
            .into_iter()
            .flat_map(|held| {
                (held.names.into_iter().map(|name| name.string))
                    .chain(held.main)
                    .chain(held.constants)
            })
            .chain(held.compiling.made);
        for s in strings {
            self.release(Value::string(s));
        }
    }
}
