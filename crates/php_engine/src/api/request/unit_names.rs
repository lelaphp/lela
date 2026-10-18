//! The names a request-only unit's op arrays carry, held as php's
//! compile holds them. A file's unit has its names interned beside its
//! constants as it is published ([`php_compile::intern_names`]), as
//! opcache's persist interns them. An `eval`'s unit, `-r`'s among them,
//! is the request's alone: php's compile makes its file's name and its
//! functions', closures' and classes' names as strings of the request,
//! and `destroy_op_array` releases them with the op array. So here the
//! request makes them when the unit is noted, holds one owner of each
//! while the unit lives, and gives them up once nothing holds the unit,
//! and a trace row or a diagnostic that copies one of them takes one
//! more owner of the unit's string (`ZVAL_STR_COPY`), never a string
//! held to the request's end.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use php_link::NameHasher;
use php_unit::Unit;

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::{PhpString, Value};

/// The names of the request-only units the request holds: each unit's
/// strings, one owner apiece, beside the unit they go with, and the
/// newest string held for each name, where a copy finds it. A name no
/// living unit holds keeps its key with no string, so the next unit of
/// that name, an `eval` in a loop's, makes no key of its own; and the
/// vectors a unit's names stood in are kept for the next unit's.
#[derive(Default)]
pub(crate) struct UnitNames {
    units: Vec<(Weak<Unit>, Vec<PhpString>)>,
    by_bytes: HashMap<Box<[u8]>, Option<PhpString>, NameHasher>,
    spare: Vec<Vec<PhpString>>,
    dead: Vec<PhpString>,
}

/// The most names a unit dedupes by comparing each with the ones it
/// already holds, past which a set of them is made.
const SCANNED_NAMES: usize = 16;

impl UnitNames {
    /// The units that are over taken out with their strings: each unit
    /// nothing holds any longer, and `ended`, a unit whose main op array
    /// has finished and that declared nothing a later call could reach,
    /// whatever share of it the loop still holds for the opline that
    /// returned. Each name one of those strings stood for is pointed at
    /// an equal string a living unit holds, or at none.
    fn take(&mut self, ended: Option<*const Unit>) -> Vec<PhpString> {
        let mut dead = std::mem::take(&mut self.dead);
        let spare = &mut self.spare;
        self.units.retain_mut(|(unit, names)| {
            if unit.strong_count() > 0 && ended != Some(unit.as_ptr()) {
                return true;
            }
            dead.append(names);
            spare.push(std::mem::take(names));
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
                    .flat_map(|(_, names)| names)
                    .find(|held| held.as_bytes() == bytes)
                    .copied();
            }
        }
        dead
    }
}

impl Request {
    /// `zend_compile`'s names for a request-only unit: the file's name
    /// and each name [`php_compile::op_array_names`] lists, made as
    /// strings of the request and held while the unit lives. The `Err`
    /// is the memory limit.
    pub(crate) fn hold_unit_names(&mut self, unit: &Arc<Unit>) -> Result<(), Thrown> {
        if self
            .unit_names
            .units
            .iter()
            .any(|(held, _)| std::ptr::eq(held.as_ptr(), Arc::as_ptr(unit)))
        {
            return Ok(());
        }
        let at = self.unit_names.units.len();
        let names = self.unit_names.spare.pop().unwrap_or_default();
        self.unit_names.units.push((Arc::downgrade(unit), names));
        let mut seen = HashSet::with_hasher(NameHasher);
        for name in php_compile::op_array_names(unit) {
            let held = self
                .unit_names
                .units
                .get(at)
                .map_or(0, |(_, names)| names.len());
            let again = if held < SCANNED_NAMES {
                self.unit_names
                    .units
                    .get(at)
                    .is_some_and(|(_, names)| names.iter().any(|made| made.as_bytes() == name))
            } else {
                if seen.is_empty()
                    && let Some((_, names)) = self.unit_names.units.get(at)
                {
                    seen.extend(names.iter().map(|made| made.as_bytes().to_vec()));
                }
                !seen.insert(name.to_vec())
            };
            if again {
                continue;
            }
            let made = self.string(name)?;
            match self.unit_names.by_bytes.get_mut(name) {
                Some(held) => *held = Some(made),
                None => {
                    self.unit_names.by_bytes.insert(name.into(), Some(made));
                }
            }
            if let Some((_, names)) = self.unit_names.units.get_mut(at) {
                names.push(made);
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
        if let Some(held) = self.unit_names.by_bytes.get(bytes).copied().flatten() {
            self.addref(&Value::string(held));
            return Ok(held);
        }
        self.declared_string(bytes)
    }

    /// `destroy_op_array`'s release of the names of each request-only
    /// unit that is over: `ended`, the unit of a main op array whose
    /// frame was just popped, where it declared no function, closure or
    /// class (one that did is held by what it declared), and every unit
    /// nothing holds any longer.
    pub(crate) fn release_unit_names(&mut self, ended: Option<&Arc<Unit>>) {
        if self.unit_names.units.is_empty() {
            return;
        }
        let ended = ended
            .filter(|unit| {
                unit.functions.is_empty() && unit.dynamic.is_empty() && unit.classes.is_empty()
            })
            .map(Arc::as_ptr);
        let mut dead = self.unit_names.take(ended);
        for name in dead.drain(..) {
            self.release(Value::string(name));
        }
        self.unit_names.dead = dead;
    }

    /// Every request-only unit's names given up at the request's end,
    /// as `shutdown_executor` destroys the op arrays that hold them.
    pub(crate) fn release_all_unit_names(&mut self) {
        let held = std::mem::take(&mut self.unit_names);
        for name in held.units.into_iter().flat_map(|(_, names)| names) {
            self.release(Value::string(name));
        }
    }
}
