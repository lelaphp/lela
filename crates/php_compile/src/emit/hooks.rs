//! `zend_compile_property_hooks` (`Z/Zend/zend_compile.c:8655`, #427):
//! a property's hook list, each hook compiled to an op array of its
//! own named `$name::get` or `$name::set`, with `$this` in scope and
//! the class as its scope, the declaration going on the property as a
//! method's goes on its class ([`PropertyHooks`]) and the body into
//! [`Unit::hooks`]. A `get` hook returns the property's type and a
//! `set` hook takes one parameter, `$value` of the property's type
//! where none is written, and returns `void`.
//!
//! [`Unit::hooks`]: php_unit::Unit::hooks

use php_syntax::tree::TypeExpr;
use php_unit::decl::{ClassDecl, ClassKind, HookKind, MethodDecl, PropertyHooks, Type, Visibility};
use php_unit::{HookBody, Opcode, OperandKind, Opline};

use super::walk::{self, Function, Hook};
use super::{Emit, FnFlags, Short};
use crate::CompileError;

/// The property a hook list is compiled for, what
/// `zend_compile_property_hooks` reads off `prop_info` and
/// `CG(active_class_entry)`: where its declaration stands in the unit,
/// its class's name and kind, its own name, the type as written, and
/// the flags a hook is held to.
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits of the property, each one fact
pub(super) struct Hooked<'a> {
    pub(super) class: u32,
    pub(super) property: u32,
    pub(super) class_name: &'a [u8],
    pub(super) kind: ClassKind,
    pub(super) name: &'a [u8],
    pub(super) ty: Option<TypeExpr>,
    /// The type `ty` compiled to, which a `set` hook's parameter is
    /// held to.
    pub(super) declared: Option<&'a Type>,
    /// The class the property is declared in where it is not yet among
    /// [`Emit::classes`], its members still compiling; `None` where it
    /// is, at `class`.
    pub(super) pending: Option<&'a ClassDecl>,
    pub(super) visibility: Visibility,
    pub(super) is_static: bool,
    pub(super) is_readonly: bool,
    pub(super) is_abstract: bool,
}

impl Hooked<'_> {
    /// `%s::$%s`, the class and the property as php's messages name them.
    fn member(&self) -> String {
        format!(
            "{}::${}",
            String::from_utf8_lossy(self.class_name),
            String::from_utf8_lossy(self.name)
        )
    }
}

impl Emit<'_> {
    /// `zend_compile_property_hooks` over the list: a readonly
    /// property and an empty list refused, then each hook held to its
    /// property and compiled in the order written; beside the hooks,
    /// whether the property is virtual, as `zend_property_is_virtual`
    /// decides it: no hook names `$this->name`, or the class is an
    /// interface.
    ///
    /// ```php
    /// class A { public readonly int $a { get => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Hooked properties cannot be readonly in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public $a {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Property hook list must not be empty in %s on line 2
    /// ```
    ///
    /// A hook runs on the read or the write it hooks, and reads its own
    /// property's backing value where it names it:
    ///
    /// ```php
    /// class P {
    ///     public string $name = "x" { get => strtoupper($this->name); set => trim($value); }
    ///     public $full { get { return __PROPERTY__ . ':' . __FUNCTION__ . ':' . __METHOD__; } }
    /// }
    /// $p = new P;
    /// echo $p->name, "\n";
    /// $p->name = "  y  ";
    /// echo $p->name, "|", $p->full, "\n";
    /// ```
    /// ```expect
    /// X
    /// Y|full:$full::get:P::$full::get
    /// ```
    pub(super) fn property_hooks(
        &mut self,
        hooked: &Hooked<'_>,
        hooks: &[Hook],
    ) -> Result<(PropertyHooks, bool), CompileError> {
        if hooked.is_readonly {
            return Err(self.compile_error("Hooked properties cannot be readonly"));
        }
        if hooks.is_empty() {
            return Err(self.compile_error("Property hook list must not be empty"));
        }
        let mut out = PropertyHooks {
            get: None,
            set: None,
            is_abstract: hooked.is_abstract,
        };
        // `zend_property_is_virtual`.
        let is_virtual =
            hooked.kind == ClassKind::Interface || !hooks.iter().any(|hook| hook.uses_property);
        let line = self.line;
        for hook in hooks.iter().cloned() {
            self.line = hook.line;
            let written = hook.name.clone();
            let (kind, method) = self.property_hook(hooked, hook)?;
            if kind == HookKind::Set {
                self.set_hook_verified(hooked, &method)?;
            }
            let slot = match kind {
                HookKind::Get => &mut out.get,
                HookKind::Set => &mut out.set,
            };
            if slot.is_some() {
                return Err(self.compile_error(format!(
                    "Cannot redeclare property hook \"{}\"",
                    String::from_utf8_lossy(&written)
                )));
            }
            *slot = Some(method);
        }
        self.line = line;
        Ok((out, is_virtual))
    }

    /// `zend_compile_property_hooks`' check of a `set` hook as soon as
    /// it has compiled, before the next hook: its parameter's type held
    /// to the property's against the classes bound while the file
    /// compiles ([`php_link::set_hook_variance`]), a refusal being the
    /// compile error it is in php, raised before anything runs, and a
    /// class it names not bound yet counted on the class as
    /// `num_hooked_prop_variance_checks` counts it, which keeps the
    /// class from the early bind and from the delayed one.
    ///
    /// ```php
    /// echo "not reached\n";
    /// class C extends P { public int $x { set(string $v) {} get => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type of parameter $v of hook C::$x::set must be compatible with property type in %s on line 3
    /// ```
    fn set_hook_verified(
        &mut self,
        hooked: &Hooked<'_>,
        set: &MethodDecl,
    ) -> Result<(), CompileError> {
        let Some(scope) = hooked
            .pending
            .or_else(|| self.classes.get(usize::try_from(hooked.class).ok()?))
        else {
            return Err(self.fatal("InternalError: a set hook of a class the unit has not"));
        };
        let lookup = |name: &[u8]| {
            self.bound_class(name).map(|decl| php_link::Held {
                decl,
                bodies: None,
                linked: true,
                record: None,
            })
        };
        let variance =
            php_link::set_hook_variance(scope, hooked.name, hooked.declared, set, &lookup);
        match variance {
            php_link::HookVariance::Refused(refusal) => {
                return Err(self.compile_error(refusal.to_string()));
            }
            php_link::HookVariance::Unresolved => {
                if let Some(active) = self.active_class.as_mut() {
                    active.hooked_variance_checks += 1;
                }
            }
            php_link::HookVariance::Holds => {}
        }
        Ok(())
    }

    /// One hook held to its property, php's arms of the loop in their
    /// order, then compiled.
    ///
    /// ```php
    /// class A { public static $a { get => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot declare hooks for static property in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { private $a { final get => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Property hook cannot be both final and private in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface I { public $a { get { return 1; } } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Abstract property hook cannot have body in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public $a { get; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-abstract property hook must have a body in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public $a { foo => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Unknown hook "foo" for property A::$a, expected "get" or "set" in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public $a { get() => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: get hook of property A::$a must not have a parameter list in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public $a { set($x, $y) {} } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: set hook of property A::$a must accept exactly one parameters in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public int $a { set(&$x) {} } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Parameter $x of set hook A::$a must not be pass-by-reference in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public int $a { set($x) {} } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type of parameter $x of hook A::$a::set must be compatible with property type in %s on line 2
    /// ```
    fn property_hook(
        &mut self,
        hooked: &Hooked<'_>,
        hook: Hook,
    ) -> Result<(HookKind, MethodDecl), CompileError> {
        let (kind, visibility, is_abstract) = self.hook_held(hooked, &hook)?;
        let shown = String::from_utf8_lossy(&hook.name).into_owned();
        let parameters = match kind {
            HookKind::Get => {
                if hook.parameters.is_some() {
                    return Err(self.compile_error(format!(
                        "get hook of property {} must not have a parameter list",
                        hooked.member()
                    )));
                }
                Vec::new()
            }
            HookKind::Set => self.set_hook_parameter(hooked, &shown, hook.parameters)?,
        };
        let name = format!("${}::{shown}", String::from_utf8_lossy(hooked.name)).into_bytes();
        let short = match &hook.body {
            walk::HookBody::Short(value) => Some(match kind {
                HookKind::Get => Short::Return(*value),
                HookKind::Set => Short::Assign(*value),
            }),
            walk::HookBody::None | walk::HookBody::Block(_) => None,
        };
        let function = Function {
            name: name.clone(),
            parameters,
            return_type: match kind {
                HookKind::Get => hooked.ty,
                HookKind::Set => None,
            },
            by_ref: hook.by_ref,
            body: match hook.body {
                walk::HookBody::Block(stmts) => stmts,
                walk::HookBody::None | walk::HookBody::Short(_) => Vec::new(),
            },
            attributes: hook.attributes,
            doc_comment: hook.doc_comment,
            line: hook.line,
            end_line: hook.end_line,
            generator: hook.generator,
            void_return: kind == HookKind::Set,
        };
        let flags = FnFlags {
            method: true,
            is_abstract,
            ..FnFlags::default()
        };
        let saved = self.begin_body(&name, true, flags);
        self.active_property = Some(hooked.name.to_vec());
        self.active_hook = Some(kind);
        let outcome =
            self.parameters_and_body(&function, &name, Some(hooked.class_name), &[], short);
        self.end_body(saved);
        let (declared, body) = outcome?;
        self.hooks.push(HookBody {
            class: hooked.class,
            property: hooked.property,
            kind,
            body,
        });
        Ok((
            kind,
            MethodDecl {
                function: declared,
                visibility,
                is_instance: true,
                is_abstract,
                is_final: hook.is_final,
                tentative_return: false,
            },
        ))
    }

    /// What `zend_compile_property_hooks` holds one hook to before its
    /// parameters, in php's order, and what that leaves: the kind its
    /// name spells, its visibility, public unless the property is
    /// private ("Non-private hooks are always public"), and whether it
    /// is abstract, as every interface property's is and an abstract
    /// property's is where no body is written.
    fn hook_held(
        &self,
        hooked: &Hooked<'_>,
        hook: &Hook,
    ) -> Result<(HookKind, Visibility, bool), CompileError> {
        let visibility = if hooked.visibility == Visibility::Private {
            Visibility::Private
        } else {
            Visibility::Public
        };
        if hooked.is_static {
            return Err(self.compile_error("Cannot declare hooks for static property"));
        }
        if hook.is_final && visibility == Visibility::Private {
            return Err(self.compile_error("Property hook cannot be both final and private"));
        }
        let has_body = hook.body.is_some();
        let is_abstract = hooked.kind == ClassKind::Interface || (hooked.is_abstract && !has_body);
        if is_abstract {
            if has_body {
                return Err(self.compile_error("Abstract property hook cannot have body"));
            }
            if visibility == Visibility::Private {
                return Err(self.compile_error("Property hook cannot be both abstract and private"));
            }
            if hook.is_final {
                return Err(self.compile_error("Property hook cannot be both abstract and final"));
            }
        } else if !has_body {
            return Err(self.compile_error("Non-abstract property hook must have a body"));
        }
        let kind = if hook.name.eq_ignore_ascii_case(b"get") {
            HookKind::Get
        } else if hook.name.eq_ignore_ascii_case(b"set") {
            HookKind::Set
        } else {
            return Err(self.compile_error(format!(
                "Unknown hook \"{}\" for property {}, expected \"get\" or \"set\"",
                String::from_utf8_lossy(&hook.name),
                hooked.member()
            )));
        };
        Ok((kind, visibility, is_abstract))
    }

    /// The one parameter of a `set` hook: the one written, held to
    /// `zend_compile_property_hooks`' rules, or `$value` of the
    /// property's type where none is written.
    fn set_hook_parameter(
        &self,
        hooked: &Hooked<'_>,
        shown: &str,
        written: Option<Vec<walk::Parameter>>,
    ) -> Result<Vec<walk::Parameter>, CompileError> {
        let Some(mut written) = written else {
            return Ok(vec![walk::Parameter {
                name: b"value".to_vec(),
                ty: hooked.ty,
                line: self.line,
                by_ref: false,
                variadic: false,
                default: None,
                promote: None,
                hooks: None,
                attributes: Vec::new(),
                doc_comment: None,
            }]);
        };
        if written.len() != 1 {
            return Err(self.compile_error(format!(
                "{shown} hook of property {} must accept exactly one parameters",
                hooked.member()
            )));
        }
        let Some(value) = written.pop() else {
            return Err(self.fatal("InternalError: a set hook's one parameter gone"));
        };
        let named = String::from_utf8_lossy(&value.name).into_owned();
        let refused = |what: &str| {
            self.compile_error(format!(
                "Parameter ${named} of {shown} hook {} must not {what}",
                hooked.member()
            ))
        };
        if value.by_ref {
            return Err(refused("be pass-by-reference"));
        }
        if value.variadic {
            return Err(refused("be variadic"));
        }
        if value.default.is_some() {
            return Err(refused("have a default value"));
        }
        if hooked.ty.is_some() != value.ty.is_some() {
            return Err(self.compile_error(format!(
                "Type of parameter ${named} of hook {}::set must be compatible with property type",
                hooked.member()
            )));
        }
        Ok(vec![value])
    }

    /// `zend_compile_parent_property_hook_call`
    /// (`Z/Zend/zend_compile.c:5096`): `parent::$name::get()` and
    /// `parent::$name::set($v)` inside the same hook of the same
    /// property, `INIT_PARENT_PROPERTY_HOOK_CALL` with the property's
    /// name in `op1` and the kind in `op2`, then the call as any whose
    /// callee nothing is known of. None where the call is no such
    /// pattern, a `(parent::$x)::get()` among them, which
    /// `zend_compile_static_call` compiles.
    ///
    /// ```php
    /// class P { public $x = 1 { get => $this->x * 10; } }
    /// class C extends P { public $x = 2 { get => parent::$x::get() + 1; } }
    /// var_dump((new C)->x);
    /// ```
    /// ```expect
    /// int(21)
    /// ```
    ///
    /// ```php
    /// class P { public $x; }
    /// class C extends P { public $x { get => parent::$y::get(); } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Must not use parent::$y::get() in a different property ($x) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class C { function f() { return parent::$x::get(); } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Must not use parent::$x::get() outside a property hook in %s on line 2
    /// ```
    pub(super) fn parent_hook_call(
        &mut self,
        scoped: php_syntax::tree::NodeId<php_syntax::tree::ScopedAccessExpr>,
        args: php_syntax::tree::NodeId<php_syntax::tree::ArgList>,
        line: u32,
    ) -> Result<Option<super::Where>, CompileError> {
        use php_syntax::tree::{Expr, ScopedSelector};
        let node = &self.parsed.tree[scoped];
        let Expr::ScopedAccessExpr(inner) = node.scope else {
            return Ok(None);
        };
        let Some(ScopedSelector::MemberName(method)) = node.member else {
            return Ok(None);
        };
        let Some(method) = self.parsed.tree[method].name else {
            return Ok(None);
        };
        let hook = self.text(method).to_vec();
        let kind = if hook.eq_ignore_ascii_case(b"get") {
            HookKind::Get
        } else if hook.eq_ignore_ascii_case(b"set") {
            HookKind::Set
        } else {
            return Ok(None);
        };
        let inner = &self.parsed.tree[inner];
        let Expr::NameRef(scope) = inner.scope else {
            return Ok(None);
        };
        if !self.text(scope).eq_ignore_ascii_case(b"parent") {
            return Ok(None);
        }
        // `class_ast->child[1]->kind != ZEND_AST_ZVAL`: `$p`, or
        // `${0}` and its kin, a name its parser wrote as one value, which
        // `zval_get_string` reads.
        let property = match inner.member {
            Some(ScopedSelector::SimpleVarExpr(var)) => match self.parsed.tree[var].var {
                Some(var) => self.variable_name(var),
                None => return Ok(None),
            },
            Some(ScopedSelector::DynamicVarExpr(var)) => {
                let Some(written) = self.parsed.tree[var]
                    .inner
                    .filter(|expr| self.is_written_value(*expr))
                else {
                    return Ok(None);
                };
                let Some(value) = self.constant_of(written)? else {
                    return Ok(None);
                };
                match self.stringified(value)?.and_then(php_heap::Zval::as_str) {
                    Some(text) => text.bytes().to_vec(),
                    None => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
        let shown = String::from_utf8_lossy(&property).into_owned();
        let hook_shown = String::from_utf8_lossy(&hook).into_owned();
        if self.active_class.is_none() {
            return Err(self.compile_error("Cannot use \"parent\" when no class scope is active"));
        }
        if self.parsed.tree[args]
            .args
            .iter()
            .any(|arg| matches!(arg, php_syntax::tree::Arg::VariadicPlaceholder(_)))
        {
            return Err(self.compile_error("Cannot create Closure for parent property hook call"));
        }
        let Some(active) = self.active_property.clone() else {
            return Err(self.compile_error(format!(
                "Must not use parent::${shown}::{hook_shown}() outside a property hook"
            )));
        };
        if active != property {
            return Err(self.compile_error(format!(
                "Must not use parent::${shown}::{hook_shown}() in a different property (${})",
                String::from_utf8_lossy(&active)
            )));
        }
        if let Some(active) = self.active_hook
            && active != kind
        {
            return Err(self.compile_error(format!(
                "Must not use parent::${shown}::{hook_shown}() in a different property hook ({})",
                active.name()
            )));
        }
        let arguments = self.arguments(args, line)?;
        let name = self.string(&property)?;
        let op1 = self.literal(name);
        let plan = super::args::Plan::of(&arguments, None);
        self.push(Opline {
            opcode: Opcode::InitParentPropertyHookCall,
            op1_type: OperandKind::Const,
            op1,
            op2: match kind {
                HookKind::Get => 0,
                HookKind::Set => 1,
            },
            extended_value: plan.count,
            ..Opline::default()
        });
        self.send_args(&plan, &arguments, None)?;
        Ok(Some(self.extended_call(
            Opcode::DoFcall,
            line,
            plan.extra_named,
        )))
    }

    /// The statement `zend_compile_property_hooks` makes of a `set`
    /// hook's short form, `$this->name = expr;`: `zend_compile_assign`'s
    /// `ZEND_AST_PROP` arm over `$this`, whose object is the frame's own
    /// and `UNUSED`, the name's constant and three cache slots taken
    /// before the value compiles, then `ASSIGN_OBJ` and its `OP_DATA`,
    /// and the value nothing reads freed as a statement's is.
    pub(super) fn assign_backing_value(
        &mut self,
        value: php_syntax::tree::Expr,
    ) -> Result<(), CompileError> {
        let Some(property) = self.active_property.clone() else {
            return Err(self.fatal("InternalError: a set hook's short form outside a hook"));
        };
        let line = self.line;
        let name = self.string(&property)?;
        let name_at = self.literal(name);
        let out = self.temporary();
        let slots = self.cache_slots(3);
        let value = self.expr(value)?;
        let (data_type, data) = self.operand(value);
        self.line = line;
        self.push(Opline {
            opcode: Opcode::AssignObj,
            op1_type: OperandKind::Unused,
            op2_type: OperandKind::Const,
            op2: name_at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slots,
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::OpData,
            op1_type: data_type,
            op1: data,
            ..Opline::default()
        });
        self.do_free(super::Where::Temporary(out));
        Ok(())
    }
}
