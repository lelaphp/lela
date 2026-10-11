//! The attributes the engine itself declares and validates,
//! `zend_attributes.c`: each one's `#[Attribute]` targets as its stub
//! writes them, and the validator `zend_compile_attributes` runs over it
//! once its target holds (`validate_attribute`,
//! `validate_allow_dynamic_properties`, `validate_deprecated`). The
//! compiler reads them, and an extension's attribute class beside them
//! ([`Config`]), where php's `zend_internal_attribute_get` answers, by
//! the attribute's lowered name.

use php_unit::decl::{
    Attribute, AttributeTarget, AttributeValidator, ClassDecl, ClassFlags, ClassKind, ConstExpr,
};

use super::walk::ClassModifiers;

/// `ZEND_ATTRIBUTE_TARGET_*` (`Z/Zend/zend_attributes.h`), each the bit
/// its [`AttributeTarget`] is.
pub(super) const TARGET_CLASS: u32 = AttributeTarget::Class as u32;
pub(super) const TARGET_FUNCTION: u32 = AttributeTarget::Function as u32;
pub(super) const TARGET_METHOD: u32 = AttributeTarget::Method as u32;
pub(super) const TARGET_PROPERTY: u32 = AttributeTarget::Property as u32;
pub(super) const TARGET_CLASS_CONST: u32 = AttributeTarget::ClassConstant as u32;
pub(super) const TARGET_PARAMETER: u32 = AttributeTarget::Parameter as u32;
pub(super) const TARGET_CONST: u32 = AttributeTarget::Constant as u32;
const TARGET_ALL: u32 = (1 << 7) - 1;
/// `ZEND_ATTRIBUTE_IS_REPEATABLE`.
const IS_REPEATABLE: u32 = 1 << 7;

/// `Attribute`'s constants by the names its stub declares them under.
const FLAG_NAMES: [(&str, u32); 9] = [
    ("TARGET_CLASS", TARGET_CLASS),
    ("TARGET_FUNCTION", TARGET_FUNCTION),
    ("TARGET_METHOD", TARGET_METHOD),
    ("TARGET_PROPERTY", TARGET_PROPERTY),
    ("TARGET_CLASS_CONSTANT", TARGET_CLASS_CONST),
    ("TARGET_PARAMETER", TARGET_PARAMETER),
    ("TARGET_CONSTANT", TARGET_CONST),
    ("TARGET_ALL", TARGET_ALL),
    ("IS_REPEATABLE", IS_REPEATABLE),
];

/// `target_names`, in the order of the bits.
const TARGET_NAMES: [&str; 7] = [
    "class",
    "function",
    "method",
    "property",
    "class constant",
    "parameter",
    "constant",
];

/// The attributes `zend_register_attribute_ce` and its neighbours mark
/// internal, one variant each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum InternalAttribute {
    Attribute,
    ReturnTypeWillChange,
    AllowDynamicProperties,
    SensitiveParameter,
    Override,
    Deprecated,
    NoDiscard,
    DelayedTargetValidation,
}

impl InternalAttribute {
    /// Every one, in the order `zend_register_attribute_ce` marks them.
    const ALL: [InternalAttribute; 8] = [
        InternalAttribute::Attribute,
        InternalAttribute::ReturnTypeWillChange,
        InternalAttribute::AllowDynamicProperties,
        InternalAttribute::SensitiveParameter,
        InternalAttribute::Override,
        InternalAttribute::Deprecated,
        InternalAttribute::NoDiscard,
        InternalAttribute::DelayedTargetValidation,
    ];

    /// `zend_internal_attribute_get`: the internal attribute a resolved
    /// name is, whatever its case, none for a user's.
    pub(super) fn named(name: &[u8]) -> Option<InternalAttribute> {
        InternalAttribute::ALL
            .into_iter()
            .find(|attribute| attribute.declared().as_bytes().eq_ignore_ascii_case(name))
    }

    /// The class's name as `zend_attributes.stub.php` declares it.
    fn declared(self) -> &'static str {
        match self {
            InternalAttribute::Attribute => "Attribute",
            InternalAttribute::ReturnTypeWillChange => "ReturnTypeWillChange",
            InternalAttribute::AllowDynamicProperties => "AllowDynamicProperties",
            InternalAttribute::SensitiveParameter => "SensitiveParameter",
            InternalAttribute::Override => "Override",
            InternalAttribute::Deprecated => "Deprecated",
            InternalAttribute::NoDiscard => "NoDiscard",
            InternalAttribute::DelayedTargetValidation => "DelayedTargetValidation",
        }
    }

    /// The targets its stub's `#[Attribute(...)]` allows; none of them
    /// sets `Attribute::IS_REPEATABLE`.
    pub(super) fn targets(self) -> u32 {
        match self {
            InternalAttribute::Attribute | InternalAttribute::AllowDynamicProperties => {
                TARGET_CLASS
            }
            InternalAttribute::ReturnTypeWillChange => TARGET_METHOD,
            InternalAttribute::SensitiveParameter => TARGET_PARAMETER,
            InternalAttribute::Override => TARGET_METHOD | TARGET_PROPERTY,
            InternalAttribute::Deprecated => {
                TARGET_METHOD | TARGET_FUNCTION | TARGET_CLASS_CONST | TARGET_CONST | TARGET_CLASS
            }
            InternalAttribute::NoDiscard => TARGET_METHOD | TARGET_FUNCTION,
            InternalAttribute::DelayedTargetValidation => TARGET_ALL,
        }
    }

    /// The validator `zend_compile_attributes` runs once the target
    /// holds, over a class of this kind, modifiers and name: php's
    /// compile error, none where it takes the class
    /// ([`php_unit::decl::class_attribute_refusal`]).
    ///
    /// ```php
    /// #[Attribute]
    /// abstract class A {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot apply #[\Attribute] to abstract class A in %s on line 3
    /// ```
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// readonly class A {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot apply #[\AllowDynamicProperties] to readonly class A in %s on line 3
    /// ```
    pub(super) fn refused_on_class(
        self,
        kind: ClassKind,
        modifiers: ClassModifiers,
        class: &str,
    ) -> Option<String> {
        let flags = ClassFlags {
            is_abstract: modifiers.is_abstract,
            is_final: modifiers.is_final,
            is_readonly: modifiers.is_readonly,
            ..ClassFlags::default()
        };
        php_unit::decl::class_attribute_refusal(self.declared().as_bytes(), kind, &flags, class)
    }
}

/// `zend_internal_attribute`, what `zend_internal_attribute_get` answers
/// for an attribute class of the engine: one of Core's, whose validator
/// the compiler runs, or one an extension declares with `#[Attribute]`
/// (`zend_mark_internal_attribute`), with the flags its `#[Attribute]`
/// was given, the targets it may stand on and whether it may repeat,
/// with the validator the extension declared for it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Config {
    /// Core's attribute, none for an extension's.
    pub(super) core: Option<InternalAttribute>,
    flags: u32,
    /// `zend_internal_attribute.validator` of an extension's attribute
    /// class, none for Core's, whose validators the compiler runs
    /// itself.
    pub(super) validator: Option<AttributeValidator>,
}

impl Config {
    /// The one of Core's that a resolved name is, or the class of
    /// `classes` (the bundle's, php's `CG(class_table)` of internal
    /// classes) declared with `#[Attribute]`; none for a user's.
    pub(super) fn named<'a>(
        name: &[u8],
        classes: impl FnOnce(&[u8]) -> Option<&'a ClassDecl>,
    ) -> Option<Config> {
        if let Some(core) = InternalAttribute::named(name) {
            return Some(Config {
                core: Some(core),
                flags: core.targets(),
                validator: None,
            });
        }
        let class = classes(name)?;
        let attribute = class
            .attributes
            .iter()
            .find(|attribute| attribute.name.0.eq_ignore_ascii_case(b"Attribute"))?;
        // `Z_LVAL(attr->args[0].value)`; `Attribute`'s own default where
        // the class gives none.
        let flags = match attribute.arguments.first() {
            Some(argument) => flags_of(&argument.value)?,
            None => TARGET_ALL,
        };
        Some(Config {
            core: None,
            flags,
            validator: class.attribute_validator,
        })
    }

    /// The targets it may stand on, `flags & ZEND_ATTRIBUTE_TARGET_ALL`.
    pub(super) fn targets(self) -> u32 {
        self.flags & TARGET_ALL
    }

    /// `ZEND_ATTRIBUTE_IS_REPEATABLE`.
    pub(super) fn repeatable(self) -> bool {
        self.flags & IS_REPEATABLE != 0
    }
}

/// The integer a native's `#[Attribute(...)]` argument stands for, as
/// `gen_stub` folds it into the `zval` it hands `zend_add_class_attribute`:
/// an integer, `Attribute`'s `TARGET_*` and `IS_REPEATABLE` by name, and
/// `|` over them.
fn flags_of(value: &ConstExpr) -> Option<u32> {
    match value {
        ConstExpr::Long { value, .. } => u32::try_from(*value).ok(),
        ConstExpr::ClassConstant { class, name, .. }
            if class.0.eq_ignore_ascii_case(b"Attribute") =>
        {
            FLAG_NAMES
                .iter()
                .find(|(known, _)| known.as_bytes() == &*name.0)
                .map(|(_, bits)| *bits)
        }
        ConstExpr::Binary { op, lhs, rhs, .. } if &*op.0 == b"|" => {
            Some(flags_of(lhs)? | flags_of(rhs)?)
        }
        _ => None,
    }
}

/// `zend_get_attribute_str(*attributes, "delayedtargetvalidation")`:
/// whether a declaration's attributes carry
/// `#[\DelayedTargetValidation]`, which defers every target check over
/// them to `ReflectionAttribute::newInstance`.
pub(super) fn delays_validation(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        InternalAttribute::named(&attribute.name.0)
            == Some(InternalAttribute::DelayedTargetValidation)
    })
}

/// Whether a declaration's attributes carry `#[\Deprecated]`, which
/// `zend_compile_class_const_decl` raises `ZEND_ACC_DEPRECATED` on a
/// class constant for, so `zend_try_ct_eval_class_const` substitutes
/// it nowhere.
///
/// ```php
/// class C {
///     #[Deprecated] const X = 1;
///     function f() { return self::X; }
/// }
/// echo (new C)->f(), "\n";
/// ```
/// ```expect
/// Deprecated: Constant C::X is deprecated in %s on line 4
/// 1
/// ```
pub(super) fn is_deprecated(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        InternalAttribute::named(&attribute.name.0) == Some(InternalAttribute::Deprecated)
    })
}

/// Whether a function's attributes raise `ZEND_ACC_DEPRECATED`, which
/// `zend_compile_func_decl` sets for a `#[\Deprecated]`, or
/// `ZEND_ACC_NODISCARD`, which `validate_nodiscard` sets: the two bits
/// `zend_get_call_op` keeps a known callee's call off `DO_ICALL` and
/// `DO_UCALL` for, since only `DO_FCALL` and `DO_FCALL_BY_NAME` test
/// them. Whether what the call returns is used is not known where the
/// call op is picked, so a `#[\NoDiscard]` callee is kept off them
/// whatever is done with it.
///
/// ```php
/// #[Deprecated] function d() { return 1; }
/// #[NoDiscard] function n() { return 1; }
/// class C { #[NoDiscard] static function m() { return 1; } }
/// $x = d();
/// $y = n();
/// $z = C::m();
/// echo $x + $y + $z, "\n";
/// ```
/// ```expect
/// Deprecated: Function d() is deprecated in %s on line 5
/// 3
/// ```
pub(super) fn deprecated_or_no_discard(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        matches!(
            InternalAttribute::named(&attribute.name.0),
            Some(InternalAttribute::Deprecated | InternalAttribute::NoDiscard)
        )
    })
}

/// `zend_get_attribute_target_names`: the targets of a mask, in bit
/// order, joined as php joins them.
pub(super) fn target_names(flags: u32) -> String {
    TARGET_NAMES
        .iter()
        .enumerate()
        .filter(|(bit, _)| flags & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}
