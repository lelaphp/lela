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
//! one per time the compile asks; an `eval`'s names are made when the
//! unit is noted. The main op array's constants are given up when its
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
/// held for each name, where a copy finds it, and the constants of the
/// compile under way. A name no living unit holds keeps its key with no
/// string, so the next unit of that name, an `eval` in a loop's, makes no
/// key of its own; and the vectors a unit's strings stood in are kept for
/// the next unit's.
#[derive(Default)]
pub(crate) struct UnitStrings {
    units: Vec<Held>,
    by_bytes: HashMap<Box<[u8]>, Option<PhpString>, NameHasher>,
    spare: Vec<Vec<PhpString>>,
    dead: Vec<PhpString>,
    compiling: Compiling,
}

/// One request-only unit and the strings it owns.
struct Held {
    unit: WeakUnit,
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
    /// file's compile made, and an `eval`'s, made as it is noted.
    names: Vec<PhpString>,
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
        let mut constants = Vec::new();
        let spare = &mut self.spare;
        self.units.retain_mut(|held| {
            let alive = held.unit.strong_count() > 0;
            let ends = ended == Some(held.unit.as_ptr());
            if alive && !ends {
                return true;
            }
            constants.append(&mut held.main);
            if alive && held.declares {
                return true;
            }
            dead.append(&mut held.names);
            constants.append(&mut held.constants);
            spare.push(std::mem::take(&mut held.names));
            spare.push(std::mem::take(&mut held.constants));
            spare.push(std::mem::take(&mut held.main));
            false
        });
        for name in &dead {
            let bytes = name.as_bytes();
            let Some(held) = self.by_bytes.get_mut(bytes) else {
                continue;
            };
            if held.is_some_and(|held| held.heap().is_same(name.heap())) {
                *held = self
                    .units
                    .iter()
                    .rev()
                    .flat_map(|held| &held.names)
                    .find(|held| held.as_bytes() == bytes)
                    .copied();
            }
        }
        dead.append(&mut constants);
        dead
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
        self.units.push(Held {
            unit: PublishedUnit::downgrade(unit),
            declares: !(unit.functions.is_empty()
                && unit.dynamic.is_empty()
                && unit.classes.is_empty()),
            main: self.spare.pop().unwrap_or_default(),
            constants: self.spare.pop().unwrap_or_default(),
            names: self.spare.pop().unwrap_or_default(),
            named: false,
        });
        self.units.len() - 1
    }
}

impl Request {
    /// Whether `class` is one a unit compiled for this request alone
    /// declared, an `eval`'s or an uncached include's, whose constant
    /// strings are its own rather than the engine's.
    pub(crate) fn declared_for_the_request(&self, class: crate::api::values::ClassId) -> bool {
        self.declared_row(class).is_some_and(|row| {
            let unit = PublishedUnit::as_ptr(&row.unit);
            (self.unit_strings.units.iter()).any(|held| std::ptr::eq(held.unit.as_ptr(), unit))
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
            if !seen.insert(name) {
                continue;
            }
            let Some(at) = made.iter().rposition(|s| s.as_bytes() == name) else {
                continue;
            };
            let s = made.swap_remove(at);
            held.names.push(s);
            match strings.by_bytes.get_mut(name) {
                Some(named) => *named = Some(s),
                None => {
                    strings.by_bytes.insert(name.into(), Some(s));
                }
            }
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
                names.iter().any(|made| made.as_bytes() == name)
            } else {
                if seen.is_empty() {
                    seen.extend(names.iter().map(|made| made.as_bytes().to_vec()));
                }
                !seen.insert(name.to_vec())
            };
            if again {
                continue;
            }
            let made = self.string(name)?;
            match self.unit_strings.by_bytes.get_mut(name) {
                Some(held) => *held = Some(made),
                None => {
                    self.unit_strings.by_bytes.insert(name.into(), Some(made));
                }
            }
            if let Some(held) = self.unit_strings.units.get_mut(at) {
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
        if let Some(held) = self.unit_strings.by_bytes.get(bytes).copied().flatten() {
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
        if self.unit_strings.units.is_empty() {
            return;
        }
        let ended = ended.map(PublishedUnit::as_ptr);
        let mut dead = self.unit_strings.take(ended);
        for s in dead.drain(..) {
            self.release(Value::string(s));
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
                held.names
                    .into_iter()
                    .chain(held.main)
                    .chain(held.constants)
            })
            .chain(held.compiling.made);
        for s in strings {
            self.release(Value::string(s));
        }
    }
}
