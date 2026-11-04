//! A unit persisted for the cache (design §6.3): its tables laid in one
//! block at their length, as opcache lays a cached file into its memory.
//!
//! php sizes a cached file and then copies it (`zend_accel_script_persist_calc`
//! and `zend_accel_script_persist`, `Z/ext/opcache/zend_persist.c:1473`),
//! storing each name once, interned (`zend_accel_store_interned_string`).
//! [`Unit::persist`] does the same over the tables whose elements are
//! plain values, every op array's oplines, literals, `try`/`catch`
//! entries, live ranges, dynamic definitions, nested declarations and
//! numeric dimensions, and over the bytes of every name the unit's op
//! arrays and declarations hold, each distinct name laid once and named
//! by a share wherever the unit names it. The tables that hold a tree,
//! the declarations, the constant expressions, the variables' names and
//! the bound variables, are each copied into an allocation of exactly
//! their length, since what they hold owns memory of its own and a block
//! holds only what it never has to drop.

use std::collections::HashMap;

use php_heap::{Persist, PersistSize};

use crate::decl::{ClassDecl, FunctionDecl, List, MethodDecl, Name};
use crate::op::OpArray;
use crate::unit::Unit;

impl Unit {
    /// The unit persisted for every request the cache serves it to: its
    /// handlers resolved, as [`Unit::published`] would resolve them, so
    /// nothing writes a table once it is laid; its plain tables and its
    /// names laid in one block of the size a first pass counted; and
    /// every other table copied at its length. A unit
    /// one request compiles for itself, an `eval`'s, is never persisted:
    /// its tables stay the compiler's vectors, as php leaves an op array
    /// it compiles outside opcache in the request's arena.
    pub fn persist(&mut self) {
        for code in self.op_arrays_mut() {
            code.resolve();
        }
        let mut names = Vec::new();
        self.each_name(&mut |name| names.push(std::mem::take(name)));
        let mut distinct = HashMap::<&[u8], usize>::with_capacity(names.len());
        let mut order = Vec::with_capacity(names.len());
        let mut size = PersistSize::default();
        for name in &names {
            let next = distinct.len();
            let at = *distinct.entry(&name.0).or_insert_with(|| {
                size.bytes(name.0.len());
                next
            });
            order.push(at);
        }
        for code in self.op_arrays_mut() {
            code.persist_calc(&mut size);
        }
        let mut persist = Persist::new(size);
        for code in self.op_arrays_mut() {
            code.persist(&mut persist);
        }
        let mut laid = vec![Name::default(); distinct.len()];
        for (bytes, at) in distinct {
            if let Some(slot) = laid.get_mut(at) {
                *slot = Name(persist.bytes(bytes));
            }
        }
        let mut order = order.into_iter();
        self.each_name(&mut |name| {
            if let Some(at) = order.next().and_then(|at| laid.get(at)) {
                name.clone_from(at);
            }
        });
        self.exact();
    }

    /// Every name the unit's op arrays and declarations hold, in one
    /// order: each op array's file and variables, then the declarations'.
    fn each_name(&mut self, visit: &mut impl FnMut(&mut Name)) {
        for code in self.op_arrays_mut() {
            visit(&mut code.filename);
            code.vars.iter_mut().for_each(&mut *visit);
            for bound in &mut code.bound_vars {
                visit(&mut bound.name);
            }
        }
        let Unit {
            functions,
            dynamic,
            classes,
            constants,
            runtime_keys,
            ..
        } = self;
        for (decl, _) in functions.iter_mut().chain(dynamic.iter_mut()) {
            function_names(decl, visit);
        }
        for class in classes {
            class_names(class, visit);
        }
        for constant in constants {
            visit(&mut constant.name);
        }
        for (name, _) in runtime_keys {
            visit(name);
        }
    }

    /// Every table of the unit that is not laid in its block copied into
    /// an allocation at its length, the declarations' within them.
    fn exact(&mut self) {
        for code in self.op_arrays_mut() {
            exact(&mut code.vars);
            exact(&mut code.const_exprs);
            exact(&mut code.bound_vars);
        }
        exact(&mut self.functions);
        exact(&mut self.dynamic);
        exact(&mut self.methods);
        exact(&mut self.hooks);
        exact(&mut self.runtime_keys);
        exact(&mut self.top_level);
        exact(&mut self.constants);
        exact(&mut self.completed);
        exact(&mut self.diagnostics);
        exact(&mut self.arms);
        let decls = self.functions.iter_mut().chain(self.dynamic.iter_mut());
        for (decl, _) in decls {
            exact_function(decl);
        }
        for class in &mut self.classes {
            exact_list(&mut class.interfaces);
            exact_list(&mut class.implementors);
            exact_list(&mut class.aliases);
            exact(&mut class.traits);
            exact(&mut class.adaptations);
            exact(&mut class.constants);
            exact_list(&mut class.properties);
            exact(&mut class.methods);
            exact_list(&mut class.attributes);
            for method in &mut class.methods {
                exact_function(&mut method.function);
            }
            if class.properties.as_borrowed().is_none() {
                for property in &mut class.properties {
                    exact_list(&mut property.attributes);
                }
            }
            for constant in &mut class.constants {
                exact_list(&mut constant.attributes);
            }
        }
        exact(&mut self.classes);
    }
}

/// `table` copied into an allocation of exactly its length, where it
/// holds room past it: a fresh allocation and not a shrink in place,
/// which the allocator may answer by keeping the block it had.
fn exact<T>(table: &mut Vec<T>) {
    if table.capacity() == table.len() {
        return;
    }
    let mut at_length = Vec::with_capacity(table.len());
    at_length.append(table);
    *table = at_length;
}

/// A list a compile made at its length; a native's, which borrows its
/// entry, has no room past it.
fn exact_list<T: Clone>(list: &mut List<T>) {
    if list
        .as_borrowed()
        .is_some_and(|entries| !entries.is_empty())
    {
        return;
    }
    exact(list.to_mut());
}

/// A function's tables at their length.
fn exact_function(decl: &mut FunctionDecl) {
    exact_list(&mut decl.attributes);
    exact_list(&mut decl.frameless);
}

/// A function's names: its own, its file's and its parameters'.
fn function_names(decl: &mut FunctionDecl, visit: &mut impl FnMut(&mut Name)) {
    visit(&mut decl.name);
    visit(&mut decl.file);
    for parameter in &mut *decl.parameters {
        visit(&mut parameter.name);
    }
}

/// A class's names: its own, its parent's and interfaces', its file's,
/// and its members'.
fn class_names(class: &mut ClassDecl, visit: &mut impl FnMut(&mut Name)) {
    visit(&mut class.name);
    if let Some(parent) = &mut class.parent {
        visit(parent);
    }
    class.interfaces.iter_mut().for_each(&mut *visit);
    visit(&mut class.file);
    for constant in &mut class.constants {
        visit(&mut constant.name);
    }
    for property in &mut class.properties {
        visit(&mut property.name);
        let hooks = property.hooks.as_deref_mut();
        let hooks = hooks.into_iter().flat_map(|h| [&mut h.get, &mut h.set]);
        for hook in hooks.flatten() {
            function_names(&mut hook.function, visit);
        }
    }
    for MethodDecl { function, .. } in &mut class.methods {
        function_names(function, visit);
    }
}

impl OpArray {
    /// The plain tables counted for the block, as
    /// `zend_persist_op_array_calc_ex` counts them.
    pub(crate) fn persist_calc(&self, size: &mut PersistSize) {
        size.table::<crate::Opline>(self.oplines.len());
        size.table::<php_heap::Zval>(self.literals.len());
        size.table::<crate::TryCatch>(self.try_catch.len());
        size.table::<crate::LiveRange>(self.live_ranges.len());
        size.table::<u32>(self.dynamic_func_defs.len());
        size.table::<crate::Nested>(self.nested.len());
        size.table::<crate::ConstIdx>(self.numeric_dims.len());
    }

    /// The plain tables laid in the block, as `zend_persist_op_array_ex`
    /// copies them, and the safepoints shrunk to what they hold.
    pub(crate) fn persist(&mut self, persist: &mut Persist) {
        self.oplines = persist.table(&self.oplines);
        self.literals = persist.table(&self.literals);
        self.try_catch = persist.table(&self.try_catch);
        self.live_ranges = persist.table(&self.live_ranges);
        self.dynamic_func_defs = persist.table(&self.dynamic_func_defs);
        self.nested = persist.table(&self.nested);
        self.numeric_dims = persist.table(&self.numeric_dims);
        self.safepoints.shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use crate::decl::Name;
    use crate::op::{OpArray, Opline};
    use crate::unit::{MethodBody, Unit};

    /// A persisted unit's plain tables and names are laid in one block,
    /// none apart, every name the unit repeats laid once and shared; its
    /// other tables are at their length; and it reads as it did.
    #[test]
    fn a_persisted_unit_is_laid_in_one_block_at_its_length() {
        let file = || Name(b"/srv/app/src/Kernel.php".as_slice().into());
        let grown = || {
            let mut vars = Vec::with_capacity(8);
            vars.push(Name(b"this".as_slice().into()));
            let mut oplines = Vec::with_capacity(16);
            oplines.push(Opline::default());
            let opcode = crate::Opcode::Return;
            oplines.push(Opline {
                opcode,
                ..Opline::default()
            });
            OpArray {
                oplines: oplines.into(),
                literals: vec![php_heap::Zval::long(1)].into(),
                vars,
                filename: file(),
                ..OpArray::default()
            }
        };
        let mut methods = Vec::with_capacity(4);
        methods.push(MethodBody {
            class: 0,
            method: 0,
            body: grown(),
        });
        let mut unit = Unit {
            main: grown(),
            methods,
            ..Unit::default()
        };
        let mut before = unit.clone();
        unit.persist();
        for code in before.op_arrays_mut() {
            code.resolve();
        }
        assert_eq!(unit, before, "a persisted unit reads as it did, resolved");
        let unit = unit.published(None);
        let (main, method) = (&unit.main, &unit.methods[0].body);
        for code in [main, method] {
            let laid = code.oplines.is_laid() && code.literals.is_laid();
            assert!(laid, "publication writes nothing of a persisted unit");
            assert_eq!(code.oplines.capacity(), 2);
            assert_eq!(code.vars.capacity(), 1);
        }
        assert_eq!(unit.methods.capacity(), 1);
        assert_eq!(
            main.filename.0.as_ptr(),
            method.filename.0.as_ptr(),
            "a name the unit repeats is laid once"
        );
        let base = main.oplines.as_ptr().addr();
        let distance = method.filename.0.as_ptr().addr().abs_diff(base);
        assert!(distance < 256, "one block holds the tables and the names");
    }
}
