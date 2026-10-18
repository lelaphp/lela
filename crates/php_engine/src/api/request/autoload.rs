//! The autoload area of a request: `spl_autoload_register()` and
//! `spl_autoload_unregister()` over the request's autoloaders, the
//! loaders a lookup that may autoload walks, `EG(in_autoload)`, which
//! keeps one name from loading itself twice, and
//! `spl_autoload_functions()`. The loaders and `EG(in_autoload)` are
//! [`Autoload`]'s, whose fields no file but this one reads.

use crate::api::abi::Callable;
use crate::api::call::Target;
use crate::api::raising::Thrown;
use crate::api::values::{Key, Owner, PhpArray, PhpString, Value};

use super::Request;

/// The autoloaders and `EG(in_autoload)`, whose fields no file but
/// this one reads.
#[derive(Default)]
pub(super) struct Autoload {
    autoloaders: Vec<Owner<Callable>>,
    /// `EG(in_autoload)`: the lowered names an autoloader is loading,
    /// each a key of php's table, which the first lookup that may
    /// autoload lays down on the request's heap and which stands, its
    /// data with it, until the request ends.
    in_autoload: Option<PhpArray>,
}

impl Request {
    /// Every autoloader given up, in registration order, as
    /// `PHP_RSHUTDOWN(spl)` destroys `spl_autoload_functions`.
    pub(super) fn free_autoloaders(&mut self) {
        for loader in std::mem::take(&mut self.autoload.autoloaders) {
            loader.give_up(self);
        }
    }

    /// `spl_autoload_register()`: the callable onto the stack, at its
    /// head under `prepend`, and nothing where one of the same target
    /// is there already (`spl_find_registered_function`). The stack
    /// takes an [`Owner`] of the callable, as
    /// `autoload_func_info_from_fci` takes one of its object and closure,
    /// and gives them up when the callable leaves the stack or the
    /// request ends.
    /// A `Class::method` string registered with `$this` in scope is
    /// served on that object after the expression that made it is gone:
    ///
    /// ```php
    /// class Test {
    ///     function register() { spl_autoload_register('Test::load'); }
    ///     function load($name) { echo "load $name\n"; var_dump($this); }
    ///     function __destruct() { echo "destructed\n"; }
    /// }
    /// (new Test)->register();
    /// $a = [1, 2, 3];
    /// var_dump(class_exists('Missing'));
    /// echo "end\n";
    /// ```
    /// ```expect
    /// load Missing
    /// object(Test)#1 (0) {
    /// }
    /// bool(false)
    /// end
    /// destructed
    /// ```
    pub fn autoload_register(&mut self, loader: Callable, prepend: bool) -> Result<(), Thrown> {
        if self
            .autoload
            .autoloaders
            .iter()
            .any(|held| held.target() == loader.target())
        {
            return Ok(());
        }
        let loader = Owner::take(self, loader);
        if prepend {
            self.autoload.autoloaders.insert(0, loader);
        } else {
            self.autoload.autoloaders.push(loader);
        }
        Ok(())
    }

    /// `spl_autoload_unregister()`.
    pub fn autoload_unregister(&mut self, loader: &Callable) -> bool {
        let Some(at) = self
            .autoload
            .autoloaders
            .iter()
            .position(|l| l.target() == loader.target())
        else {
            return false;
        };
        self.autoload.autoloaders.remove(at).give_up(self);
        true
    }

    /// The autoloader at `at` on the stack, what `spl_perform_autoload`
    /// serves next.
    pub(crate) fn autoloader(&self, at: usize) -> Option<Callable> {
        self.autoload
            .autoloaders
            .get(at)
            .map(|loader| (**loader).clone())
    }

    /// `zend_hash_add_empty_element(EG(in_autoload), lc_name)`, the
    /// table laid down first where this is the request's first lookup
    /// that may autoload (`ALLOC_HASHTABLE`, `zend_hash_init(8)`): the
    /// lowered name as the request string php's `lc_name` is, the one
    /// standing where the caller says one does (`standing`, the interned
    /// string of the bytes) and a new one otherwise, which the caller
    /// hands back to [`Request::autoload_leave`], or `None` for a name an
    /// autoloader is loading already. The `Err` is the memory limit.
    pub(crate) fn autoload_enter(
        &mut self,
        lowered: &[u8],
        standing: bool,
    ) -> Result<Option<PhpString>, Thrown> {
        let name = if standing {
            self.declared_string(lowered)?
        } else {
            self.string(lowered)?
        };
        let made = match self.autoload.in_autoload {
            Some(table) => Ok(table),
            None => self
                .array()
                .inspect(|table| self.autoload.in_autoload = Some(*table)),
        };
        let mut table = match made {
            Ok(table) => table,
            Err(thrown) => {
                self.release(Value::string(name));
                return Err(thrown);
            }
        };
        if table.find(lowered).is_some() {
            self.release(Value::string(name));
            return Ok(None);
        }
        let key = Owner::take(self, Key::Str(name));
        if let Err(thrown) = table.add_new(self, key, Owner::null()) {
            self.release(Value::string(name));
            return Err(thrown);
        }
        Ok(Some(name))
    }

    /// `zend_hash_del(EG(in_autoload), lc_name)`, and `lc_name` released.
    pub(crate) fn autoload_leave(&mut self, name: PhpString) {
        if let Some(table) = self.autoload.in_autoload {
            self.array_del(table.heap(), php_heap::Key::Str(name.heap()));
        }
        self.release(Value::string(name));
    }

    /// `zend_hash_destroy(EG(in_autoload))` and `FREE_HASHTABLE` in
    /// `shutdown_executor`, after the object storage.
    pub(crate) fn free_in_autoload(&mut self) {
        if let Some(table) = self.autoload.in_autoload.take() {
            self.release(Value::array(table));
        }
    }

    /// `spl_autoload_functions()`: each loader as its
    /// `autoload_func_info` lists it, one owner each, the caller's: a
    /// closure or an invokable object itself; a method as the pair of its receiver, or of the
    /// class the callable resolved to where it has none (`alfi->ce`), so
    /// `['self', 'load']` lists the class `self` named where it was
    /// registered, and the method's declared name; a function by its
    /// declared name.
    ///
    /// ```php
    /// class First { static function init() { spl_autoload_register(['self', 'LOAD']); } static function load($c) {} }
    /// First::init();
    /// spl_autoload_register('first::load');
    /// spl_autoload_register('Strlen');
    /// $f = fn ($c) => null; spl_autoload_register($f);
    /// class Inv { function __invoke($c) {} } spl_autoload_register(new Inv);
    /// foreach (spl_autoload_functions() as $l) echo is_array($l) ? implode('::', $l) : (is_string($l) ? $l : get_class($l)), "\n";
    /// ```
    /// ```expect
    /// Deprecated: Use of "self" in callables is deprecated in %s on line 2
    /// First::load
    /// strlen
    /// Closure
    /// Inv
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn autoload_functions(&mut self) -> Result<Vec<Owner<Value>>, Thrown> {
        let made = self.autoload_functions_made()?;
        let mut owners = Vec::with_capacity(made.len());
        for value in made {
            owners.push(Owner::adopt(value));
        }
        Ok(owners)
    }

    /// What [`autoload_functions`](Self::autoload_functions) makes, its one owner not yet named.
    pub(crate) fn autoload_functions_made(&mut self) -> Result<Vec<Value>, Thrown> {
        let loaders: Vec<(Target, Value)> = self
            .autoload
            .autoloaders
            .iter()
            .map(|loader| (loader.target(), loader.value()))
            .collect();
        let mut out = Vec::with_capacity(loaders.len());
        for (target, value) in loaders {
            match self.autoload_listed(target, &value) {
                Ok(listed) => out.push(listed),
                Err(thrown) => {
                    for listed in out {
                        self.release(listed);
                    }
                    return Err(thrown);
                }
            }
        }
        Ok(out)
    }

    /// One loader as [`Request::autoload_functions`] lists it: one
    /// registered as an object, a closure or an invokable one, is that
    /// object (`alfi->closure`).
    fn autoload_listed(&mut self, target: Target, value: &Value) -> Result<Value, Thrown> {
        if let Some(object) = value.deref().as_object().copied() {
            self.addref(&object.as_value());
            return Ok(object.as_value());
        }
        match target {
            Target::Closure(object) => {
                self.addref(&object.as_value());
                Ok(object.as_value())
            }
            Target::Function(function) => {
                let name = self.function_name(function);
                Ok(Value::string(self.string(name.as_bytes())?))
            }
            Target::Method {
                function,
                scope,
                this,
            } => {
                let first = if let Some(object) = this {
                    Owner::take(self, object.as_value())
                } else {
                    let class = self.class_name(scope);
                    let class = Value::string(self.string(class.as_bytes())?);
                    Owner::adopt(class)
                };
                let name = self.function_name(function);
                let mut pair = self.array()?;
                pair.append(self, first)?;
                let name = Value::string(self.string(name.as_bytes())?);
                pair.append(self, Owner::adopt(name))?;
                Ok(Value::array(pair))
            }
        }
    }
}
