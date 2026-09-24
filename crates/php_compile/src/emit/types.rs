//! Declared types, after `zend_compile_typename_ex` and
//! `zend_compile_single_typename` (`Z/Zend/zend_compile.c`): a type
//! node of the tree becomes the unit's [`Type`] where php compiles it,
//! at a return type, a parameter and a property, and what php refuses
//! about the type as written is refused there in php's words, each at
//! the line the compiler stands on. What the refusals print is the
//! type as `zend_type_to_string` writes the `zend_type` php built
//! ([`php_unit::type_to_string`]), so the order in a message is php's
//! and not the order written.

use php_syntax::tree;
use php_syntax::tree::{NodeId, TypeExpr};
use php_unit::Level;
use php_unit::decl::{Name, Type};
use php_unit::type_to_string;

use super::walk::unread;
use super::{Emit, FETCH_CLASS_DEFAULT, FETCH_CLASS_PARENT, FETCH_CLASS_SELF, class_fetch_type};
use crate::CompileError;

/// `MAY_BE_NULL`, `MAY_BE_FALSE`, `MAY_BE_TRUE`, `MAY_BE_OBJECT`,
/// `MAY_BE_VOID`, `MAY_BE_STATIC` and `MAY_BE_NEVER`
/// (`Z/Zend/zend_type_info.h`), the bits the refusals below read.
pub(super) const MAY_BE_NULL: u32 = 1 << 1;
const MAY_BE_FALSE: u32 = 1 << 2;
const MAY_BE_TRUE: u32 = 1 << 3;
pub(super) const MAY_BE_OBJECT: u32 = 1 << 8;
pub(super) const MAY_BE_CALLABLE: u32 = 1 << 12;
pub(super) const MAY_BE_VOID: u32 = 1 << 14;
const MAY_BE_STATIC: u32 = 1 << 15;
pub(super) const MAY_BE_NEVER: u32 = 1 << 17;

impl Emit<'_> {
    /// `zend_compile_typename_ex`: the type `ast` declares, with
    /// whether `force_allow_null` made it nullable where it was not
    /// written so, which is the PHP 5 spelling `T $x = null` of `?T`.
    /// A type marked nullable comes back as [`Type::Nullable`]; one
    /// made nullable by the default does not, and the caller wraps it.
    ///
    /// ```php
    /// function f(?mixed $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type mixed cannot be marked as nullable since mixed already includes null in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(?null $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: null cannot be marked as nullable in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(): void|int {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Void can only be used as a standalone type in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(): ?never {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: never can only be used as a standalone type in %s on line 2
    /// ```
    ///
    /// What php takes, it takes, a class name resolved in the namespace
    /// it is written in and `self` named after the class it stands in:
    ///
    /// ```php
    /// namespace N;
    /// class C { function f(self|int|null $a, ?iterable $b): self|false|null { return $a; } }
    /// $c = new C;
    /// var_dump($c->f($c, []) === $c, $c->f(null, null));
    /// ```
    /// ```expect exit=0
    /// bool(true)
    /// NULL
    /// ```
    pub(super) fn typename(
        &mut self,
        ast: TypeExpr,
        force_allow_null: bool,
    ) -> Result<(Type, bool), CompileError> {
        let (marked_nullable, written) = match ast {
            TypeExpr::NullableType(id) => match self.parsed.tree[id].ty {
                Some(inner) => (true, inner),
                None => return Err(unread(self.line, "a type")),
            },
            other => (false, other),
        };
        let ty = match written {
            TypeExpr::NamedType(id) => self.single_typename(id)?,
            TypeExpr::UnionType(id) => {
                let members = self.parsed.tree[id].types.to_vec();
                self.union_type(&members)?
            }
            TypeExpr::IntersectionType(id) => {
                let members = self.parsed.tree[id].types.to_vec();
                self.intersection_type(&members)?
            }
            TypeExpr::ParenType(_) => return Err(unread(self.line, "a DNF type")),
            TypeExpr::NullableType(_) => return Err(unread(self.line, "a type")),
        };
        let mut mask = ty.mask();
        if mask == Type::MAY_BE_ANY && marked_nullable {
            return Err(self.compile_error(
                "Type mixed cannot be marked as nullable since mixed already includes null",
            ));
        }
        if mask & MAY_BE_NULL != 0 && marked_nullable {
            return Err(self.compile_error("null cannot be marked as nullable"));
        }
        let forced = force_allow_null && !marked_nullable && mask & MAY_BE_NULL == 0;
        if marked_nullable || force_allow_null {
            mask |= MAY_BE_NULL;
        }
        let complex = is_complex(&ty);
        if mask & MAY_BE_VOID != 0 && (complex || mask != MAY_BE_VOID) {
            return Err(self.compile_error("Void can only be used as a standalone type"));
        }
        if mask & MAY_BE_NEVER != 0 && (complex || mask != MAY_BE_NEVER) {
            return Err(self.compile_error("never can only be used as a standalone type"));
        }
        let ty = if marked_nullable {
            Type::Nullable(Box::new(ty))
        } else {
            ty
        };
        Ok((ty, forced))
    }

    /// The union arm of `zend_compile_typename_ex`: each member in
    /// turn, a scalar type whose bit an earlier member set and a class an
    /// earlier member names being php's `Duplicate type`, and after
    /// them all `object` beside a class name.
    ///
    /// ```php
    /// function f(int|string|INT $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Duplicate type int is redundant in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(): Foo|Bar|FOO {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Duplicate type FOO is redundant in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(iterable|array $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Duplicate type array is redundant in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(mixed|int $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type mixed can only be used as a standalone type in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(true|false $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type contains both true and false, bool must be used instead in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(): T|iterable|object|null {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type T|Traversable|object|array|null contains both object and a class type, which is redundant in %s on line 2
    /// ```
    fn union_type(&mut self, members: &[TypeExpr]) -> Result<Type, CompileError> {
        let mut mask = 0;
        let mut out: Vec<Type> = Vec::with_capacity(members.len());
        let mut has_only_iterable_class = true;
        for member in members {
            let single = match member {
                TypeExpr::NamedType(id) => self.single_typename(*id)?,
                TypeExpr::IntersectionType(id) => {
                    has_only_iterable_class = false;
                    let inner = self.parsed.tree[*id].types.to_vec();
                    let single = self.intersection_type(&inner)?;
                    for earlier in out.iter().filter(|ty| is_complex(ty)) {
                        match earlier {
                            Type::Intersection(_) => {
                                self.intersections_redundant(&single, earlier)?;
                            }
                            _ => self.intersection_redundant_by_single(&single, earlier)?,
                        }
                    }
                    out.push(single);
                    continue;
                }
                TypeExpr::ParenType(_) => return Err(unread(self.line, "a DNF type")),
                TypeExpr::NullableType(_) | TypeExpr::UnionType(_) => {
                    return Err(unread(self.line, "a type"));
                }
            };
            let single_mask = single.mask();
            if single_mask == Type::MAY_BE_ANY {
                return Err(self.compile_error("Type mixed can only be used as a standalone type"));
            }
            if is_complex(&single) && single != Type::Iterable {
                has_only_iterable_class = false;
            }
            let overlap = mask & single_mask;
            if overlap != 0 {
                return Err(self.compile_error(format!(
                    "Duplicate type {} is redundant",
                    type_to_string(&of_mask(overlap))
                )));
            }
            if (mask & MAY_BE_TRUE != 0 && single_mask == MAY_BE_FALSE)
                || (mask & MAY_BE_FALSE != 0 && single_mask == MAY_BE_TRUE)
            {
                return Err(self.compile_error(
                    "Type contains both true and false, bool must be used instead",
                ));
            }
            mask |= single_mask;
            if is_complex(&single) {
                self.list_redundant_by_single(&out, &single)?;
            }
            out.push(single);
        }
        let ty = Type::Union(out);
        if mask & MAY_BE_OBJECT != 0
            && ((!has_only_iterable_class && is_complex(&ty)) || mask & MAY_BE_STATIC != 0)
        {
            return Err(self.compile_error(format!(
                "Type {} contains both object and a class type, which is redundant",
                type_to_string(&ty)
            )));
        }
        Ok(ty)
    }

    /// The intersection arm of `zend_compile_typename_ex`: class names
    /// alone, each a class no earlier member names.
    ///
    /// ```php
    /// function f(A&int $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type int cannot be part of an intersection type in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(A&iterable $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type Traversable|array cannot be part of an intersection type in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(): A&B&A {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Duplicate type A is redundant in %s on line 2
    /// ```
    fn intersection_type(&mut self, members: &[TypeExpr]) -> Result<Type, CompileError> {
        let mut out: Vec<Type> = Vec::with_capacity(members.len());
        for member in members {
            let TypeExpr::NamedType(id) = member else {
                return Err(unread(self.line, "a type"));
            };
            let single = self.single_typename(*id)?;
            if single == Type::Iterable || !is_complex(&single) {
                return Err(self.compile_error(format!(
                    "Type {} cannot be part of an intersection type",
                    type_to_string(&single)
                )));
            }
            if let Type::SelfClass | Type::ParentClass = single {
                return Err(self.compile_error(format!(
                    "Type {} cannot be part of an intersection type",
                    String::from_utf8_lossy(self.text(self.parsed.tree[*id].name))
                )));
            }
            self.list_redundant_by_single(&out, &single)?;
            out.push(single);
        }
        Ok(Type::Intersection(out))
    }

    /// `zend_compile_single_typename`: `static`, `array` and `callable`,
    /// which the grammar writes as keywords; a scalar or special type by name, which
    /// may not be written with a `\`; and a class, `self` and `parent`
    /// held to the scope and named after the class where it is known,
    /// and any other name resolved in the namespace and held to the
    /// reserved names.
    ///
    /// ```php
    /// function f(): static {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "static" when no class scope is active in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(parent $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "parent" when no class scope is active in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(\int $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type declaration 'int' must be unqualified in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(bar\int $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "bar\int" as a type name as it is reserved in %s on line 2
    /// ```
    fn single_typename(&mut self, id: NodeId<tree::NamedType>) -> Result<Type, CompileError> {
        let written = self.text(self.parsed.tree[id].name).to_vec();
        let lowered = written.to_ascii_lowercase();
        match lowered.as_slice() {
            b"static" => {
                if self.active_class.is_none() && self.scope_known() {
                    return Err(
                        self.compile_error("Cannot use \"static\" when no class scope is active")
                    );
                }
                return Ok(Type::CalledClass);
            }
            b"array" => return Ok(Type::Array),
            b"callable" => return Ok(Type::Callable),
            _ => {}
        }
        let fully_qualified = written.first() == Some(&b'\\');
        let bare = if fully_qualified {
            &lowered[1..]
        } else {
            &lowered[..]
        };
        if let Some(builtin) = builtin_type(bare) {
            if fully_qualified {
                return Err(self.compile_error(format!(
                    "Type declaration '{}' must be unqualified",
                    String::from_utf8_lossy(bare)
                )));
            }
            return Ok(builtin);
        }
        let fetch = if fully_qualified {
            FETCH_CLASS_DEFAULT
        } else {
            class_fetch_type(&written)
        };
        if fetch == FETCH_CLASS_DEFAULT {
            let resolved = self.class_name(&written);
            let resolved = match resolved.strip_prefix(b"\\") {
                Some(rest) => rest.to_vec(),
                None => resolved,
            };
            let unqualified = resolved.rsplit(|b| *b == b'\\').next().unwrap_or_default();
            if is_reserved_class_name(unqualified) {
                return Err(self.compile_error(format!(
                    "Cannot use \"{}\" as a type name as it is reserved",
                    String::from_utf8_lossy(&resolved)
                )));
            }
            if resolved == b"_" {
                self.raise(
                    Level::Deprecated,
                    "Using \"_\" as a type name is deprecated since 8.4",
                );
            }
            return Ok(Type::Named(Name(resolved.into_boxed_slice())));
        }
        self.ensure_valid_class_fetch_type(fetch)?;
        if !self.scope_known() {
            return Ok(if fetch == FETCH_CLASS_SELF {
                Type::SelfClass
            } else {
                Type::ParentClass
            });
        }
        let substituted = match fetch {
            FETCH_CLASS_PARENT => self.active_parent.clone(),
            _ => self.active_class.clone(),
        };
        Ok(match substituted {
            Some(name) => Type::Named(Name(name.into_boxed_slice())),
            None if fetch == FETCH_CLASS_SELF => Type::SelfClass,
            None => Type::ParentClass,
        })
    }

    /// `zend_is_type_list_redundant_by_single_type`: a class the list
    /// already names, or one an intersection in it already requires.
    fn list_redundant_by_single(&self, list: &[Type], single: &Type) -> Result<(), CompileError> {
        for earlier in list.iter().filter(|ty| is_complex(ty)) {
            if let Type::Intersection(_) = earlier {
                self.intersection_redundant_by_single(earlier, single)?;
                continue;
            }
            if class_name_of(earlier).eq_ignore_ascii_case(&class_name_of(single)) {
                return Err(self.compile_error(format!(
                    "Duplicate type {} is redundant",
                    type_to_string(single)
                )));
            }
        }
        Ok(())
    }

    /// `zend_is_intersection_type_redundant_by_single_type`: a union
    /// member that is one of an intersection's own classes makes the
    /// intersection redundant.
    fn intersection_redundant_by_single(
        &self,
        intersection: &Type,
        single: &Type,
    ) -> Result<(), CompileError> {
        let Type::Intersection(members) = intersection else {
            return Ok(());
        };
        let name = class_name_of(single);
        if members
            .iter()
            .any(|member| class_name_of(member).eq_ignore_ascii_case(&name))
        {
            return Err(self.compile_error(format!(
                "Type {} is redundant as it is more restrictive than type {}",
                type_to_string(intersection),
                type_to_string(single)
            )));
        }
        Ok(())
    }

    /// `zend_are_intersection_types_redundant`: two intersections of a
    /// union, one of which requires every class of the other.
    fn intersections_redundant(&self, left: &Type, right: &Type) -> Result<(), CompileError> {
        let (Type::Intersection(l), Type::Intersection(r)) = (left, right) else {
            return Ok(());
        };
        let (smaller, larger, smaller_ty, larger_ty) = if r.len() < l.len() {
            (r, l, right, left)
        } else {
            (l, r, left, right)
        };
        let found = smaller
            .iter()
            .filter(|outer| {
                larger
                    .iter()
                    .any(|inner| class_name_of(inner).eq_ignore_ascii_case(&class_name_of(outer)))
            })
            .count();
        if found != smaller.len() {
            return Ok(());
        }
        let (smaller_text, larger_text) = (type_to_string(smaller_ty), type_to_string(larger_ty));
        Err(self.compile_error(if smaller.len() == larger.len() {
            format!("Type {smaller_text} is redundant with type {larger_text}")
        } else {
            format!(
                "Type {larger_text} is redundant as it is more restrictive than type {smaller_text}"
            )
        }))
    }
}

/// `ZEND_TYPE_IS_COMPLEX`: whether a type names a class, which a class
/// name, `self`, `parent`, `iterable`'s `Traversable` and an
/// intersection do.
pub(super) fn is_complex(ty: &Type) -> bool {
    match ty {
        Type::Named(_)
        | Type::SelfClass
        | Type::ParentClass
        | Type::Iterable
        | Type::Intersection(_) => true,
        Type::Nullable(inner) => is_complex(inner),
        Type::Union(members) => members.iter().any(is_complex),
        _ => false,
    }
}

/// `ZEND_TYPE_NAME` of a type with one name.
fn class_name_of(ty: &Type) -> Vec<u8> {
    match ty {
        Type::Named(name) => name.0.to_vec(),
        Type::SelfClass => b"self".to_vec(),
        Type::ParentClass => b"parent".to_vec(),
        Type::Iterable => b"Traversable".to_vec(),
        _ => Vec::new(),
    }
}

/// `ZEND_TYPE_INIT_MASK`: the builtins of a pure mask as a type, which
/// is what php hands `zend_type_to_string` to name an overlap or the
/// type a magic method must declare.
pub(super) fn of_mask(mask: u32) -> Type {
    if mask & Type::MAY_BE_ANY == Type::MAY_BE_ANY {
        return Type::Mixed;
    }
    let mut members = Vec::new();
    for (bit, ty) in [
        (MAY_BE_STATIC, Type::CalledClass),
        (MAY_BE_CALLABLE, Type::Callable),
        (MAY_BE_OBJECT, Type::Object),
        (1 << 7, Type::Array),
        (1 << 6, Type::String),
        (1 << 4, Type::Int),
        (1 << 5, Type::Float),
        (MAY_BE_FALSE, Type::False),
        (MAY_BE_TRUE, Type::True),
        (MAY_BE_VOID, Type::Void),
        (MAY_BE_NEVER, Type::Never),
        (MAY_BE_NULL, Type::Null),
    ] {
        if mask & bit != 0 {
            members.push(ty);
        }
    }
    match members.len() {
        1 => members.pop().unwrap_or(Type::Null),
        _ => Type::Union(members),
    }
}

/// `zend_lookup_builtin_type_by_name`: the builtins php knows by name,
/// whatever the case, which a class may not be called.
fn builtin_type(lowered: &[u8]) -> Option<Type> {
    Some(match lowered {
        b"null" => Type::Null,
        b"true" => Type::True,
        b"false" => Type::False,
        b"int" => Type::Int,
        b"float" => Type::Float,
        b"string" => Type::String,
        b"bool" => Type::Bool,
        b"void" => Type::Void,
        b"never" => Type::Never,
        b"iterable" => Type::Iterable,
        b"object" => Type::Object,
        b"mixed" => Type::Mixed,
        _ => return None,
    })
}

/// `zend_is_reserved_class_name` over an unqualified name: the names
/// `reserved_class_names` lists, whatever the case.
pub(super) fn is_reserved_class_name(name: &[u8]) -> bool {
    matches!(
        name.to_ascii_lowercase().as_slice(),
        b"bool"
            | b"false"
            | b"float"
            | b"int"
            | b"null"
            | b"parent"
            | b"self"
            | b"static"
            | b"string"
            | b"true"
            | b"void"
            | b"never"
            | b"iterable"
            | b"object"
            | b"mixed"
            | b"array"
            | b"callable"
    )
}
