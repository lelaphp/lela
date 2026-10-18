//! The `static` variables of a request: each function's and main op
//! array's `ZEND_MAP_PTR(static_variables_ptr)`, which the published
//! unit never holds (design §5.14), a closure's own, the table
//! `ReflectionFunction::getStaticVariables()` reads, and their release
//! at the request's end. The tables are [`StaticVars`]'s, whose field no
//! file but this one reads.

use std::collections::HashMap;

use php_heap::FrameRef;
use php_unit::OpArray;

use crate::api::call::{Function, Target};
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, PhpArray, PhpObject, TypeTag, Value};
use crate::exec;

use super::Request;

/// Whose `static` table the request holds: a linked function's, the
/// `zend_function` a request's function table or a class's method table
/// holds, or a file's main op array's, by its map pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Statics {
    Function(Function),
    Main(php_unit::MapPtrOffset),
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
    /// How many of `tables` are a main op array's, which every code
    /// frame's push forgets ([`Request::forget_main_statics`]) and which
    /// most requests have none of.
    mains: usize,
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
        let Some(code) = exec::body_of(&user.unit, user.body) else {
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
        if let Some(object) = state.closure
            && object.closure(self).is_some_and(|closure| !closure.fake)
        {
            return StaticsAt::Closure(object);
        }
        match state.function {
            Some(function) => StaticsAt::Table(Statics::Function(function)),
            None => StaticsAt::Table(Statics::Main(code.runtime_cache)),
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
                        && matches!(of, Statics::Main(_))
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
            Target::Closure(object) => {
                let fake = object.closure(self).is_some_and(|closure| closure.fake);
                match object.closure(self).map(|closure| closure.function) {
                    Some(function) if fake => function,
                    _ => return self.closure_bound(object, false),
                }
            }
            Target::Function(function) | Target::Method { function, .. } => function,
        };
        let Some((unit, body)) = self
            .user_function(function)
            .map(|user| (php_unit::PublishedUnit::clone(&user.unit), user.body))
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

    /// The `static` variables of the main op array whose map pointer is
    /// `offset` given up: php compiles a file afresh at each include, so
    /// its main op array's table starts over each time it is entered.
    pub(super) fn forget_main_statics(&mut self, offset: php_unit::MapPtrOffset) {
        if self.static_vars.mains == 0 {
            return;
        }
        if let Some(table) = self.static_vars.tables.remove(&Statics::Main(offset)) {
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
