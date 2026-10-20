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
    spare: Spare,
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
    compiling: Compiling,
}

/// The vectors a unit's strings and arrays stood in, kept for the next
/// unit's.
#[derive(Default)]
struct Spare {
    names: Vec<Vec<PhpString>>,
    constants: Vec<Vec<Zval>>,
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
}

/// What holds a request-only unit's declarations, and so its strings,
/// past its main op array's end.
enum Past {
    /// The closures over a function it declared that stand, each one
    /// more count of the op array, `(*op_array->refcount)++` in
    /// `zend_create_closure_ex`, given back by `zend_closure_free_storage`.
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

/// `made`, the strings and arrays a compile of `unit` made, split
/// between its main op array's, put in `main`, and what it declared, put
/// in `declared`, as php's compile leaves each in the op array whose
/// constant or name it is. A string or an array a function's, a
/// closure's, a method's or a hook's constants (`op_array->literals`) or
/// static variables hold is the declaration's, and so is a string written
/// as one of their variables' names or a declared property's name, which
/// `zend_compile` keeps in `op_array->vars` and `prop_info->name`; every
/// other is the main op array's, its constants, its variables' names and
/// what a fold made on the way, which php's compile frees as it goes. A
/// constant is told by its allocation, not its bytes, since two written
/// alike are two strings.
fn main_constants(
    unit: &Unit,
    made: &mut Vec<Zval>,
    main: &mut Vec<Zval>,
    declared: &mut Vec<Zval>,
) {
    let bodies = || {
        (unit.functions.iter().chain(&unit.dynamic))
            .map(|(_, body)| body)
            .chain(unit.methods.iter().map(|method| &method.body))
            .chain(unit.hooks.iter().map(|hook| &hook.body))
    };
    let constants = || {
        bodies().flat_map(|body| {
            (body.literals.iter().copied()).chain(body.bound_vars.iter().map(|bound| bound.value))
        })
    };
    let mut held: Vec<Str> = constants().filter_map(Zval::as_str).collect();
    held.sort_unstable_by(|a, b| a.bytes().cmp(b.bytes()));
    let arrays: Vec<Zval> = constants().filter(|z| z.as_arr().is_some()).collect();
    let names = declared_names(unit);
    for z in made.drain(..) {
        let theirs = match z.as_str() {
            Some(s) => {
                let bytes = s.bytes();
                let from = held.partition_point(|held| held.bytes() < bytes);
                (held.get(from..).unwrap_or_default().iter())
                    .take_while(|held| held.bytes() == bytes)
                    .any(|held| held.is_same(s))
                    || names.contains(bytes)
            }
            None => arrays.iter().any(|held| held.is_same_value(z)),
        };
        if theirs {
            declared.push(z);
        } else {
            main.push(z);
        }
    }
}

/// The names `unit`'s declarations are read by beside their op arrays'
/// ([`php_compile::op_array_names`]), as [`php_compile::intern_declared`]
/// makes them: each declared function's, closure's, method's and hook's
/// variables' (`op_array->vars`) and each declared property's, under the
/// key `prop_info->name` is too, mangled for a protected or a private
/// one. The main op array's variables are not among them.
fn declared_names(unit: &Unit) -> HashSet<Vec<u8>, NameHasher> {
    let bodies = (unit.functions.iter().chain(&unit.dynamic))
        .map(|(_, body)| body)
        .chain(unit.methods.iter().map(|method| &method.body))
        .chain(unit.hooks.iter().map(|hook| &hook.body));
    let mut names: HashSet<Vec<u8>, NameHasher> = HashSet::with_hasher(NameHasher);
    names.extend(bodies.flat_map(|body| body.vars.iter().map(|var| var.0.to_vec())));
    for class in &unit.classes {
        for property in &class.properties {
            names.insert(property.name.0.to_vec());
            if let Some(prefix) = php_compile::mangling(property.visibility, &class.name.0) {
                names.insert([prefix.as_slice(), &property.name.0].concat());
            }
        }
    }
    names
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

impl UnitStrings {
    /// The string a request-only unit that lives holds for the name
    /// `bytes`, else the one the request keeps for it to its end
    /// ([`Request::kept_string`]); a view, no owner taken.
    pub(super) fn find(&self, bytes: &[u8]) -> Option<PhpString> {
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

    /// `s`, a string of the request's with one owner, the kept string
    /// for `bytes` to the request's end ([`Request::kept_string`]).
    pub(super) fn keep(&mut self, bytes: &[u8], s: PhpString) {
        self.kept.insert(bytes.into(), s);
    }

    /// The compile under way, which the home its constants are made in
    /// keeps them in ([`super::memory::RequestStrings`]).
    pub(super) fn compiling_mut(&mut self) -> &mut Compiling {
        &mut self.compiling
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
        self.units.retain_mut(|held| {
            let alive = held.unit.strong_count() > 0;
            let ends = ended == Some(held.unit.as_ptr());
            if alive && !ends && !held.over {
                return true;
            }
            held.over = true;
            constants.append(&mut held.main);
            found.append(&mut held.found_main);
            if alive && !matches!(held.past, Past::Closures(0)) {
                return true;
            }
            dead.append(&mut held.names);
            files.extend(held.file.take());
            constants.append(&mut held.constants);
            found.append(&mut held.found);
            spare.names.push(std::mem::take(&mut held.names));
            spare.names.push(std::mem::take(&mut held.found));
            spare.names.push(std::mem::take(&mut held.found_main));
            spare.constants.push(std::mem::take(&mut held.constants));
            spare.constants.push(std::mem::take(&mut held.main));
            false
        });
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
                    .flat_map(|held| held.names.iter().chain(&held.found).chain(&held.found_main))
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
            past: if unit.functions.is_empty() && unit.classes.is_empty() {
                Past::Closures(0)
            } else {
                Past::Tables
            },
            over: false,
            main: self.spare.constants.pop().unwrap_or_default(),
            constants: self.spare.constants.pop().unwrap_or_default(),
            names: self.spare.names.pop().unwrap_or_default(),
            named: false,
            file: None,
            found_main: self.spare.names.pop().unwrap_or_default(),
            found: self.spare.names.pop().unwrap_or_default(),
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
        // ([`Request::declared_string`]) while the unit lives; an
        // `eval`'s made none, and they are made as it is noted
        // ([`Request::hold_unit_names`]). A compile that made no string
        // has none of them to find.
        if !made.is_empty() {
            let mut seen = HashSet::with_hasher(NameHasher);
            for name in php_compile::op_array_names(unit) {
                let Some(at) = made
                    .iter()
                    .rposition(|z| z.as_str().is_some_and(|s| s.bytes() == name))
                else {
                    continue;
                };
                if !seen.insert(name) {
                    continue;
                }
                let Some(s) = made.swap_remove(at).as_str().map(PhpString::of) else {
                    continue;
                };
                held.names.push(s);
                point(&mut strings.by_bytes, s);
            }
        }
        let (main_from, from) = (held.main.len(), held.constants.len());
        if held.declares {
            main_constants(unit, made, &mut held.main, &mut held.constants);
        } else {
            held.main.append(made);
        }
        // The strings that stand for a name a declaration is read by,
        // which a copy of one finds while what holds them lives: the main
        // op array's variables' until it ends, the declarations' while
        // the unit lives.
        let main_vars: HashSet<&[u8], NameHasher> =
            unit.main.vars.iter().map(|var| &*var.0).collect();
        for z in held.main.get(main_from..).unwrap_or_default() {
            if let Some(s) = z.as_str().filter(|s| main_vars.contains(s.bytes())) {
                let s = PhpString::of(s);
                held.found_main.push(s);
                point(&mut strings.by_bytes, s);
            }
        }
        if held.constants.len() > from {
            let names = declared_names(unit);
            for z in held.constants.get(from..).unwrap_or_default() {
                if let Some(s) = z.as_str().filter(|s| names.contains(s.bytes())) {
                    let s = PhpString::of(s);
                    held.found.push(s);
                    point(&mut strings.by_bytes, s);
                }
            }
        }
    }

    /// The constant strings a compile the compiler refused made, given up
    /// as `zend_compile`'s refusal destroys the op array it was making.
    pub(crate) fn release_compiled(&mut self) {
        let mut made = std::mem::take(&mut self.unit_strings.compiling.made);
        for z in made.drain(..) {
            self.release(Value::of_zval(z));
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
            let file = (self.unit_strings.units.get(at)).and_then(|held| held.file);
            let again = if file.is_some_and(|file| file.as_bytes() == name) {
                true
            } else if names.len() < SCANNED_NAMES {
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
        // `zend_string_release`: a string is no possible root, and
        // destroying one runs nothing.
        for s in dead.drain(..) {
            self.heap.release_string(s.heap());
        }
        for z in constants.drain(..) {
            self.release(Value::of_zval(z));
        }
        self.unit_strings.dead = dead;
        self.unit_strings.dead_constants = constants;
    }

    /// `zend_create_closure_ex`'s `(*op_array->refcount)++`: one more
    /// closure over `function`, which holds the strings of the
    /// request-only unit that declared it.
    pub(crate) fn count_closure(&mut self, function: crate::api::call::Function) {
        if let Some(Held {
            past: Past::Closures(count),
            ..
        }) = self.closure_unit(function)
        {
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
    pub(crate) fn uncount_closure(&mut self, function: crate::api::call::Function) {
        let Some(Held {
            past: Past::Closures(count),
            over,
            ..
        }) = self.closure_unit(function)
        else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 && *over {
            self.release_unit_strings(None);
        }
    }

    /// The request-only unit `function` is a function of, where it is one:
    /// none at once where the request holds no such unit, the common
    /// program's every closure, or where the function is not one of the
    /// request's own, whose unit is the engine's or the cache's.
    fn closure_unit(&mut self, function: crate::api::call::Function) -> Option<&mut Held> {
        if self.unit_strings.units.is_empty() {
            return None;
        }
        let user = self.user_functions.get(function.local_at()?)?;
        let unit = PublishedUnit::as_ptr(&user.unit);
        (self.unit_strings.units.iter_mut()).find(|held| std::ptr::eq(held.unit.as_ptr(), unit))
    }

    /// Every request-only unit's strings given up at the request's end,
    /// as `shutdown_executor` destroys the op arrays that hold them.
    pub(crate) fn release_all_unit_strings(&mut self) {
        let held = std::mem::take(&mut self.unit_strings);
        for s in held.kept.into_values() {
            self.release(Value::string(s));
        }
        let mut constants = held.compiling.made;
        for held in held.units {
            for s in held.names.into_iter().chain(held.file) {
                self.release(Value::string(s));
            }
            constants.extend(held.main);
            constants.extend(held.constants);
        }
        for z in constants {
            self.release(Value::of_zval(z));
        }
    }
}
