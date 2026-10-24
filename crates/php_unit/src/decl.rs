//! The declaration model (design §4.7): the unlinked description of a
//! class, function, constant or attribute as its producer wrote it, with
//! constant expressions still unevaluated. Produced by the compiler for
//! user code and by the native macros for natives; consumed by the linker,
//! Reflection, the dumpers and the stub cross-check, which read one model.
//! The types are re-exported by `php_engine::api::decl`, so their surface
//! is `api.lock`'s as well as `unit.lock`'s; the trait adaptations, which
//! no native writes, are not re-exported.

/// A name as declared: resolved, as `zend_resolve_class_name` leaves a
/// written one, so fully qualified with no leading `\`, and in the case
/// it was declared in. The compiler resolves a name the source wrote
/// before it makes one of it; a native's is spelled in its crate. What a
/// table by name files the declaration under ([`Bare`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Name(pub Box<[u8]>);

/// A name to look up a class or a function by: past the one leading
/// `\` that marks a written name fully qualified, which
/// `zend_lookup_class_ex` and `zend_init_dynamic_call_string` drop, and
/// in any case, since a table by name folds it. Made once where a name
/// as written enters a lookup ([`Bare::of`]), or from a name that never
/// had the `\` ([`Bare::resolved`], a [`Name`]); never made again from
/// one, so no lookup drops a second `\`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bare<'a>(&'a [u8]);

impl<'a> Bare<'a> {
    /// `written` past its leading `\`, where it has one.
    #[must_use]
    pub fn of(written: &'a [u8]) -> Bare<'a> {
        Bare(written.strip_prefix(b"\\").unwrap_or(written))
    }

    /// `name`, which the compiler resolved or the engine spells in its
    /// own source, as it stands. A name as written handed here is a
    /// strip missed, which a debug build refuses (#2711).
    #[must_use]
    pub fn resolved(name: &'a [u8]) -> Bare<'a> {
        debug_assert!(
            !name.starts_with(b"\\"),
            "a name as written handed to Bare::resolved"
        );
        Bare(name)
    }

    /// The name's bytes.
    #[must_use]
    pub const fn bytes(self) -> &'a [u8] {
        self.0
    }
}

impl<'a> From<&'a Name> for Bare<'a> {
    fn from(name: &'a Name) -> Bare<'a> {
        Bare(&name.0)
    }
}

impl From<&str> for Name {
    fn from(s: &str) -> Name {
        Name(s.as_bytes().into())
    }
}

/// A name as php's `%s` prints `ZSTR_VAL(name)` into a message or a
/// dump: up to its first NUL. Only an anonymous class's name holds one,
/// `class@anonymous` before it and the declaring file and line after,
/// so the class is shown as `class@anonymous` wherever php formats it
/// with `%s`, and every other name is shown whole.
#[must_use]
pub fn shown(name: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(shown_bytes(name))
}

/// [`shown`]'s bytes as they stand, for a dump that prints a name as
/// php prints it, byte for byte, whatever encoding it is in.
///
/// ```php
/// eval("class \xdcb {}");
/// ob_start(); var_dump(new ("\xdcb")); echo bin2hex(ob_get_clean()), "\n";
/// var_dump(new class {});
/// ```
/// ```expect
/// 6f626a65637428dc6229233120283029207b0a7d0a
/// object(class@anonymous)#1 (0) {
/// }
/// ```
#[must_use]
pub fn shown_bytes(name: &[u8]) -> &[u8] {
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    &name[..end]
}

/// `validate_attribute`, `validate_allow_dynamic_properties` and
/// `validate_deprecated` (`Z/Zend/zend_attributes.c`), the validators of
/// the internal attributes that take a class: php's refusal of the
/// attribute `attribute` names, by its resolved name in any case, over
/// a class of this kind and these flags named `class`; none where the
/// validator takes the class or the attribute has none. What the
/// compiler raises where the class is declared, and what
/// `ReflectionAttribute::newInstance` throws for an attribute
/// `#[\DelayedTargetValidation]` deferred, php's
/// `zend_attribute.validation_error`, so the rule stands in one place
/// for both.
#[must_use]
pub fn class_attribute_refusal(
    attribute: &[u8],
    kind: ClassKind,
    flags: &ClassFlags,
    class: &str,
) -> Option<String> {
    let named = |name: &str| attribute.eq_ignore_ascii_case(name.as_bytes());
    let (declared, word) = if named("Attribute") {
        let word = match kind {
            ClassKind::Trait => "trait",
            ClassKind::Interface => "interface",
            ClassKind::Enum => "enum",
            ClassKind::Class if flags.is_abstract => "abstract class",
            ClassKind::Class => return None,
        };
        ("Attribute", word)
    } else if named("AllowDynamicProperties") {
        let word = match kind {
            ClassKind::Trait => "trait",
            ClassKind::Interface => "interface",
            ClassKind::Class if flags.is_readonly => "readonly class",
            ClassKind::Enum => "enum",
            ClassKind::Class => return None,
        };
        ("AllowDynamicProperties", word)
    } else if named("Deprecated") {
        let word = match kind {
            ClassKind::Trait => return None,
            ClassKind::Interface => "interface",
            ClassKind::Enum => "enum",
            ClassKind::Class => "class",
        };
        ("Deprecated", word)
    } else {
        return None;
    };
    Some(format!("Cannot apply #[\\{declared}] to {word} {class}"))
}

/// `validate_nodiscard` (`Z/Zend/zend_attributes.c:231`), the one
/// validator of an internal attribute that reads whether its subject is
/// a property hook (`CG(context).active_property_info_name`): php's
/// refusal of the attribute `attribute` names, by its resolved name in
/// any case, on a hook, none for every other attribute. What the
/// compiler raises where the hook is declared, and what
/// `ReflectionAttribute::newInstance` throws for an attribute
/// `#[\DelayedTargetValidation]` deferred, php's
/// `zend_attribute.validation_error`; the hook carries no
/// `ZEND_ACC_NODISCARD` either way.
#[must_use]
pub fn hook_attribute_refusal(attribute: &[u8]) -> Option<&'static str> {
    attribute
        .eq_ignore_ascii_case(b"NoDiscard")
        .then_some("#[\\NoDiscard] is not supported for property hooks")
}

/// php's three member visibilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Visibility {
    #[default]
    Public,
    Protected,
    Private,
}

/// What a class declaration declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ClassKind {
    #[default]
    Class,
    Interface,
    Trait,
    Enum,
}

/// The modifiers of a class (`ZEND_ACC_*` on the class entry).
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits of a class, each one fact
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ClassFlags {
    pub is_abstract: bool,
    pub is_final: bool,
    pub is_readonly: bool,
    /// `ZEND_ACC_ANON_CLASS`: declared by `new class`, under the name
    /// `zend_generate_anon_class_name` made, which holds a NUL.
    pub is_anonymous: bool,
    /// `ZEND_ACC_TOP_LEVEL`: declared by a statement of the file's own
    /// list, or a bracketed namespace's, and not in a block or a body,
    /// which is what `preload_link` links of a class the preload left
    /// unbound.
    pub is_top_level: bool,
    /// `ZEND_ACC_NOT_SERIALIZABLE`: the stub's `@not-serializable`, and
    /// what `zend_compile_class_decl` sets on an anonymous class.
    /// `serialize` and `unserialize` refuse an object of the class and
    /// of every class below it, since `zend_do_inheritance_ex` passes
    /// the bit down.
    ///
    /// ```php
    /// class R extends ReflectionClass {}
    /// foreach ([new ReflectionClass('stdClass'), new R('stdClass'), new class {}] as $o) {
    ///     try { serialize($o); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// }
    /// foreach (['O:15:"ReflectionClass":0:{}', 'O:1:"R":0:{}', 'C:15:"ReflectionClass":0:{}'] as $s) {
    ///     try { unserialize($s); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Exception: Serialization of 'ReflectionClass' is not allowed
    /// Exception: Serialization of 'R' is not allowed
    /// Exception: Serialization of 'class@anonymous' is not allowed
    /// Exception: Unserialization of 'ReflectionClass' is not allowed
    /// Exception: Unserialization of 'R' is not allowed
    /// Exception: Unserialization of 'ReflectionClass' is not allowed
    /// ```
    pub not_serializable: bool,
    pub dynamic_properties: DynamicProperties,
    /// `ZEND_ACC_USE_GUARDS` as an interface hands it on: every class
    /// that implements the interface keeps its objects' recursion guards
    /// one bit per walk (`ZEND_GUARD_RECURSION_DEBUG`, `_EXPORT`,
    /// `_JSON`) rather than in the one `GC_PROTECTED` bit, which is what
    /// ext/json's `interface_gets_implemented` hook does for
    /// `JsonSerializable`. A class's own `__get`, `__set`, `__isset`,
    /// `__unset` or `__debugInfo` does the same without this, as
    /// `zend_add_magic_method` sets the bit; the engine asks those by
    /// name.
    pub use_guards: bool,
    /// `ce->num_hooked_prop_variance_checks` not zero: a `set` hook's
    /// parameter type the file could not hold to its property's while
    /// it compiled, which only the link holds it to. php links such a
    /// class at its opline and never while the file compiles
    /// (`Z/Zend/zend_compile.c:9362`), an anonymous one included.
    pub hooked_variance: bool,
}

/// What a class does with a write to a name it did not declare, php's
/// two `ZEND_ACC_*` bits read as the one question they answer
/// (`Z/Zend/zend_object_handlers.c:1288`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DynamicProperties {
    /// Neither bit, which every class carries unless it says
    /// otherwise: creating one is php's 8.2 deprecation.
    #[default]
    Deprecated,
    /// `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES`, what
    /// `#[\AllowDynamicProperties]` sets and what `stdClass` is
    /// registered with: creating one raises nothing.
    Allowed,
    /// `ZEND_ACC_NO_DYNAMIC_PROPERTIES`, which an enum carries and
    /// `zend_register_closure_ce` sets on `Closure`: creating one is
    /// php's `Error`.
    Forbidden,
}

/// The backing type of an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EnumBacking {
    #[default]
    Pure,
    Int,
    String,
}

/// A declared type: scalar names, class names, `?T`, unions,
/// intersections and DNF, as `zend_type` holds them. The tentative bit of
/// a native method's return lives on the method (design §4.7).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Mixed,
    Void,
    Never,
    Null,
    Bool,
    False,
    True,
    Int,
    Float,
    String,
    Array,
    Object,
    Iterable,
    Callable,
    /// The `static` type: the called class.
    CalledClass,
    /// `self` and `parent`, resolved at link time.
    SelfClass,
    ParentClass,
    Named(Name),
    Nullable(Box<Type>),
    Union(Vec<Type>),
    Intersection(Vec<Type>),
}

impl Type {
    /// `MAY_BE_ANY` (`Z/Zend/zend_type_info.h:35`): the bit of every
    /// `IS_*` a value can be, which is the mask a parameter with no
    /// declared type carries in a `RECV`'s `op2`.
    pub const MAY_BE_ANY: u32 = 0x3fe;

    /// `ZEND_TYPE_PURE_MASK` (`Z/Zend/zend_types.h:284`): the `1 <<
    /// IS_*` of every builtin this type accepts, which a `RECV`'s `op2`
    /// carries so the handler's fast path is one test against the
    /// argument's type. A class name contributes no bit, so an object
    /// argument takes the slow path and is checked against the name;
    /// `mixed` is every bit and `iterable` is `array` beside the bit of
    /// `Traversable`'s name.
    #[must_use]
    pub fn mask(&self) -> u32 {
        match self {
            Type::Mixed => Type::MAY_BE_ANY,
            Type::Void => 1 << 14,
            Type::Never => 1 << 17,
            Type::Null => 1 << 1,
            Type::Bool => (1 << 2) | (1 << 3),
            Type::False => 1 << 2,
            Type::True => 1 << 3,
            Type::Int => 1 << 4,
            Type::Float => 1 << 5,
            Type::String => 1 << 6,
            // `iterable` is `array` beside `Traversable`, whose bit is
            // the name's and not the mask's.
            Type::Array | Type::Iterable => 1 << 7,
            Type::Object => 1 << 8,
            Type::Callable => 1 << 12,
            Type::CalledClass => 1 << 15,
            // A class name, and an intersection, which is names alone,
            // contribute no bit: the check reads the name.
            Type::SelfClass | Type::ParentClass | Type::Named(_) | Type::Intersection(_) => 0,
            Type::Nullable(inner) => inner.mask() | (1 << 1),
            Type::Union(members) => members.iter().fold(0, |mask, ty| mask | ty.mask()),
        }
    }

    /// Whether a value of this `IS_*` code satisfies the type without a
    /// coercion: `ZEND_TYPE_CONTAINS_CODE` over the pure mask, which
    /// `zend_check_type` tries before anything else.
    #[must_use]
    pub fn contains_code(&self, code: u8) -> bool {
        self.mask() & (1u32 << u32::from(code)) != 0
    }
}

/// A name's mark in php's tree, `ZEND_NAME_NOT_FQ`, `ZEND_NAME_FQ` or
/// `ZEND_NAME_RELATIVE`: written as it stands, after a `\`, or after
/// `namespace\`. A class constant's class keeps the mark
/// `zend_compile_const_expr_class_const` leaves on it, `ZEND_NAME_FQ`
/// wherever resolution rewrote the name, through an import or the
/// namespace, and `zend_ast_export_ns_name` prints it back as `\` or
/// `namespace\` before the name.
///
/// ```php
/// namespace App { use Other\Baz; function f($a = Bar::BAZ, $b = \Q::X, $c = Baz::Y, $d = namespace\R::Z) {} }
/// namespace { function g($a = N\Bar::BAZ, $b = namespace\R::Z) {} }
/// namespace {
///     foreach (['App\f', 'g'] as $f) {
///         foreach ((new ReflectionFunction($f))->getParameters() as $p) { echo $p, "\n"; }
///     }
/// }
/// ```
/// ```expect
/// Parameter #0 [ <optional> $a = \App\Bar::BAZ ]
/// Parameter #1 [ <optional> $b = \Q::X ]
/// Parameter #2 [ <optional> $c = \Other\Baz::Y ]
/// Parameter #3 [ <optional> $d = \App\R::Z ]
/// Parameter #0 [ <optional> $a = N\Bar::BAZ ]
/// Parameter #1 [ <optional> $b = namespace\R::Z ]
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameKind {
    NotFullyQualified,
    FullyQualified,
    Relative,
}

/// The name a constant expression fetches a constant by, php's
/// `ZEND_AST_CONSTANT` and its `attr`: the name as
/// `zend_compile_const_expr_const` resolved it against the constant
/// imports and the namespace, which Reflection prints and an undefined
/// constant's `Error` names, and whether a miss falls back to the
/// global constant. A bare name no `use const` names, written inside a
/// namespace, is php's `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE`: the
/// namespace's constant where it is defined, else the global one of the
/// same name (`zend_get_constant_ex`).
///
/// ```php
/// namespace N;
/// const FOO = "ns";
/// define("BAR", "global");
/// function f($a = FOO, $b = BAR, $c = \N\FOO, $d = [FOO, BAR]) { return [$a, $b, $c, $d]; }
/// foreach ((new \ReflectionFunction('N\f'))->getParameters() as $p) {
///     echo $p, " ", $p->isDefaultValueConstant() ? $p->getDefaultValueConstantName() : "-", "\n";
/// }
/// echo json_encode(f()), "\n";
/// class A { public $foo = FOO; }
/// class B { const Q = NOPE; }
/// var_dump((new A)->foo);
/// try { B::Q; } catch (\Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Parameter #0 [ <optional> $a = N\FOO ] N\FOO
/// Parameter #1 [ <optional> $b = N\BAR ] N\BAR
/// Parameter #2 [ <optional> $c = N\FOO ] N\FOO
/// Parameter #3 [ <optional> $d = [N\FOO, N\BAR] ] -
/// ["ns","global","ns",["ns","global"]]
/// string(2) "ns"
/// Undefined constant "N\NOPE"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConstantName {
    /// A name fetched as resolved: written with a `\` in it, named by a
    /// `use const`, or written outside any namespace.
    Resolved(Name),
    /// A bare name written inside a namespace, resolved against it
    /// (`N\FOO`), whose global constant (`FOO`) stands in where the
    /// namespace defines none.
    UnqualifiedInNamespace(Name),
}

impl ConstantName {
    /// The name resolved at compile time, `zend_ast_get_constant_name`.
    #[must_use]
    pub const fn resolved(&self) -> &Name {
        match self {
            ConstantName::Resolved(name) | ConstantName::UnqualifiedInNamespace(name) => name,
        }
    }

    /// The global name fetched where the resolved one names no
    /// constant: the last segment of a bare name written inside a
    /// namespace, and none for any other.
    #[must_use]
    pub fn fallback(&self) -> Option<&[u8]> {
        match self {
            ConstantName::Resolved(_) => None,
            ConstantName::UnqualifiedInNamespace(name) => {
                let at = name.0.iter().rposition(|byte| *byte == b'\\')?;
                name.0.get(at.saturating_add(1)..)
            }
        }
    }
}

/// An unevaluated constant expression: Zend's `zend_ast` for constant
/// expressions, evaluated by the engine on first use and kept as written
/// for Reflection. A closure is an index into the owning unit's function
/// table and a first-class callable the name it resolves at evaluation,
/// as php's `zend_ast_fcc` resolves it.
///
/// `line` is `zend_ast_get_lineno` of a node whose evaluation can raise,
/// the line of its first child as `zend_ast_create` gives it one: where
/// an expression with a class for its scope is evaluated,
/// `zend_ast_evaluate_ex` stands `EG(lineno_override)` at each node's
/// line and `EG(filename_override)` at the scope's file, so what the
/// node raises names that file and line and a backtrace taken there
/// carries php's `[constant expression]` row (`Z/Zend/zend_ast.c:569`).
/// A value written out raises nothing and carries none; a native's expression,
/// which no file holds, carries zero.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstExpr {
    Null,
    Bool(bool),
    /// An integer and the radix it is written in, which Reflection
    /// renders it in. The compiler folds a program's integers to
    /// `Decimal`, as php's `zend_ast` holds a folded `zval` and
    /// Reflection prints `511` for a user function's `$x = 0777`.
    Long {
        value: i64,
        radix: Radix,
    },
    Double(f64),
    Str(Box<[u8]>),
    /// A constant by its name, php's `ZEND_AST_CONSTANT`.
    Constant {
        name: ConstantName,
        line: u32,
    },
    /// `class::name`, php's `ZEND_AST_CLASS_CONST`: the class resolved,
    /// `self` and `parent` as written, and the mark compilation left on
    /// the class's name, which Reflection prints it back with.
    ClassConstant {
        class: Name,
        class_kind: NameKind,
        name: Name,
        line: u32,
    },
    /// `class::{expr}`, php's `ZEND_AST_CLASS_CONST` whose name the
    /// compiler could not fold to a string: the name evaluated first,
    /// then the constant it names fetched as [`ConstExpr::ClassConstant`]
    /// fetches one, php's `TypeError` where the name is no string.
    DynamicClassConstant {
        class: Name,
        class_kind: NameKind,
        name: Box<ConstExpr>,
        line: u32,
    },
    EnumCase {
        class: Name,
        case: Name,
        line: u32,
    },
    /// `class::class`, php's `ZEND_AST_CLASS_NAME`: the name of the class
    /// as written, `self` and `parent` the scope the expression is
    /// evaluated in and its parent's. A program's names fold to the
    /// string where the scope is known, so what stays is `self` or
    /// `parent` in a trait, a closure or code outside any function, and a
    /// native's default written `SplFileObject::class`.
    ClassName {
        class: Name,
        line: u32,
    },
    Unary {
        op: Name,
        operand: Box<ConstExpr>,
        line: u32,
    },
    /// A binary operator by its spelling, `&&`, `||` and `??` among them
    /// (php's `ZEND_AST_AND`, `ZEND_AST_OR` and `ZEND_AST_COALESCE`),
    /// whose right side is evaluated only where the left leaves it to.
    Binary {
        op: Name,
        lhs: Box<ConstExpr>,
        rhs: Box<ConstExpr>,
        line: u32,
    },
    /// `[key => value, ...]`, php's `ZEND_AST_ARRAY`, whose line is its
    /// first element's (`zend_ast_create_list`): an array some key or
    /// value of which is still to evaluate, or a native's, which no
    /// unit holds.
    Array {
        elements: Vec<(Option<ConstExpr>, ConstExpr)>,
        line: u32,
    },
    /// A value of a program's declaration the compiler folded and the
    /// unit holds: an array whose every key and value folded
    /// (`zend_try_ct_eval_array`), or the string a request's compile made
    /// for a class constant, a default or a parameter's default, which
    /// php's tables hold as a value and not as a tree, and which raises
    /// nothing where it is read.
    UnitValue(UnitValue),
    /// `...value` inside an array, php's `ZEND_AST_UNPACK`: the value
    /// of an element with no key, whose array or `Traversable` is
    /// spread into the array being built, under the array's line.
    Unpack(Box<ConstExpr>),
    /// `container[dim]`, php's `ZEND_AST_DIM`.
    Dim {
        container: Box<ConstExpr>,
        dim: Box<ConstExpr>,
        line: u32,
    },
    /// `object->name` or, `nullsafe`, `object?->name`, php's
    /// `ZEND_AST_PROP` and `ZEND_AST_NULLSAFE_PROP`, the name itself an
    /// expression.
    Prop {
        object: Box<ConstExpr>,
        name: Box<ConstExpr>,
        nullsafe: bool,
        line: u32,
    },
    /// `cond ? then : otherwise`, and `cond ?: otherwise` with no
    /// `then`, php's `ZEND_AST_CONDITIONAL`, which raises nothing of its
    /// own.
    Conditional {
        cond: Box<ConstExpr>,
        then: Option<Box<ConstExpr>>,
        otherwise: Box<ConstExpr>,
    },
    /// `(type) operand`, php's `ZEND_AST_CAST`, `to` the `IS_*` code
    /// the `CAST` opline carries in `extended_value`.
    Cast {
        to: u32,
        operand: Box<ConstExpr>,
        line: u32,
    },
    /// `new class(arguments)`, php's `ZEND_AST_NEW`: the class as
    /// written, `self` and `parent` resolved in the scope the expression
    /// is evaluated in, and each argument with its name where it was
    /// written `name: value`.
    New {
        class: Name,
        arguments: Vec<(Option<Name>, ConstExpr)>,
        line: u32,
    },
    /// `static function (...) { ... }` (8.5), php's `ZEND_AST_OP_ARRAY`:
    /// the body is function `at` of the unit's [`Unit::dynamic`], which
    /// no op array declares (`FUNC_DECL_LEVEL_CONSTEXPR`), reached
    /// through `unit` once the engine has published the unit, and made
    /// a closure over the evaluating scope each time the expression is
    /// evaluated (`zend_create_closure`); `name` is the
    /// `{closure:...}` php prints the node as.
    ///
    /// [`Unit::dynamic`]: crate::Unit::dynamic
    Closure {
        at: u32,
        name: Name,
        unit: crate::unit::UnitCell,
    },
    /// `name(...)` (8.5), php's `ZEND_AST_CALL` over
    /// `ZEND_AST_CALLABLE_CONVERT`: the function's name as
    /// `zend_resolve_function_name` left it and whether it was written
    /// fully qualified, which is what keeps the global fallback from an
    /// unqualified name in a namespace; the fake closure is made of the
    /// function found at evaluation. The function a request finds first
    /// is the one it keeps for the node, php's `zend_ast_fcc.fptr` map
    /// pointer, so the node is named by its unit and its number `at`
    /// among the unit's callables.
    FunctionCallable {
        name: Name,
        name_kind: NameKind,
        line: u32,
        at: u32,
        unit: crate::unit::UnitCell,
    },
    /// `Class::method(...)` (8.5), php's `ZEND_AST_STATIC_CALL` over
    /// `ZEND_AST_CALLABLE_CONVERT`: the class resolved, `self` and
    /// `parent` as written, and the method as written; the fake closure
    /// is made of the static method found at evaluation, with the class
    /// as its called scope.
    MethodCallable {
        class: Name,
        method: Name,
        line: u32,
    },
    /// `__CLASS__` in a trait's constant expression, php's
    /// `ZEND_AST_CONSTANT_CLASS`: the name of the scope the expression
    /// is evaluated in, which is the class that took the member from the
    /// trait, and the empty string where there is none.
    ScopeClass,
}

impl ConstExpr {
    /// An integer written in decimal, as a program's integers are
    /// folded and as most of the stubs write them.
    #[must_use]
    pub const fn long(value: i64) -> ConstExpr {
        ConstExpr::Long {
            value,
            radix: Radix::Decimal,
        }
    }

    /// The node's own line, `zend_ast_get_lineno`, where its evaluation
    /// can raise; none on a node that raises nothing of its own.
    #[must_use]
    pub const fn line(&self) -> Option<u32> {
        match self {
            ConstExpr::Constant { line, .. }
            | ConstExpr::ClassConstant { line, .. }
            | ConstExpr::DynamicClassConstant { line, .. }
            | ConstExpr::FunctionCallable { line, .. }
            | ConstExpr::MethodCallable { line, .. }
            | ConstExpr::EnumCase { line, .. }
            | ConstExpr::ClassName { line, .. }
            | ConstExpr::Unary { line, .. }
            | ConstExpr::Binary { line, .. }
            | ConstExpr::Array { line, .. }
            | ConstExpr::Dim { line, .. }
            | ConstExpr::Prop { line, .. }
            | ConstExpr::Cast { line, .. }
            | ConstExpr::New { line, .. } => Some(*line),
            ConstExpr::Null
            | ConstExpr::Bool(_)
            | ConstExpr::Long { .. }
            | ConstExpr::Double(_)
            | ConstExpr::Str(_)
            | ConstExpr::UnitValue(_)
            | ConstExpr::Unpack(_)
            | ConstExpr::Conditional { .. }
            | ConstExpr::Closure { .. }
            | ConstExpr::ScopeClass => None,
        }
    }
}

/// A value a declaration of the unit holds, laid down where the unit's
/// strings are, by identity: an array every key and value of which
/// folded (`zend_try_ct_eval_array`, [`php_heap::Intern::constant_array`]),
/// as an op array's constant table holds one (design §5.3), immutable in
/// a unit the cache keeps (`IS_ARRAY_IMMUTABLE`, `zend_persist_zval`) and
/// counted in a unit compiled for one request, which owns it through the
/// request's unit strings, as php leaves an `eval`'s; or a counted string
/// of such a unit, the zval php's `constants_table`,
/// `default_properties_table` or `RECV_INIT` constant holds, where an
/// interned one is [`ConstExpr::Str`], named by its bytes. A view: a copy
/// takes an owner (`ZVAL_COPY`), which an immutable array does not count.
#[derive(Debug, Clone, Copy)]
pub struct UnitValue(php_heap::Zval);

impl UnitValue {
    /// `value` where it is an array or a counted string, none for any
    /// other value.
    #[must_use]
    pub fn new(value: php_heap::Zval) -> Option<UnitValue> {
        let held = value.as_arr().is_some() || (value.as_str().is_some() && value.is_refcounted());
        held.then_some(UnitValue(value))
    }

    /// The value as the zval that names it, no owner taken.
    #[must_use]
    pub const fn value(self) -> php_heap::Zval {
        self.0
    }

    /// Whether it is an array that holds no element.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0.as_arr().is_some_and(|array| array.is_empty())
    }
}

/// Two values are the same declaration where `zend_is_identical` finds
/// them so ([`php_heap::identical_constants`]), whichever unit laid
/// either down: what a trait's default is compared to its user's by.
impl PartialEq for UnitValue {
    fn eq(&self, other: &UnitValue) -> bool {
        php_heap::identical_constants(self.0, other.0)
    }
}

/// The radix an integer of a native's declaration is written in, which
/// `gen_stub.php` keeps by copying the stub's token into
/// `zend_internal_arg_info.default_value`, and which Reflection prints
/// back: `mkdir`'s `$permissions = 0777` is the octal `511`, rendered
/// `0777`. The stubs write two octal spellings in a parameter's
/// default, php's legacy `0777` and 8.1's explicit `0o644`; hexadecimal
/// and binary stand only in constants' values, which are declared by
/// value. Exhaustive, so a radix added later is a change every renderer
/// answers before it builds.
///
/// ```php
/// echo (new ReflectionFunction('mkdir'))->getParameters()[1], "\n";
/// function f($x = 0777, $y = 0o17) {}
/// echo implode(", ", (new ReflectionFunction('f'))->getParameters()), "\n";
/// ```
/// ```expect
/// Parameter #1 [ <optional> int $permissions = 0777 ]
/// Parameter #0 [ <optional> $x = 511 ], Parameter #1 [ <optional> $y = 15 ]
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Radix {
    Decimal,
    /// `0777`: a leading zero.
    Octal,
    /// `0o644`: the `0o` prefix of php 8.1.
    OctalExplicit,
}

impl Radix {
    /// `value` as this radix writes it, the text `gen_stub.php` copies
    /// from the stub.
    #[must_use]
    pub fn written(self, value: i64) -> String {
        let sign = if value < 0 { "-" } else { "" };
        let magnitude = value.unsigned_abs();
        match self {
            Radix::Decimal => value.to_string(),
            Radix::Octal => format!("{sign}0{magnitude:o}"),
            Radix::OctalExplicit => format!("{sign}0o{magnitude:o}"),
        }
    }
}

/// What an attribute stands on, one `ZEND_ATTRIBUTE_TARGET_*` bit
/// (`Z/Zend/zend_attributes.h`), each variant the bit itself: what
/// `zend_compile_attributes` hands an internal attribute's validator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttributeTarget {
    Class = 1 << 0,
    Function = 1 << 1,
    Method = 1 << 2,
    Property = 1 << 3,
    ClassConstant = 1 << 4,
    Parameter = 1 << 5,
    Constant = 1 << 6,
}

impl TryFrom<u32> for AttributeTarget {
    type Error = u32;

    /// The target one `ZEND_ATTRIBUTE_TARGET_*` bit names, the variant
    /// that is that bit; any other mask is handed back.
    fn try_from(bit: u32) -> Result<AttributeTarget, u32> {
        [
            AttributeTarget::Class,
            AttributeTarget::Function,
            AttributeTarget::Method,
            AttributeTarget::Property,
            AttributeTarget::ClassConstant,
            AttributeTarget::Parameter,
            AttributeTarget::Constant,
        ]
        .into_iter()
        .find(|target| *target as u32 == bit)
        .ok_or(bit)
    }
}

/// `zend_internal_attribute.validator`: what an extension's attribute
/// class runs over each place a program applies it, once the target is
/// one its `#[Attribute]` allows, handed the attribute as written, the
/// target and the name of the class it is applied within, php's `scope`
/// (`CG(active_class_entry)`: the class itself for a class's attribute,
/// none outside a class); the sentence of the compile error that refuses
/// it, none where it takes the target. The compiler raises the sentence where the
/// attribute is applied, and `ReflectionAttribute::newInstance` throws
/// it where `#[\DelayedTargetValidation]` deferred it, php's
/// `zend_attribute.validation_error`.
#[derive(Debug, Clone, Copy)]
pub struct AttributeValidator(pub fn(&Attribute, AttributeTarget, Option<&[u8]>) -> Option<String>);

/// One validator where it is one function, by address
/// (`std::ptr::fn_addr_eq`), which is all two declarations compared
/// for equality can ask of it.
impl PartialEq for AttributeValidator {
    fn eq(&self, other: &AttributeValidator) -> bool {
        std::ptr::fn_addr_eq(self.0, other.0)
    }
}

/// One argument of an attribute, positional or named.
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeArgument {
    pub name: Option<Name>,
    pub value: ConstExpr,
}

/// An attribute as written, its arguments unevaluated, with php's
/// `zend_attribute.lineno`, the line its name stands on, and
/// `ZEND_ATTRIBUTE_STRICT_TYPES`, whether the file it was written in
/// declares `strict_types`: the site and the regime
/// `zend_get_attribute_object` makes its object under; and `own_name`,
/// whether the string `attr->name` is is one no holder the compile
/// filled before it holds: one `zend_resolve_class_name` made, or the
/// first copy of the scanner's or a `use`'s, which a later holder copies
/// again, a promoted parameter's property the parameter's and another
/// attribute the `use`'s. A native's has line zero and no regime.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: Name,
    pub arguments: Vec<AttributeArgument>,
    pub line: u32,
    pub strict: bool,
    pub own_name: bool,
}

/// What stands after a parameter's name, decoded once where the
/// declaration is written and matched wherever a default is read.
///
/// A user function's is the `RECV_INIT` constant, kept as written
/// (design §4.7), and `None` where the opline is a plain `RECV`, which
/// is what `zend_compile_params` makes of a default written before the
/// last required parameter. A native's is the expression behind
/// `gen_stub.php`'s `zend_internal_arg_info.default_value`, whose text
/// is what the stub generator's printer makes of that expression, so
/// the text is rendered from it and not carried beside it; and
/// `Unshown` is the stub's `= UNKNOWN`, a parameter past
/// `required_num_args` with no `default_value`, which
/// `_parameter_string` prints as `<default>` and
/// `ReflectionParameter::isDefaultValueAvailable` answers `false` for.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ParameterDefault {
    #[default]
    None,
    Unshown,
    Expr(ConstExpr),
}

impl ParameterDefault {
    /// The expression php's `get_parameter_default` hands back: none on
    /// a required parameter and none on `Unshown`.
    pub fn expr(&self) -> Option<&ConstExpr> {
        match self {
            ParameterDefault::None | ParameterDefault::Unshown => None,
            ParameterDefault::Expr(expr) => Some(expr),
        }
    }
}

/// One declared parameter slot.
///
/// `prefer_ref` is php's `ZEND_SEND_PREFER_REF` beside `by_ref`'s
/// `ZEND_SEND_BY_REF` (`Z/Zend/zend_compile.h:1117`): a parameter that
/// takes a variable by reference and anything else by value, which the
/// stub marks `@prefer-ref` and only `extract()` and
/// `array_multisort()` declare. It is set only where `by_ref` is, so
/// `by_ref` alone is `ARG_SHOULD_BE_SENT_BY_REF` and `by_ref` without
/// `prefer_ref` is `ARG_MUST_BE_SENT_BY_REF`. `promoted` is
/// `ZEND_ARG_IS_PROMOTED`, which `zend_compile_params` sets on a
/// constructor parameter that declares a property, and which
/// `ReflectionParameter::isPromoted` reads.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // the ZEND_SEND_* and ZEND_ARG_* bits of a slot, each one fact
pub struct Parameter {
    pub name: Name,
    pub ty: Option<Type>,
    pub by_ref: bool,
    pub prefer_ref: bool,
    pub promoted: bool,
    pub variadic: bool,
    pub default: ParameterDefault,
    pub attributes: Vec<Attribute>,
}

impl Parameter {
    /// Whether the slot counts toward php's `required_num_args`: not the
    /// variadic one, and none with a default or `Unshown` after its name.
    pub fn is_required(&self) -> bool {
        !self.variadic && self.default == ParameterDefault::None
    }
}

/// A function's declaration: its signature, doc comment and site; the
/// body is the unit's op array of the same index.
///
/// `line` and `end_line` are `op_array.line_start` and `line_end`, the
/// lines of the `function` keyword and of the closing brace, and zero
/// on a native, whose `file` is empty. Reflection's `_function_string`
/// prints the pair as the `@@ file start - end` row it writes for a
/// user function alone.
///
/// `frameless` is a native's `zend_frameless_function_info` list
/// (`Z/Zend/zend_frameless_function.h`): the arities it serves without
/// a call frame, in the order php's `frameless_function_infos_*` table
/// lists them, `trim`'s being `[1, 2]`. php pairs each arity with a
/// handler of its own; here the handler is the native's one entry,
/// served over the arguments the opline carries, so the arity is the
/// whole row. Empty on a user function and on a native with no such
/// entry, as php's `frameless_function_infos` is null there. The
/// compiler emits `FRAMELESS_ICALL_<n>` for a call whose count an
/// arity here covers, and [`crate::frameless_table`] numbers the rows.
///
/// `is_generator` is `ZEND_ACC_GENERATOR` beside `returns_by_ref`'s
/// `ZEND_ACC_RETURN_REFERENCE`, the two `fn_flags` bits a declaration
/// carries: the body has a `yield` of its own, so its op array starts
/// with `GENERATOR_CREATE` after the parameters and every `RETURN*` in
/// it is `GENERATOR_RETURN` (`zend_mark_function_as_generator`,
/// `Z/Zend/zend_compile.c:1530`). What
/// `ReflectionFunctionAbstract::isGenerator` answers; false on every
/// native, as php has no internal generator function.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionDecl {
    pub name: Name,
    pub parameters: Vec<Parameter>,
    pub return_type: Option<Type>,
    pub returns_by_ref: bool,
    pub is_generator: bool,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<Box<[u8]>>,
    pub file: Name,
    pub line: u32,
    pub end_line: u32,
    pub frameless: Vec<u32>,
}

/// A native's declaration named `name`: no parameter, no return type,
/// the empty file and line zero, for its declarer to fill. A free
/// function rather than an associated one, since `api` re-exports
/// [`FunctionDecl`] and each of its methods would be an item of the
/// extension face.
#[must_use]
pub fn native_function(name: &[u8]) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: Vec::new(),
        return_type: None,
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name::default(),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

/// A parameter named `name` of type `ty`, required and by value.
#[must_use]
pub fn parameter(name: &[u8], ty: Option<Type>) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty,
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: ParameterDefault::None,
        attributes: Vec::new(),
    }
}

/// A method: a function with a visibility, whether it is an instance
/// member (`false` for a `static` method), and, on a native, the tentative
/// bit of its return type.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits of a method, each one fact
pub struct MethodDecl {
    pub function: FunctionDecl,
    pub visibility: Visibility,
    pub is_instance: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    pub tentative_return: bool,
}

/// A declared property, with its default still an expression.
///
/// `set_visibility` is php's asymmetric visibility, `ZEND_ACC_PUBLIC_SET`,
/// `ZEND_ACC_PROTECTED_SET` and `ZEND_ACC_PRIVATE_SET`: the second
/// visibility a write is held to, `None` where the property has one
/// visibility for both. `zend_declare_typed_property` drops a set
/// visibility equal to `visibility`, so what stands here is stricter
/// than `visibility` and never `Public`. `is_final` is `ZEND_ACC_FINAL`,
/// which the same function raises on a `private(set)` property.
/// `is_promoted` is `ZEND_ACC_PROMOTED`, the property a constructor's
/// promoted parameter declares, which `ReflectionProperty::isPromoted`
/// reads. `hooks` is php's `prop_info->hooks`, none for a property the
/// declaration wrote no hook list for; a property whose class inherits
/// hooks for it carries none of its own here, since the link finds an
/// inherited member by walking the chain.
///
/// `is_virtual` is `ZEND_ACC_VIRTUAL` as the declaration leaves it: the
/// property has no backing slot of its own. The compiler decides it as
/// `zend_property_is_virtual` does, a hooked property whose hooks never
/// name `$this->name`, and every hooked property of an interface; a
/// native's is the stub's `@virtual`, a property its class's handlers
/// serve with no hook, as `DOMNode::$nodeName` is. The link clears it
/// where a declaration the property overrides is backed
/// (`php_link::resolve_property`), and Reflection renders it.
///
/// ```php
/// $p = new ReflectionProperty('DOMNode', 'nodeName');
/// var_dump($p->isVirtual());
/// echo $p;
/// class A { public $v { get => 1; } public $b { get => $this->b; } }
/// echo new ReflectionProperty('A', 'v'), new ReflectionProperty('A', 'b');
/// ```
/// ```expect
/// bool(true)
/// Property [ public virtual string $nodeName ]
/// Property [ public virtual $v { get; } ]
/// Property [ public $b = NULL { get; } ]
/// ```
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits of a property, each one fact
pub struct PropertyDecl {
    pub name: Name,
    pub visibility: Visibility,
    pub set_visibility: Option<Visibility>,
    pub is_instance: bool,
    pub is_readonly: bool,
    pub is_final: bool,
    pub is_promoted: bool,
    pub is_virtual: bool,
    pub ty: Option<Type>,
    pub default: Option<ConstExpr>,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<Box<[u8]>>,
    pub hooks: Option<Box<PropertyHooks>>,
}

/// php's two hooks, `zend_property_hook_kind`, in the order
/// `prop_info->hooks` indexes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookKind {
    Get,
    Set,
}

impl HookKind {
    /// Both kinds, in php's order, `ZEND_PROPERTY_HOOK_GET` first.
    pub const ALL: [HookKind; 2] = [HookKind::Get, HookKind::Set];

    /// The name the hook is written with, `get` or `set`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            HookKind::Get => "get",
            HookKind::Set => "set",
        }
    }
}

/// A hooked property's hooks, `prop_info->hooks`, and the bit only a
/// hooked property carries. Each hook is a method as php keeps one: a
/// `zend_function` named `$name::get` or `$name::set`, public unless the
/// property is private, whose body is the unit's [`crate::HookBody`] of
/// the same class, property and kind. `is_abstract` is the property's
/// `ZEND_ACC_ABSTRACT`, written or given by an interface. Whether the
/// property is virtual is the property's own bit,
/// [`PropertyDecl::is_virtual`], which a property with no hooks carries
/// too.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyHooks {
    pub get: Option<MethodDecl>,
    pub set: Option<MethodDecl>,
    pub is_abstract: bool,
}

impl PropertyHooks {
    /// The hook of `kind`, `prop_info->hooks[kind]`.
    #[must_use]
    pub fn of(&self, kind: HookKind) -> Option<&MethodDecl> {
        match kind {
            HookKind::Get => self.get.as_ref(),
            HookKind::Set => self.set.as_ref(),
        }
    }
}

/// A class constant, or an enum's case: `is_case` is php's
/// `ZEND_CLASS_CONST_IS_CASE`, which `case A;` sets and `const A = 1;`
/// inside the same enum does not, so an enum's constants table holds
/// both. A case's `value` is its backing value as written, `Null` for a
/// pure enum's; the case itself is the one instance the engine makes of
/// the enum for it (`zend_enum_new`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClassConstDecl {
    pub name: Name,
    pub visibility: Visibility,
    pub is_final: bool,
    pub is_case: bool,
    pub ty: Option<Type>,
    pub value: ConstExpr,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<Box<[u8]>>,
}

/// A method a trait adaptation names, php's `zend_trait_method_reference`:
/// `T::f` with the trait written, or `f` alone, which the link resolves
/// to the one used trait that has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitMethodRef {
    pub trait_name: Option<Name>,
    pub method: Name,
}

/// The one modifier an `as` may write, php's `member_modifier` under
/// `ZEND_MODIFIER_TARGET_METHOD` less the `static` and `abstract`
/// `zend_check_trait_alias_modifiers` refuses: a visibility the copy
/// takes, or `final`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasModifier {
    Visibility(Visibility),
    Final,
}

/// One adaptation of a class's trait `use` list, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraitAdaptation {
    /// `T::f insteadof A, B;`, php's `zend_trait_precedence`: the
    /// method of `trait_name` is the one taken, the same-named method
    /// of each trait `excluded` names is left out.
    Insteadof {
        trait_name: Name,
        method: Name,
        excluded: Vec<Name>,
    },
    /// `[T::]f as [modifier] [alias];`, php's `zend_trait_alias`: a
    /// copy of the method under `alias`, or the method itself with the
    /// modifier where no alias is written. The grammar writes at least
    /// one of the two.
    As {
        method: TraitMethodRef,
        modifier: Option<AliasModifier>,
        alias: Option<Name>,
    },
}

/// A class, interface, trait or enum, unlinked: names, not identities.
/// `line` and `end_line` are `ce->info.user.line_start` and `line_end`,
/// the lines of the keyword and of the closing brace, and zero on a
/// native, whose `file` is empty.
///
/// `traits` is php's `ce->trait_names`, every trait each `use` of the
/// body names, in the order written, and `adaptations` the
/// `ce->trait_precedences` and `ce->trait_aliases` of their adaptation
/// lists, in one list in the order written; the link reads each kind
/// in its own order (`zend_traits_init_trait_structures`).
///
/// `implementors` is what an interface's `interface_gets_implemented`
/// holds a class to where php gives an extension's interface one: the
/// classes a class implementing it must descend from, as ext/date's
/// `date_interface_gets_implemented` holds `DateTimeInterface` to
/// `DateTime` and `DateTimeImmutable`; empty for any other. `aliases`
/// are a native class's stub `@alias` names, which `register_class_*`
/// registers with `zend_register_class_alias` right after the class;
/// empty for any other.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDecl {
    pub name: Name,
    pub kind: ClassKind,
    pub flags: ClassFlags,
    pub parent: Option<Name>,
    pub interfaces: Vec<Name>,
    pub implementors: Vec<Name>,
    pub aliases: Vec<Name>,
    pub traits: Vec<Name>,
    pub adaptations: Vec<TraitAdaptation>,
    pub backing: EnumBacking,
    pub constants: Vec<ClassConstDecl>,
    pub properties: Vec<PropertyDecl>,
    pub methods: Vec<MethodDecl>,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<Box<[u8]>>,
    pub file: Name,
    pub line: u32,
    pub end_line: u32,
    /// `sizeof` of the struct an internal class's `create_object` asks
    /// `zend_object_alloc` for, its `zend_object` included: what
    /// `memory_get_usage()` counts of each object beside the properties
    /// (design §5.13). `0` where the class and every class above it lay
    /// a plain `zend_object` down, which every class a program declares
    /// does. A class whose `create_object` asks `emalloc(sizeof(..))`
    /// whole, as `Closure`'s, `Generator`'s and `Fiber`'s do, keeps the
    /// properties slot `zend_object_alloc` would give back: its size is
    /// the struct's and sixteen.
    pub object_size: u32,
    /// The `emalloc`s the same `create_object` makes beside the object,
    /// each the size php asks for (a random engine's state, a
    /// connection's `pdo_dbh_t`), given back as the object is freed
    /// (design §5.13). Empty where it makes none.
    pub object_beside: Box<[u32]>,
    /// The validator an extension's attribute class declares,
    /// `zend_internal_attribute.validator` as its `MINIT` sets it after
    /// `zend_mark_internal_attribute`; none on every other class, a
    /// program's among them.
    pub attribute_validator: Option<AttributeValidator>,
}

/// A global constant, `const X = ...;` or `define()` folded at compile time.
///
/// `attributes` is php's `zend_constant::attributes`, the table
/// `zend_add_global_constant_attribute` hangs on a constant an
/// extension registered and `ZEND_DECLARE_ATTRIBUTED_CONST` hangs on
/// one a program wrote. `#[\Deprecated]` among them is php's
/// `CONST_DEPRECATED`: `gen_stub` raises the bit from the attribute in
/// the stub (`Z/Zend/zend_constants_arginfo.h:17` for `E_STRICT`) and
/// `zend_compile_attributes`' validator raises it for a program's own,
/// so the attribute is the whole of what `CONST_DEPRECATED` says and
/// no second field carries it.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstDecl {
    pub name: Name,
    pub value: ConstExpr,
    pub attributes: Vec<Attribute>,
}

/// An ini entry an extension declares: its default and where it may be
/// changed (`PHP_INI_USER`, `PHP_INI_PERDIR`, `PHP_INI_SYSTEM`). The
/// default is `None` where php registers the entry with no value at all,
/// `NULL`, which `ini_get()` reads as the empty string and an entry's
/// `on_modify` is handed apart from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniDecl {
    pub name: Name,
    pub default: Option<Box<[u8]>>,
    pub user: bool,
    pub per_dir: bool,
    pub system: bool,
}

#[cfg(test)]
mod tests {
    use super::{Bare, Name};

    #[test]
    fn a_bare_name_drops_one_leading_backslash_once() {
        assert_eq!(Bare::of(b"\\\\Foo").bytes(), b"\\Foo");
        assert_eq!(Bare::of(b"Foo").bytes(), b"Foo");
        let name = Name::from("NS\\Foo");
        assert_eq!(Bare::from(&name).bytes(), b"NS\\Foo");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a name as written handed to Bare::resolved")]
    fn a_resolved_name_refuses_a_leading_backslash_in_a_debug_build() {
        assert!(Bare::resolved(b"\\Foo").bytes().is_empty());
    }
}
