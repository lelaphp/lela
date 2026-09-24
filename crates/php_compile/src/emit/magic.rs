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

/// The magic methods `zend_check_magic_method_implementation` knows,
/// one variant per branch of its `if` chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MagicMethod {
    Construct,
    Destruct,
    Clone,
    Get,
    Set,
    Unset,
    Isset,
    Call,
    CallStatic,
    ToString,
    DebugInfo,
    Serialize,
    Unserialize,
    SetState,
    Invoke,
    Sleep,
    Wakeup,
}

/// Whose method a magic method must be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Receiver {
    /// `zend_check_magic_method_non_static`.
    Instance,
    /// `zend_check_magic_method_static`.
    Class,
}

/// What a magic method's declared return is held to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Returns {
    /// Nothing: any return type, or none.
    Any,
    /// `zend_check_magic_method_no_return_type`: none may be declared.
    Undeclared,
    /// `zend_check_magic_method_return_type`: one declared must be
    /// within this mask.
    Within(u32),
}

/// The signature one magic method is held to: the count of parameters
/// where php fixes one, whose method it is, whether it must be public,
/// the mask each parameter declared with a type must accept, and its
/// return.
struct Signature {
    arity: Option<usize>,
    receiver: Receiver,
    public: bool,
    parameters: &'static [u32],
    returns: Returns,
}

impl MagicMethod {
    /// Each variant by its lowered name, as `ZEND_*_FUNC_NAME` spells it.
    const NAMES: [(&'static [u8], MagicMethod); 17] = [
        (b"__construct", MagicMethod::Construct),
        (b"__destruct", MagicMethod::Destruct),
        (b"__clone", MagicMethod::Clone),
        (b"__get", MagicMethod::Get),
        (b"__set", MagicMethod::Set),
        (b"__unset", MagicMethod::Unset),
        (b"__isset", MagicMethod::Isset),
        (b"__call", MagicMethod::Call),
        (b"__callstatic", MagicMethod::CallStatic),
        (b"__tostring", MagicMethod::ToString),
        (b"__debuginfo", MagicMethod::DebugInfo),
        (b"__serialize", MagicMethod::Serialize),
        (b"__unserialize", MagicMethod::Unserialize),
        (b"__set_state", MagicMethod::SetState),
        (b"__invoke", MagicMethod::Invoke),
        (b"__sleep", MagicMethod::Sleep),
        (b"__wakeup", MagicMethod::Wakeup),
    ];

    /// The magic method a lowered method name is, none for any other.
    fn named(lowered: &[u8]) -> Option<MagicMethod> {
        MagicMethod::NAMES
            .iter()
            .find(|(name, _)| *name == lowered)
            .map(|(_, magic)| *magic)
    }

    /// The branch of php's chain for this method.
    fn signature(self) -> Signature {
        use MagicMethod as M;
        use Receiver::{Class, Instance};
        let (arity, receiver, public, parameters, returns): (_, _, _, &'static [u32], _) =
            match self {
                M::Construct => (None, Instance, false, &[], Returns::Undeclared),
                M::Destruct => (Some(0), Instance, false, &[], Returns::Undeclared),
                M::Clone => (Some(0), Instance, false, &[], Returns::Within(MAY_BE_VOID)),
                M::Get => (Some(1), Instance, true, &[MAY_BE_STRING], Returns::Any),
                M::Set => (
                    Some(2),
                    Instance,
                    true,
                    &[MAY_BE_STRING],
                    Returns::Within(MAY_BE_VOID),
                ),
                M::Unset => (
                    Some(1),
                    Instance,
                    true,
                    &[MAY_BE_STRING],
                    Returns::Within(MAY_BE_VOID),
                ),
                M::Isset => (
                    Some(1),
                    Instance,
                    true,
                    &[MAY_BE_STRING],
                    Returns::Within(MAY_BE_BOOL),
                ),
                M::Call => (
                    Some(2),
                    Instance,
                    true,
                    &[MAY_BE_STRING, MAY_BE_ARRAY],
                    Returns::Any,
                ),
                M::CallStatic => (
                    Some(2),
                    Class,
                    true,
                    &[MAY_BE_STRING, MAY_BE_ARRAY],
                    Returns::Any,
                ),
                M::ToString => (Some(0), Instance, true, &[], Returns::Within(MAY_BE_STRING)),
                M::DebugInfo => (
                    Some(0),
                    Instance,
                    true,
                    &[],
                    Returns::Within(MAY_BE_ARRAY | MAY_BE_NULL),
                ),
                M::Serialize | M::Sleep => {
                    (Some(0), Instance, true, &[], Returns::Within(MAY_BE_ARRAY))
                }
                M::Unserialize => (
                    Some(1),
                    Instance,
                    true,
                    &[MAY_BE_ARRAY],
                    Returns::Within(MAY_BE_VOID),
                ),
                M::SetState => (
                    Some(1),
                    Class,
                    true,
                    &[MAY_BE_ARRAY],
                    Returns::Within(MAY_BE_OBJECT),
                ),
                M::Invoke => (None, Instance, true, &[], Returns::Any),
                M::Wakeup => (Some(0), Instance, true, &[], Returns::Within(MAY_BE_VOID)),
            };
        Signature {
            arity,
            receiver,
            public,
            parameters,
            returns,
        }
    }
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
        let Some(magic) = MagicMethod::named(&lowered) else {
            return Ok(());
        };
        let signature = magic.signature();
        let class = String::from_utf8_lossy(class).into_owned();
        let name = String::from_utf8_lossy(&method.function.name.0).into_owned();
        let parameters = &method.function.parameters;
        if let Some(count) = signature.arity {
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
        match (signature.receiver, method.is_instance) {
            (Receiver::Instance, false) => {
                return Err(
                    self.compile_error(format!("Method {class}::{name}() cannot be static"))
                );
            }
            (Receiver::Class, true) => {
                return Err(self.compile_error(format!("Method {class}::{name}() must be static")));
            }
            _ => {}
        }
        if signature.returns == Returns::Undeclared && method.function.return_type.is_some() {
            return Err(self.compile_error(format!(
                "Method {class}::{name}() cannot declare a return type"
            )));
        }
        if signature.public && method.visibility != Visibility::Public {
            self.raise(
                Level::Warning,
                &format!("The magic method {class}::{name}() must have public visibility"),
            );
        }
        for (at, (required, parameter)) in signature.parameters.iter().zip(parameters).enumerate() {
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
        if let (Returns::Within(required), Some(declared)) =
            (signature.returns, &method.function.return_type)
        {
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
