//! The declaration group (design §4.4): re-exports of `php_unit`'s model,
//! so there is no second one. What a native class or function declares is
//! what the compiler emits for user code.

pub use php_unit::decl::{
    Attribute, AttributeArgument, AttributeTarget, AttributeValidator, ClassConstDecl, ClassDecl,
    ClassFlags, ClassKind, ConstDecl, ConstExpr, ConstantName, DynamicProperties, EnumBacking,
    FunctionDecl, HookKind, IniDecl, MethodDecl, Name, NameKind, Parameter, ParameterDefault,
    PropertyDecl, PropertyHooks, Radix, TraitAdaptation, Type, Visibility, class_attribute_refusal,
    hook_attribute_refusal,
};
