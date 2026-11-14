//! The `static` variables of a request: each function's and main op
//! array's `ZEND_MAP_PTR(static_variables_ptr)`, which the published
//! unit never holds (design §5.14), a closure's own, the table
//! `ReflectionFunction::getStaticVariables()` reads, and their release
//! at the request's end. The tables are [`StaticVars`]'s, whose field no
//! file but this one reads.

use std::collections::HashMap;

use php_heap::FrameRef;
use php_unit::{MapPtrOffset, OpArray, PublishedUnit};

use crate::api::call::{Function, Target};
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, PhpArray, PhpObject, TypeTag, Value};
use crate::exec;

use super::Request;

/// Whose `static` table the request holds: a linked function's, the
/// `zend_function` a request's function table or a class's method table
/// holds, or a file's main op array's, by its map pointer where opcache
/// loaded it and by the request's number for the op array where it was
/// compiled afresh ([`exec::FileStatics`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Statics {
    Function(Function),
    Main(php_unit::MapPtrOffset),
    Own(u32),
}

/// Where the `static` variables of what a frame runs stand: in a
/// closure's object, or in one of the request's tables.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StaticsAt {
    Closure(PhpObject),
    Table(Statics),
}

/// The `static` variables' tables, whose field no file but this one
/// reads.
#[derive(Default)]
pub(super) struct StaticVars {
    /// `ZEND_MAP_PTR(op_array->static_variables_ptr)` for every function
    /// and main op array whose `static` variables the request has
    /// touched: the values, one per entry of the op array's
    /// [`php_unit::OpArray::bound_vars`], which the published unit never
    /// holds (design §5.14). A closure's are its object's.
    tables: HashMap<Statics, PhpArray>,
    /// How many of `tables` are a main op array's, which a file's frame
    /// gives up as it leaves ([`Request::destroy_file_statics`]) and
    /// which most requests have none of.
    mains: usize,
    /// The number the next op array compiled afresh takes, never handed
    /// out twice, so a table a fatal left standing is no other's.
    next_own: u32,
}

impl Request {
    /// `ZEND_MAP_PTR_GET(closure->func.op_array.static_variables_ptr)`
    /// beside the names the op array carries: what the closure bound,
    /// name and value, in the order the table took them, with a slot
    /// nothing filled left out, and with `lexical` the `use` variables
    /// alone. A closure made of a named function shares that function's
    /// table (`zend_create_closure_ex` under `is_fake`), which is read
    /// where the request has laid it down and at its starting values
    /// where it has not. Empty for an object that is no closure and for
    /// a closure whose op array binds nothing.
    pub(crate) fn closure_bound(&self, object: PhpObject, lexical: bool) -> Vec<(Vec<u8>, Value)> {
        let Some(closure) = object.closure(self) else {
            return Vec::new();
        };
        let Some(user) = self.user_function(closure.function) else {
            return Vec::new();
        };
        let Some(code) = exec::body_of(user.unit(), user.body()) else {
            return Vec::new();
        };
        let table = if closure.fake {
            self.static_vars
                .tables
                .get(&Statics::Function(closure.function))
                .copied()
        } else {
            closure.bound
        };
        let values = exec::statics_values(code, table);
        code.bound_vars
            .iter()
            .zip(values.iter())
            .filter(|(var, value)| value.tag() != TypeTag::Undef && (var.lexical || !lexical))
            .map(|(var, value)| (var.name.0.to_vec(), *value))
            .collect()
    }

    /// The `static` variables of what `frame` runs, php's
    /// `ZEND_MAP_PTR_GET(EX(func)->op_array.static_variables_ptr)`: a
    /// closure's own table, which its object holds; a function's or a
    /// method's, which the request holds per linked function, so a
    /// method a trait gave two classes counts apart in each and an
    /// inherited one counts with its parent's; or, for the main op array
    /// of a file, the request's table for that op array.
    pub(crate) fn statics_of(&self, frame: FrameRef, code: &OpArray) -> StaticsAt {
        let Some(state) = self.vm.at(frame) else {
            return StaticsAt::Table(Statics::Main(code.runtime_cache));
        };
        let file = |annex: Option<exec::AnnexId>| match annex
            .and_then(|id| self.annexes().get(id)?.statics)
        {
            Some(exec::FileStatics::Own(number)) => Statics::Own(number),
            _ => Statics::Main(code.runtime_cache),
        };
        if let Some(object) = state.closure()
            && object.closure(self).is_some_and(|closure| !closure.fake)
        {
            return StaticsAt::Closure(object);
        }
        match state.function() {
            Some(function) => StaticsAt::Table(Statics::Function(function)),
            None => StaticsAt::Table(file(state.annex())),
        }
    }

    /// The value at `slot` of a `static` table, the table laid down from
    /// `code`'s starting values where the request has not touched it
    /// yet, which is `zend_array_dup(op_array->static_variables)` at the
    /// first `BIND_STATIC`. None for a slot the table has not.
    pub(crate) fn static_at(
        &mut self,
        at: StaticsAt,
        code: &OpArray,
        slot: usize,
    ) -> Result<Option<Value>, Thrown> {
        let table = self.static_table(at, code)?;
        Ok(table
            .zip(u32::try_from(slot).ok())
            .and_then(|(table, slot)| table.at_position(slot))
            .map(|(value, _)| value))
    }

    /// The `static` table `at` names, laid down from `code`'s starting
    /// values where the request has not touched a function's yet.
    fn static_table(&mut self, at: StaticsAt, code: &OpArray) -> Result<Option<PhpArray>, Thrown> {
        Ok(match at {
            StaticsAt::Closure(object) => object.closure(self).and_then(|closure| closure.bound),
            StaticsAt::Table(of) => {
                if let Some(table) = self.static_vars.tables.get(&of) {
                    Some(*table)
                } else {
                    let table = exec::starting_statics(self, code)?;
                    if let Some(table) = table
                        && self.static_vars.tables.insert(of, table).is_none()
                        && !matches!(of, Statics::Function(_))
                    {
                        self.static_vars.mains += 1;
                    }
                    table
                }
            }
        })
    }

    /// `value` into `slot` of a `static` table, which takes the owner
    /// the caller held; what stood there is handed back, since the table
    /// only ever trades a value for the reference made over it.
    pub(crate) fn set_static(&mut self, at: StaticsAt, slot: usize, value: Value) -> Value {
        let table = match at {
            StaticsAt::Closure(object) => object.closure(self).and_then(|closure| closure.bound),
            StaticsAt::Table(of) => self.static_vars.tables.get(&of).copied(),
        };
        let place = table
            .zip(u32::try_from(slot).ok())
            .and_then(|(table, slot)| table.at_position(slot));
        match place {
            Some((previous, place)) => {
                place.heap().write(value.to_zval());
                previous
            }
            None => value,
        }
    }

    /// The `static` variables `callable` reads, name and value, as
    /// `ReflectionFunctionAbstract::getStaticVariables` copies them: a
    /// closure's own table, and a function's or a method's laid down
    /// from its starting values where no call has yet, as php lays it
    /// down there. Empty for a native and for a function with none.
    pub(crate) fn static_variables(&mut self, target: Target) -> Vec<(Vec<u8>, Value)> {
        let function = match target {
            Target::Closure(object) => match object.closure(self).map(|c| (c.fake, c.function)) {
                Some((true, function)) => function,
                _ => return self.closure_bound(object, false),
            },
            Target::Function(function) | Target::Method { function, .. } => function,
        };
        let Some((unit, body)) = self
            .user_function(function)
            .map(|user| (php_unit::PublishedUnit::clone(user.unit()), user.body()))
        else {
            return Vec::new();
        };
        let Some(code) = exec::body_of(&unit, body) else {
            return Vec::new();
        };
        let at = StaticsAt::Table(Statics::Function(function));
        let Ok(table) = self.static_table(at, code) else {
            return Vec::new();
        };
        code.bound_vars
            .iter()
            .zip(exec::statics_values(code, table))
            .map(|(var, value)| (var.name.0.to_vec(), value))
            .collect()
    }

    /// Where the frame about to serve `unit`'s main op array finds its
    /// `static` table: the map pointer, where opcache loaded the unit
    /// (`zend_accel_load_script`'s copy keeps the persisted op array's
    /// offset), and otherwise a table of the frame's own, as php's
    /// `zend_compile` gives every op array its own pointer. A file the
    /// cache holds is one op array whose pointer every include of it
    /// shares, so an include of it entered while another stands counts
    /// on in the same table, and the first of them to leave starts it
    /// over ([`Request::destroy_file_statics`]); a file compiled afresh
    /// at each include has a table of its own each time:
    ///
    /// ```php
    /// $depth = ($depth ?? 0) + 1;
    /// static $calls = 0;
    /// $calls++;
    /// if ($depth < 3) { include __FILE__; include __FILE__; }
    /// echo $depth, ":", $calls, "\n";
    /// $depth--;
    /// ```
    /// ```expect regime=jit-off
    /// 3:3
    /// 3:1
    /// 2:1
    /// 3:2
    /// 3:1
    /// 2:1
    /// 1:1
    /// ```
    ///
    /// ```php
    /// $depth = ($depth ?? 0) + 1;
    /// static $calls = 0;
    /// $calls++;
    /// if ($depth < 3) { include __FILE__; include __FILE__; }
    /// echo $depth, ":", $calls, "\n";
    /// $depth--;
    /// ```
    /// ```expect
    /// 3:1
    /// 3:1
    /// 2:1
    /// 3:1
    /// 3:1
    /// 2:1
    /// 1:1
    /// ```
    pub(super) fn file_statics(&mut self, unit: &PublishedUnit) -> exec::FileStatics {
        let loaded = (self
            .accel_loaded
            .take_if(|(loaded, _)| PublishedUnit::ptr_eq(loaded, unit)))
        .is_some_and(|_| matches!(exec::code_of(unit).runtime_cache, MapPtrOffset::Engine(_)));
        if loaded {
            return exec::FileStatics::Shared { kept: false };
        }
        let number = self.static_vars.next_own;
        self.static_vars.next_own = number.wrapping_add(1);
        exec::FileStatics::Own(number)
    }

    /// `frame`, a file's frame a native serves, keeps the map pointer's
    /// table as it leaves, as `spl_autoload` serves a file
    /// ([`exec::FileStatics::Shared`]).
    pub(super) fn keep_file_statics(&mut self, frame: FrameRef) {
        if let Some(extra) = self.annex_mut(frame)
            && let Some(exec::FileStatics::Shared { kept }) = extra.statics.as_mut()
        {
            *kept = true;
        }
    }

    /// The `static` table a file's frame destroys as it leaves, none for
    /// a function's frame, after a fatal, which unwinds nothing, and
    /// for a frame that keeps its table ([`exec::FileStatics::Shared`]).
    pub(super) fn leaving_statics(&self, annex: Option<exec::AnnexId>) -> Option<Statics> {
        if self.bailout {
            return None;
        }
        let annex = self.annexes().get(annex?)?;
        match annex.statics? {
            exec::FileStatics::Own(number) => Some(Statics::Own(number)),
            exec::FileStatics::Shared { kept: true } => None,
            exec::FileStatics::Shared { kept: false } => Some(Statics::Main(
                exec::code_of(annex.unit.as_ref()?).runtime_cache,
            )),
        }
    }

    /// `zend_destroy_static_vars` of a file's main op array as its frame
    /// leaves (`Z/Zend/zend_vm_def.h:3035`, `Z/Zend/zend.c:1988`): the
    /// table given up and the pointer emptied.
    pub(super) fn destroy_file_statics(&mut self, of: Statics) {
        if self.static_vars.mains == 0 {
            return;
        }
        if let Some(table) = self.static_vars.tables.remove(&of) {
            self.static_vars.mains -= 1;
            self.release(Value::array(table));
        }
    }

    /// The request's `static` tables given up, php's walk over
    /// `EG(function_table)` in `zend_shutdown_executor_values`
    /// (`Z/Zend/zend_execute_API.c:313`), after the symbol table: the
    /// destructors are behind the request by then, so this frees and
    /// serves nothing.
    pub(crate) fn destroy_static_vars(&mut self) {
        self.static_vars.mains = 0;
        for (_, table) in std::mem::take(&mut self.static_vars.tables) {
            self.release(Value::array(table));
        }
    }

    /// `func->common.scope` of a closure's own copy of its function: the
    /// class the closure was written in, which a backtrace names before
    /// the closure's own name. None where the call stands on no closure
    /// or the closure was made outside a class.
    pub(super) fn closure_scope(&self, closure: Option<PhpObject>) -> Option<ClassId> {
        closure?.closure(self)?.scope
    }
}
