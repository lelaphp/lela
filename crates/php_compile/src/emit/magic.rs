//! The signatures php holds a magic method to,
//! `zend_check_magic_method_implementation` (`Z/Zend/zend_API.c`), which
//! `zend_compile_func_decl` runs over a method once its body has
//! compiled, at the line of the method's keyword: how many parameters
//! it takes and none by reference, whether it belongs to the class or
//! to an instance, which type a
//! parameter declared with one must accept, and which type a declared
//! return must be within. What is not public is php's warning, which
//! the method still compiles past.

use php_unit::Level;
use php_unit::decl::{MethodDecl, Type, Visibility};
use php_unit::type_to_string;

use super::Emit;
use super::types::{MAY_BE_NEVER, MAY_BE_NULL, MAY_BE_OBJECT, MAY_BE_VOID, is_complex, of_mask};
use crate::CompileError;

const MAY_BE_BOOL: u32 = 0b1100;
const MAY_BE_STRING: u32 = 1 << 6;
const MAY_BE_ARRAY: u32 = 1 << 7;
const MAY_BE_STATIC: u32 = 1 << 15;

/// What one magic method is held to, a row of php's `if` chain: the
/// count of parameters where it has one, whether it must be a class's
/// method or an instance's, whether it must be public, the type each parameter
/// must accept, and the type its return must be within.
struct Rule {
    arguments: Option<usize>,
    is_static: Option<bool>,
    public: bool,
    parameter_types: &'static [u32],
    return_type: Option<u32>,
    no_return_type: bool,
}

/// The row for a lowered method name, none for a name that is not
/// magic.
fn rule(lowered: &[u8]) -> Option<Rule> {
    let row = |arguments, is_static, public, parameter_types, return_type| Rule {
        arguments,
        is_static: Some(is_static),
        public,
        parameter_types,
        return_type,
        no_return_type: false,
    };
    Some(match lowered {
        b"__construct" => Rule {
            no_return_type: true,
            ..row(None, false, false, &[], None)
        },
        b"__destruct" => Rule {
            no_return_type: true,
            ..row(Some(0), false, false, &[], None)
        },
        b"__clone" => row(Some(0), false, false, &[], Some(MAY_BE_VOID)),
        b"__get" => row(Some(1), false, true, &[MAY_BE_STRING], None),
        b"__set" => row(Some(2), false, true, &[MAY_BE_STRING], Some(MAY_BE_VOID)),
        b"__unset" => row(Some(1), false, true, &[MAY_BE_STRING], Some(MAY_BE_VOID)),
        b"__isset" => row(Some(1), false, true, &[MAY_BE_STRING], Some(MAY_BE_BOOL)),
        b"__call" => row(Some(2), false, true, &[MAY_BE_STRING, MAY_BE_ARRAY], None),
        b"__callstatic" => row(Some(2), true, true, &[MAY_BE_STRING, MAY_BE_ARRAY], None),
        b"__tostring" => row(Some(0), false, true, &[], Some(MAY_BE_STRING)),
        b"__debuginfo" => row(Some(0), false, true, &[], Some(MAY_BE_ARRAY | MAY_BE_NULL)),
        b"__serialize" | b"__sleep" => row(Some(0), false, true, &[], Some(MAY_BE_ARRAY)),
        b"__unserialize" => row(Some(1), false, true, &[MAY_BE_ARRAY], Some(MAY_BE_VOID)),
        b"__set_state" => row(Some(1), true, true, &[MAY_BE_ARRAY], Some(MAY_BE_OBJECT)),
        b"__invoke" => row(None, false, true, &[], None),
        b"__wakeup" => row(Some(0), false, true, &[], Some(MAY_BE_VOID)),
        _ => return None,
    })
}

impl Emit<'_> {
    /// `zend_check_magic_method_implementation` over a method of the
    /// class named `class`, each of php's checks in php's order and the
    /// first one that fails php's fatal.
    ///
    /// ```php
    /// class A { function __get(int $name) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: A::__get(): Parameter #1 ($name) must be of type string when declared in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { function __debugInfo(): array|int {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: A::__debugInfo(): Return type must be ?array when declared in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A {
    ///     function __destruct($x) {}
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Method A::__destruct() cannot take arguments in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { function __set($a, &$b) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Method A::__set() cannot take arguments by reference in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { function __call($a, $b, $c) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Method A::__call() must take exactly 2 arguments in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { function __isset() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Method A::__isset() must take exactly 1 argument in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { static function __invoke() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Method A::__invoke() cannot be static in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { function __set_state(array $a) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Method A::__set_state() must be static in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { function __construct(): void {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Method A::__construct() cannot declare a return type in %s on line 2
    /// ```
    ///
    /// What php takes, it takes, a type wider than the one required and
    /// `never` among them, and a method that is not public is its
    /// warning and is served:
    ///
    /// ```php
    /// class A {
    ///     function __get(string|int $name): mixed { return $name; }
    ///     function __isset(mixed $name): bool { return true; }
    ///     function __toString(): string { return "A"; }
    ///     function __set($name, $value): never { throw new Exception; }
    ///     function __clone(): void {}
    ///     static function __set_state(array $a): static { return new static; }
    ///     private function __call($name, $arguments) { return $name; }
    /// }
    /// $a = new A;
    /// echo $a->x, " ", isset($a->y) ? "set" : "unset", " ", $a, "\n";
    /// ```
    /// ```expect exit=0
    /// Warning: The magic method A::__call() must have public visibility in %s on line 9
    /// x set A
    /// ```
    pub(super) fn magic_method(
        &mut self,
        class: &[u8],
        method: &MethodDecl,
    ) -> Result<(), CompileError> {
        let lowered = method.function.name.0.to_ascii_lowercase();
        if !lowered.starts_with(b"__") {
            return Ok(());
        }
        let Some(rule) = rule(&lowered) else {
            return Ok(());
        };
        let class = String::from_utf8_lossy(class).into_owned();
        let name = String::from_utf8_lossy(&method.function.name.0).into_owned();
        let parameters = &method.function.parameters;
        if let Some(count) = rule.arguments {
            if parameters.len() != count {
                return Err(self.compile_error(match count {
                    0 => format!("Method {class}::{name}() cannot take arguments"),
                    1 => format!("Method {class}::{name}() must take exactly 1 argument"),
                    n => format!("Method {class}::{name}() must take exactly {n} arguments"),
                }));
            }
            if parameters.iter().any(|parameter| parameter.by_ref) {
                return Err(self.compile_error(format!(
                    "Method {class}::{name}() cannot take arguments by reference"
                )));
            }
        }
        match rule.is_static {
            Some(false) if !method.is_instance => {
                return Err(
                    self.compile_error(format!("Method {class}::{name}() cannot be static"))
                );
            }
            Some(true) if method.is_instance => {
                return Err(self.compile_error(format!("Method {class}::{name}() must be static")));
            }
            _ => {}
        }
        if rule.no_return_type && method.function.return_type.is_some() {
            return Err(self.compile_error(format!(
                "Method {class}::{name}() cannot declare a return type"
            )));
        }
        if rule.public && method.visibility != Visibility::Public {
            self.raise(
                Level::Warning,
                &format!("The magic method {class}::{name}() must have public visibility"),
            );
        }
        for (at, required) in rule.parameter_types.iter().enumerate() {
            let Some(parameter) = parameters.get(at) else {
                continue;
            };
            if let Some(ty) = &parameter.ty
                && ty.mask() & required == 0
            {
                return Err(self.compile_error(format!(
                    "{class}::{name}(): Parameter #{} (${}) must be of type {} when declared",
                    at + 1,
                    String::from_utf8_lossy(&parameter.name.0),
                    type_to_string(&of_mask(*required))
                )));
            }
        }
        if let (Some(required), Some(declared)) = (rule.return_type, &method.function.return_type) {
            self.magic_return(&class, &name, required, declared)?;
        }
        Ok(())
    }

    /// `zend_check_magic_method_return_type`: a declared return that is
    /// `never`, or within the type required, where only `object` takes
    /// a class and `static` counts as one.
    fn magic_return(
        &self,
        class: &str,
        name: &str,
        required: u32,
        declared: &Type,
    ) -> Result<(), CompileError> {
        let mask = declared.mask();
        if mask & MAY_BE_NEVER != 0 {
            return Ok(());
        }
        let mut complex = is_complex(declared);
        let mut extra = mask & !required;
        if extra & MAY_BE_STATIC != 0 {
            extra &= !MAY_BE_STATIC;
            complex = true;
        }
        if extra != 0 || (complex && required != MAY_BE_OBJECT) {
            return Err(self.compile_error(format!(
                "{class}::{name}(): Return type must be {} when declared",
                type_to_string(&of_mask(required))
            )));
        }
        Ok(())
    }
}
