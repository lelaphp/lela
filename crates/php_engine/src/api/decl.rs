//! The declaration group (design §4.4): re-exports of `php_unit`'s model,
//! so there is no second one. What a native class or function declares is
//! what the compiler emits for user code.

pub use php_unit::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstDecl,
    ConstExpr, DynamicProperties, EnumBacking, FunctionDecl, IniDecl, MethodDecl, Name, Parameter,
    ParameterDefault, PropertyDecl, Type, Visibility, class_attribute_refusal,
};
