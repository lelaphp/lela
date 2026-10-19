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
//! once per bytes within the unit; an `eval`'s names are made when the
//! unit is noted; and both are given up once nothing holds the unit. A
//! value copied out of a constant takes an owner of its own
//! (`ZVAL_COPY`), as a trace row or a diagnostic that copies a name does
//! (`ZVAL_STR_COPY`), so nothing a unit made is held to the request's end
//! for being made.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use php_heap::Str;
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
    /// The constant strings its compile made, `op_array->literals` and
    /// the names `zend_compile` keeps beside them.
    constants: Vec<PhpString>,
    /// An `eval`'s op array names ([`php_compile::op_array_names`]).
    names: Vec<PhpString>,
    /// Whether the names were made, which a unit's noting does once.
    named: bool,
}

/// The constant strings the compile under way has made, each once by its
/// bytes, which become the unit's when the compile hands it over
/// ([`Request::hold_compiled`]) and are given up when it refuses
/// ([`Request::release_compiled`]).
#[derive(Default)]
pub(crate) struct Compiling {
    made: Vec<PhpString>,
    by_bytes: HashSet<ByBytes, NameHasher>,
}

impl Compiling {
    /// The string this compile made for `bytes`, if it made one.
    pub(super) fn find(&self, bytes: &[u8]) -> Option<Str> {
        self.by_bytes.get(bytes).map(|held| held.0)
    }

    /// `s`, a string this compile made with one owner, kept for the unit.
    pub(super) fn add(&mut self, s: Str) {
        self.by_bytes.insert(ByBytes(s));
        self.made.push(PhpString::of(s));
    }
}

/// A string keyed by its bytes, so a set of them is asked by bytes.
#[derive(Clone, Copy)]
struct ByBytes(Str);

impl PartialEq for ByBytes {
    fn eq(&self, other: &ByBytes) -> bool {
        self.0.bytes() == other.0.bytes()
    }
}

impl Eq for ByBytes {}

impl Hash for ByBytes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.bytes().hash(state);
    }
}

impl Borrow<[u8]> for ByBytes {
    fn borrow(&self) -> &[u8] {
        self.0.bytes()
    }
}

impl UnitStrings {
    /// The compile under way, which the home its constants are made in
    /// keeps them in ([`super::memory::RequestStrings`]).
    pub(super) fn compiling_mut(&mut self) -> &mut Compiling {
        &mut self.compiling
    }

    /// The units that are over taken out with their strings: each unit
    /// nothing holds any longer, and `ended`, a unit whose main op array
    /// has finished and that declared nothing a later call could reach,
    /// whatever share of it the loop still holds for the opline that
    /// returned. Each name one of those strings stood for is pointed at
    /// an equal string a living unit holds, or at none.
    fn take(&mut self, ended: Option<*const Unit>) -> Vec<PhpString> {
        let mut dead = std::mem::take(&mut self.dead);
        let mut constants = Vec::new();
        let spare = &mut self.spare;
        self.units.retain_mut(|held| {
            if held.unit.strong_count() > 0 && ended != Some(held.unit.as_ptr()) {
                return true;
            }
            dead.append(&mut held.names);
            constants.append(&mut held.constants);
            spare.push(std::mem::take(&mut held.names));
            spare.push(std::mem::take(&mut held.constants));
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
            constants: self.spare.pop().unwrap_or_default(),
            names: self.spare.pop().unwrap_or_default(),
            named: false,
        });
        self.units.len() - 1
    }
}

impl Request {
    /// The constant strings the compile just made, given to `unit`, the
    /// unit it made, and held while the unit lives.
    pub(crate) fn hold_compiled(&mut self, unit: &PublishedUnit) {
        let strings = &mut self.unit_strings;
        strings.compiling.by_bytes.clear();
        let at = strings.held(unit);
        if let Some(held) = strings.units.get_mut(at) {
            held.constants.append(&mut strings.compiling.made);
        }
    }

    /// The constant strings a compile the compiler refused made, given up
    /// as `zend_compile`'s refusal destroys the op array it was making.
    pub(crate) fn release_compiled(&mut self) {
        let mut made = std::mem::take(&mut self.unit_strings.compiling.made);
        self.unit_strings.compiling.by_bytes.clear();
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
    /// unit that is over, its constants and its names: `ended`, the unit of a main op array whose
    /// frame was just popped, where it declared no function, closure or
    /// class (one that did is held by what it declared), and every unit
    /// nothing holds any longer.
    pub(crate) fn release_unit_strings(&mut self, ended: Option<&PublishedUnit>) {
        if self.unit_strings.units.is_empty() {
            return;
        }
        let ended = ended
            .filter(|unit| {
                unit.functions.is_empty() && unit.dynamic.is_empty() && unit.classes.is_empty()
            })
            .map(PublishedUnit::as_ptr);
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
            .flat_map(|held| held.names.into_iter().chain(held.constants))
            .chain(held.compiling.made);
        for s in strings {
            self.release(Value::string(s));
        }
    }
}
