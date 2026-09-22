//! The shapes `emit`'s walk of `php_syntax`'s tree hands the emitter
//! (design §4.9, #595).
//!
//! `php_syntax::Parsed` carries the tree, [`crate::emit`] walks it, and
//! what is here is the shape each statement and expression reaches the
//! emitter in, one per node kind, each named after the tree node it
//! stands for and the `zend_ast` kind that node is. The reader that once
//! built them from the source is gone; each shape goes as the emitter
//! learns to read its node directly, and this file with the last.
//!
//! What the walk builds: the declarations, a `class` with no modifier
//! whose members are typed or untyped properties, constants and
//! methods, each declarator with its doc comment, an interface, a
//! function and a closure, each body the tree's own statement nodes,
//! which the emitter walks itself; and in an expression a `match`, an
//! array expression with no `...` and no `&`, a number, a string in
//! every spelling with `$a`, `{$a}` and `${a}` interpolated as php's
//! grammar takes them, `true`, `false`, `null`, a constant by name, a
//! call of a named function, `exit`, `die` and `clone`, a variable that
//! is a CV, an assignment to one, to an element, a property or a
//! class's property, plain, compound, coalescing or by reference, `++`
//! and `--` on a variable or a class's property, unary `-`, `+`, `!`
//! and `~`, `@` over an expression, a cast to a scalar type,
//! `instanceof`, `new`, a method call, a call of a class's method, a
//! call through a variable, `isset` and `empty` over a variable chain,
//! `include` and its kin, `throw`, a closure and an arrow function, the
//! conditional, `??`, `&&`, `||`, `and`, `or`, `xor`, and every binary
//! operator but `|>`. Anything else is a [`crate::CompileError`], so
//! what this cannot read is refused rather than silently compiled to
//! something else. The statements left first: the emitter reads every
//! statement node from the tree (#595).

use std::fmt;
use std::ops::Deref;

use php_syntax::magic_const_name;
use php_unit::decl::{ClassKind, Type, Visibility};

/// Which of the four file constructs an [`Expr::Include`] is: php's
/// `ZEND_INCLUDE`, `ZEND_INCLUDE_ONCE`, `ZEND_REQUIRE` and
/// `ZEND_REQUIRE_ONCE` (`Z/Zend/zend_compile.h:1076-1079`), one bit
/// each, which the parser puts in the node's `attr` and
/// `zend_compile_include_or_eval` copies into the opline's
/// `extended_value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncludeKind {
    Include,
    IncludeOnce,
    Require,
    RequireOnce,
}

impl IncludeKind {
    /// The bit php names the construct by.
    pub(crate) fn attr(self) -> u32 {
        match self {
            IncludeKind::Include => 1 << 1,
            IncludeKind::IncludeOnce => 1 << 2,
            IncludeKind::Require => 1 << 3,
            IncludeKind::RequireOnce => 1 << 4,
        }
    }
}

/// The nine magic constants, php's `T_LINE` through `T_PROPERTY_C`: the
/// lexer's tokens, matched whatever their case, so `__file__` is
/// `__FILE__`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Magic {
    Line,
    File,
    Dir,
    Function,
    Class,
    Trait,
    Method,
    Namespace,
    Property,
}

impl Magic {
    /// The magic constant this spelling is, by the token name
    /// `php_syntax` gives it; none for any other name, `\__FILE__`
    /// included, which php reads as a constant of that name.
    pub(crate) fn of(name: &[u8]) -> Option<Magic> {
        Some(match magic_const_name(name) {
            "T_LINE" => Magic::Line,
            "T_FILE" => Magic::File,
            "T_DIR" => Magic::Dir,
            "T_FUNC_C" => Magic::Function,
            "T_CLASS_C" => Magic::Class,
            "T_TRAIT_C" => Magic::Trait,
            "T_METHOD_C" => Magic::Method,
            "T_NS_C" => Magic::Namespace,
            "T_PROPERTY_C" => Magic::Property,
            _ => return None,
        })
    }
}

/// One element of an array expression, php's `ZEND_AST_ARRAY_ELEM`: the
/// value, and the key where one is written before `=>`.
#[derive(Debug, PartialEq)]
pub(crate) struct Element {
    pub(crate) key: Option<Child>,
    pub(crate) value: Child,
}

/// One class declaration, php's `zend_ast_decl` for a class: the name as
/// written, the doc comment taken before its `{` (`class_declaration_statement`
/// reduces `backup_doc_comment` after `implements_list`), and its
/// members as php's `ZEND_AST_PROP_GROUP` and `ZEND_AST_CLASS_CONST_GROUP`
/// hold them, one declarator each so the doc comment and the line are
/// the declarator's own.
///
/// `extends_from` is the parent of a class, `child[0]`; `implements_list`
/// is `child[1]`, and an interface's own `extends` list is the same
/// child, since `interface_declaration_statement` hands its
/// `interface_extends_list` to `zend_ast_create_decl` where a class
/// hands its `implements_list`.
#[derive(Debug, PartialEq)]
pub(crate) struct Class {
    pub(crate) name: Vec<u8>,
    pub(crate) kind: ClassKind,
    pub(crate) parent: Option<Vec<u8>>,
    pub(crate) interfaces: Vec<Vec<u8>>,
    pub(crate) attributes: Vec<Attr>,
    pub(crate) doc_comment: Option<Vec<u8>>,
    pub(crate) properties: Vec<Property>,
    pub(crate) constants: Vec<Constant>,
    pub(crate) methods: Vec<Method>,
    /// The line the name is on, `ce->info.user.line_start`, which is
    /// where php raises what `zend_compile_attributes` refuses and not
    /// where the attribute itself was written.
    pub(crate) line: u32,
    /// The line of the closing brace, `ce->info.user.line_end`.
    pub(crate) end_line: u32,
}

/// One attribute as written, php's `ZEND_AST_ATTRIBUTE`: the class name
/// before it is resolved, and the arguments of its list, still
/// expressions. `zend_compile_attributes` is what turns a group of
/// these into a declaration's, so a group is not a thing that outlives
/// the reader: `#[A] #[B]` and `#[A, B]` are the same two attributes.
#[derive(Debug, PartialEq)]
pub(crate) struct Attr {
    pub(crate) name: Vec<u8>,
    pub(crate) arguments: Vec<Child>,
    pub(crate) line: u32,
}

/// One function declaration, php's `zend_ast_decl` for a function or a
/// method: the name as written, the parameters, the return type where
/// one is written after the `)`, whether an `&` before the name returns
/// it by reference (`ZEND_ACC_RETURN_REFERENCE`), the body's statement
/// nodes, which the emitter walks from the tree, the doc comment taken
/// before the `function` keyword, and the lines of the keyword and of
/// the closing brace, `start_lineno` and `end_lineno`.
#[derive(Debug, PartialEq)]
pub(crate) struct Function {
    pub(crate) name: Vec<u8>,
    pub(crate) parameters: Vec<Parameter>,
    pub(crate) return_type: Option<Type>,
    pub(crate) by_ref: bool,
    pub(crate) body: Vec<php_syntax::tree::Stmt>,
    pub(crate) doc_comment: Option<Vec<u8>>,
    pub(crate) line: u32,
    pub(crate) end_line: u32,
}

/// One parameter, php's `ZEND_AST_PARAM`: the declared type where one is
/// written, `ZEND_PARAM_REF` for an `&` and `ZEND_PARAM_VARIADIC` for a
/// `...` before the variable, the name, the default, and the modifiers
/// that promote it to a property in a constructor.
#[derive(Debug, PartialEq)]
pub(crate) struct Parameter {
    pub(crate) name: Vec<u8>,
    pub(crate) ty: Option<Type>,
    pub(crate) by_ref: bool,
    pub(crate) variadic: bool,
    pub(crate) default: Option<Child>,
    pub(crate) promote: Option<Promoted>,
}

/// The modifiers that make a constructor's parameter a property,
/// php's `promotion_flags` in `zend_compile_params`: the visibility and
/// the `readonly` bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Promoted {
    pub(crate) visibility: Visibility,
    pub(crate) is_readonly: bool,
}

/// One method, php's `ZEND_AST_METHOD`: a function with the group's
/// modifiers beside it.
#[derive(Debug, PartialEq)]
pub(crate) struct Method {
    pub(crate) function: Function,
    pub(crate) visibility: Visibility,
    pub(crate) is_static: bool,
    pub(crate) is_final: bool,
}

/// One closure as written, php's `ZEND_AST_CLOSURE` and its
/// `ZEND_AST_ARROW_FUNC`: the function itself, whose `name` is empty
/// because `zend_begin_func_decl` builds `{closure:...}` from where it
/// stands; the `use` list as written, empty for an arrow
/// function, whose captures are found in its body
/// (`find_implicit_binds`); whether `static` stands before the
/// keyword, php's `ZEND_ACC_STATIC`, which keeps `$this` out of the
/// body; and for an arrow function the expression after `=>`, the one
/// `return` php makes its body of (`zend_compile_func_decl`), the
/// function's own statement list being empty.
#[derive(Debug, PartialEq)]
pub(crate) struct Closure {
    pub(crate) function: Function,
    pub(crate) uses: Vec<Use>,
    pub(crate) is_static: bool,
    pub(crate) arrow: Option<php_syntax::tree::Expr>,
}

/// One `use` variable of a closure, php's `lexical_var`: the name
/// without its `$`, whether an `&` stands before it
/// (`ZEND_BIND_REF`), and the line the name is written on, which
/// `zend_compile_closure_binding` sets `CG(zend_lineno)` to before the
/// `BIND_LEXICAL` it emits.
#[derive(Debug, PartialEq)]
pub(crate) struct Use {
    pub(crate) name: Vec<u8>,
    pub(crate) by_ref: bool,
    pub(crate) line: u32,
}

/// One property declarator, php's `ZEND_AST_PROP_ELEM` with the group's
/// modifiers and type beside it: `public $x`, `static $y = 1`,
/// `public private(set) int $z`. Its doc comment is what was pending
/// after its last token, php's `property: T_VARIABLE
/// backup_doc_comment | T_VARIABLE '=' expr backup_doc_comment`.
/// `set_visibility` is the group's `public(set)`, `protected(set)` or
/// `private(set)` as written, before `zend_declare_typed_property`
/// holds it to the read visibility.
#[derive(Debug, PartialEq)]
pub(crate) struct Property {
    pub(crate) name: Vec<u8>,
    pub(crate) visibility: Visibility,
    pub(crate) set_visibility: Option<Visibility>,
    pub(crate) ty: Option<Type>,
    pub(crate) is_static: bool,
    pub(crate) default: Option<Child>,
    pub(crate) doc_comment: Option<Vec<u8>>,
    pub(crate) line: u32,
}

/// One class constant declarator, php's `ZEND_AST_CONST_ELEM` with the
/// group's modifiers beside it: `const A = 1`, `final protected const B = 2`.
/// Its doc comment is taken after its value, `class_const_decl: identifier
/// '=' expr backup_doc_comment`.
#[derive(Debug, PartialEq)]
pub(crate) struct Constant {
    pub(crate) name: Vec<u8>,
    pub(crate) visibility: Visibility,
    pub(crate) is_final: bool,
    pub(crate) value: Child,
    pub(crate) doc_comment: Option<Vec<u8>>,
    pub(crate) line: u32,
}

/// One `cond, cond => body` or `default => body` of a `match`, php's
/// `ZEND_AST_MATCH_ARM`: the conditions, none for the default arm, and
/// the body. Each is compiled at the line its own node carries
/// ([`Child::line`]), so the arm carries no line of its own.
#[derive(Debug, PartialEq)]
pub(crate) struct Arm {
    pub(crate) conds: Option<Vec<Child>>,
    pub(crate) body: Child,
}

/// One piece of an interpolated string, php's `encaps_var` or
/// `T_ENCAPSED_AND_WHITESPACE`: the expression, and how it was
/// written, php's `attr` on the `encaps_var`, which
/// `zend_compile_encaps_list` reads for its two deprecations.
#[derive(Debug, PartialEq)]
pub(crate) struct Piece {
    pub(crate) expr: Child,
    pub(crate) written: Written,
}

/// How a variable piece of an interpolated string was written: `$a`,
/// `{$a}` and the plain bytes carry no `attr`; `${a}` and `${a[0]}` are
/// php's `ZEND_ENCAPS_VAR_DOLLAR_CURLY`, and `${expr}` its
/// `ZEND_ENCAPS_VAR_DOLLAR_CURLY_VAR_VAR`, each deprecated in its own
/// words.
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) enum Written {
    Plain,
    DollarCurly,
    DollarCurlyVarVar,
}

/// One expression.
#[derive(Debug, PartialEq)]
pub(crate) enum Expr {
    /// A node the emitter reads from the tree itself, no shape between
    /// (design §4.9): the kinds `emit` has learned to read directly,
    /// which the walk hands on as they stand. One kind at a time joins
    /// this arm until every kind has, and the shapes below go (#595).
    /// Today: `MagicConstExpr`.
    Node(php_syntax::tree::Expr),
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    /// `"a$b"`, php's `ZEND_AST_ENCAPS_LIST`: the written and the variable
    /// pieces of a double-quoted string in written order, at least one a
    /// variable, since a string with none is [`Expr::Str`].
    Encaps(Vec<Piece>),
    /// `[1, 'k' => 2]`, php's `ZEND_AST_ARRAY` over its elements; the
    /// `array(...)` spelling, `...` and `&` are not read.
    Array(Vec<Element>),
    /// A constant named as written, php's `ZEND_AST_CONST`.
    Constant(Vec<u8>),
    /// `-$a` and `+$a`, php's `ZEND_AST_UNARY_MINUS` and
    /// `ZEND_AST_UNARY_PLUS`.
    Unary {
        minus: bool,
        operand: Child,
    },
    /// `!$a`, php's `ZEND_AST_UNARY_OP` with `ZEND_BOOL_NOT` in `attr`,
    /// which `zend_compile_unary_op` emits and `zend_compile_unary_pm`
    /// does not. It has a level of its own, `%precedence '!'`, with `*`
    /// below it and `T_INSTANCEOF` above, and it nests: `!!$a` is two of
    /// them.
    Not(Child),
    /// `~$a`, php's `ZEND_AST_UNARY_OP` with `ZEND_BW_NOT` in `attr`,
    /// the same node as [`Expr::Not`] at another of php's levels:
    /// `%precedence '~'`, which unary `-`, the casts and `@` share, so
    /// it binds tighter than `instanceof` and looser than `**`, and it
    /// nests: `~~$a` is two of them.
    BwNot(Child),
    /// `@$a`, php's `ZEND_AST_SILENCE` over the expression the
    /// diagnostics of are held: at the level of unary `-` and the casts,
    /// so `@$a + 1` is php's `(@$a) + 1` and `@$a ** 2` php's
    /// `@($a ** 2)`; and an assignment after it is inside it, `@$a = 1`
    /// being php's `@($a = 1)`, as the grammar shifts the `=`.
    Silence(Child),
    /// `(int)$a`, php's `ZEND_AST_CAST` with the type in `attr`.
    Cast {
        to: CastTo,
        operand: Child,
    },
    /// `$a + $b`, php's `ZEND_AST_BINARY_OP`.
    Binary {
        op: BinaryOp,
        left: Child,
        right: Child,
    },
    Call {
        name: Vec<u8>,
        arguments: Vec<Child>,
    },
    /// `$a`, php's `ZEND_AST_VAR` over a name written as is, which
    /// `zend_try_compile_cv` makes a CV: not `$this`, which is [`Expr::This`],
    /// and not `$_GET` or its kin, which php fetches by other oplines and
    /// this refuses.
    Var(Vec<u8>),
    /// `$this`, php's `ZEND_AST_VAR` over the one name `is_this_fetch`
    /// picks out, fetched by `FETCH_THIS` and never a CV.
    This,
    /// `${expr}` inside a double-quoted string, php's `ZEND_AST_VAR`
    /// over an expression rather than a name: the variable the
    /// expression's value names, which `zend_compile_simple_var` fetches
    /// by `FETCH_R` unless php's parser had the expression as one value,
    /// `ZEND_AST_ZVAL`, and the value names a CV. `$$a` and `${expr}`
    /// outside a string are not read yet.
    VarVar(Child),
    /// `$o->p`, php's `ZEND_AST_PROP` over any expression and a property
    /// named as written; `$o?->p` and `$o->$p` are not read.
    Prop {
        object: Child,
        name: Vec<u8>,
    },
    /// `$a[k]` and `$a[]`, php's `ZEND_AST_DIM` over a container and a
    /// dimension, the dimension `None` for the append php's grammar
    /// writes as an empty pair of brackets and reads as a write alone
    /// (`optional_expr`, `Cannot use [] for reading`). The chain nests
    /// through `container`, so `$a[1][2]` is the dimension `2` over the
    /// dimension `1` over `$a`.
    Dim {
        container: Child,
        index: Option<Child>,
    },
    /// `$a = expr`, php's `ZEND_AST_ASSIGN` to a variable.
    Assign {
        var: Vec<u8>,
        value: Child,
    },
    /// `$a op= expr`, php's `ZEND_AST_ASSIGN_OP` over a `ZEND_AST_VAR`
    /// with the operator in `attr`, which `zend_compile_compound_assign`
    /// emits as one `ASSIGN_OP`. The twelve spellings are one node here
    /// as they are one node in php, the operator a field beside the
    /// variable and the value.
    AssignOp {
        var: Vec<u8>,
        op: BinaryOp,
        value: Child,
    },
    /// `$a[k] = expr` and `$a[] = expr`, php's `ZEND_AST_ASSIGN` to a
    /// `ZEND_AST_DIM`: the dimension written, and the value.
    AssignDim {
        target: Child,
        value: Child,
    },
    /// `$o->p = expr`, php's `ZEND_AST_ASSIGN` to a `ZEND_AST_PROP`:
    /// the property's receiver, its name as written, and the value.
    AssignProp {
        object: Child,
        name: Vec<u8>,
        value: Child,
    },
    /// `C::$p = expr`, php's `ZEND_AST_ASSIGN` to a
    /// `ZEND_AST_STATIC_PROP`: the class and the property both named as
    /// written, and the value.
    AssignStaticProp {
        class: Vec<u8>,
        name: Vec<u8>,
        value: Child,
    },
    /// `$b = &$a`, `$a[] = &$x` and `$b = &f()`, php's
    /// `ZEND_AST_ASSIGN_REF`: the target, a variable or a dimension, and
    /// the source, a variable or a call (`zend_is_call`, which php marks
    /// `ZEND_RETURNS_FUNCTION`). php's grammar puts a `variable` after
    /// the `&`; a property or a dimension there is not read yet.
    AssignRef {
        target: Child,
        source: Child,
    },
    /// `++$a`, php's `ZEND_AST_PRE_INC`, whose grammar puts a variable
    /// and nothing else after the `++`.
    PreInc(Vec<u8>),
    /// `$a++`, php's `ZEND_AST_POST_INC`.
    PostInc(Vec<u8>),
    /// `--$a`, php's `ZEND_AST_PRE_DEC`, the same grammar as `++$a`.
    PreDec(Vec<u8>),
    /// `$a--`, php's `ZEND_AST_POST_DEC`.
    PostDec(Vec<u8>),
    /// `match (expr) { arms }`, php's `ZEND_AST_MATCH` over a
    /// `ZEND_AST_MATCH_ARM_LIST`. The `MATCH` and the chain over it are
    /// emitted at the subject's line, which the subject carries.
    Match {
        expr: Child,
        arms: Vec<Arm>,
    },
    /// `new C(args)`, php's `ZEND_AST_NEW` over a class named as
    /// written; `new $c`, `new static` and an anonymous class are not
    /// read.
    New {
        class: Vec<u8>,
        arguments: Vec<Child>,
    },
    /// `C::A`, php's `ZEND_AST_CLASS_CONST` over a class and a constant
    /// both named as written; `C::class` is not read.
    ClassConst {
        class: Vec<u8>,
        name: Vec<u8>,
    },
    /// `C::$p`, php's `ZEND_AST_STATIC_PROP` over a class named as
    /// written and a `simple_variable` that is one `T_VARIABLE`, the
    /// property's name without its `$`; `C::$$p` and `C::${expr}` name
    /// the property as the program serves and are not read.
    StaticProp {
        class: Vec<u8>,
        name: Vec<u8>,
    },
    /// `C::m(args)`, php's `ZEND_AST_STATIC_CALL` over a class and a
    /// method both named as written.
    StaticCall {
        class: Vec<u8>,
        method: Vec<u8>,
        arguments: Vec<Child>,
    },
    /// `$o->m(args)`, php's `ZEND_AST_METHOD_CALL` over any expression
    /// and a method named as written.
    MethodCall {
        object: Child,
        method: Vec<u8>,
        arguments: Vec<Child>,
    },
    /// `$o instanceof C`, php's `ZEND_AST_INSTANCEOF` over a class named
    /// as written.
    Instanceof {
        expr: Child,
        class: Vec<u8>,
    },
    /// `function (…) use (…) {…}`, `fn (…) => e` and either with
    /// `static` before it, php's `ZEND_AST_CLOSURE` and
    /// `ZEND_AST_ARROW_FUNC`.
    Closure(Box<Closure>),
    /// `$f(args)`, php's `ZEND_AST_CALL` over a variable rather than a
    /// name, which `zend_compile_dynamic_call` compiles: the callee as
    /// an expression and the arguments.
    CallVar {
        callee: Child,
        arguments: Vec<Child>,
    },
    /// `throw expr`, php's `ZEND_AST_THROW`: an expression since 8.0,
    /// which `throw expr;` is the statement of, so the grammar's
    /// `expr: T_THROW expr` is read wherever a full expression stands.
    Throw(Child),
    /// `include expr` and its three kin, php's `ZEND_AST_INCLUDE_OR_EVAL`
    /// with the construct in `attr`: the operand is a whole expression,
    /// since the four rules are `T_INCLUDE expr` and their tokens stand
    /// on the grammar's lowest `%precedence` line
    /// (`Z/Zend/zend_language_parser.y:56`, `:1708`), so
    /// `include $a . 'b' or $c` is php's `include ($a . 'b' or $c)`.
    /// `eval(...)`, the fifth construct of that node, is refused.
    Include {
        kind: IncludeKind,
        path: Child,
    },
    /// `isset($a)` and `empty($a)`, php's `ZEND_AST_ISSET` and
    /// `ZEND_AST_EMPTY` over one variable node, and whether the word was
    /// `empty`. The node is a `ZEND_AST_VAR` that `zend_try_compile_cv`
    /// makes a CV, or a `ZEND_AST_DIM` chain over one, which php
    /// compiles under `BP_VAR_IS`. `isset($this)`, `isset($a, $b)`, a
    /// property and `empty` over what is no variable are not read.
    Isset {
        var: Child,
        empty: bool,
    },
    /// `a ? b : c` and `a ?: c`, php's `ZEND_AST_CONDITIONAL` over three
    /// children, the middle absent for the short form as php leaves
    /// `child[1]` NULL there. `parenthesised` is php's
    /// `ZEND_PARENTHESIZED_CONDITIONAL`, which the grammar puts in `attr`
    /// for a conditional a `(` and a `)` stand around and
    /// `zend_compile_conditional` reads to tell an unparenthesised nested
    /// one, php's compile error, from the grouping that is written out.
    Ternary {
        cond: Child,
        then: Option<Child>,
        otherwise: Child,
        parenthesised: bool,
    },
    /// `a ?? b`, php's `ZEND_AST_COALESCE` over two children, read at a
    /// level of its own, right to left, above the conditional and below
    /// `||` (`%right T_COALESCE`). The left side is compiled under
    /// `BP_VAR_IS`, the silent read, so an undefined variable on it
    /// raises nothing. `??=`, php's `T_COALESCE_EQUAL`, is the
    /// assignment form of the same shape and is not read.
    Coalesce {
        left: Child,
        right: Child,
    },
    /// `a && b` and `a and b`, php's `ZEND_AST_AND` over two children,
    /// which `zend_compile_short_circuiting` emits: the right side is
    /// reached only where the left is true. The two spellings are one
    /// node at two precedence levels, `%left T_BOOLEAN_AND` above `??`
    /// and `%left T_LOGICAL_AND` below `=`.
    And {
        left: Child,
        right: Child,
    },
    /// `a || b` and `a or b`, php's `ZEND_AST_OR`, the same node the
    /// other way round: the right side is reached only where the left is
    /// false. `%left T_BOOLEAN_OR` and `%left T_LOGICAL_OR` are its two
    /// levels.
    Or {
        left: Child,
        right: Child,
    },
    /// `C::$p op= expr`, php's `ZEND_AST_ASSIGN_OP` over a
    /// `ZEND_AST_STATIC_PROP` with the operator in `attr`, which
    /// `zend_compile_compound_assign` turns the fetch's own opline into
    /// `ASSIGN_STATIC_PROP_OP` for. A variable is [`Expr::AssignOp`];
    /// the other two targets php's switch has, a dimension and a
    /// property, are not read yet.
    AssignStaticPropOp {
        class: Vec<u8>,
        name: Vec<u8>,
        op: BinaryOp,
        value: Child,
    },
    /// `$a[k] op= expr` and `$a[] op= expr`, php's `ZEND_AST_ASSIGN_OP`
    /// over a `ZEND_AST_DIM` with the operator in `attr`, which
    /// `zend_compile_compound_assign` turns the chain's last delayed
    /// opline into `ASSIGN_DIM_OP` for. The target is the dimension
    /// written, as [`Expr::AssignDim`] carries it.
    AssignDimOp {
        target: Child,
        op: BinaryOp,
        value: Child,
    },
    /// `$o->p op= expr`, php's `ZEND_AST_ASSIGN_OP` over a
    /// `ZEND_AST_PROP` with the operator in `attr`, which
    /// `zend_compile_compound_assign` turns the delayed fetch into
    /// `ASSIGN_OBJ_OP` for, the cache slot moved onto the `OP_DATA`.
    AssignObjOp {
        object: Child,
        name: Vec<u8>,
        op: BinaryOp,
        value: Child,
    },
    /// `++C::$p`, `--C::$p`, `C::$p++` and `C::$p--`, php's four
    /// `ZEND_AST_{PRE,POST}_{INC,DEC}` over a `ZEND_AST_STATIC_PROP`,
    /// which `zend_compile_pre_incdec` and `zend_compile_post_incdec`
    /// turn the fetch's own opline into one of four opcodes for. The
    /// step is a field here where php keeps it in the node's kind,
    /// since the four differ in nothing else.
    StaticPropStep {
        class: Vec<u8>,
        name: Vec<u8>,
        step: Step,
    },
    /// `$a ??= b`, php's `ZEND_AST_ASSIGN_COALESCE` over a target and a
    /// value: one token at the assignment level, read right to left,
    /// whose target is evaluated once and assigned only where what it
    /// holds is null. The target is a `ZEND_AST_VAR`, which is the CV
    /// under both fetch types; a dimension or a property waits on the
    /// `BP_VAR_IS` chain and is refused where it stands.
    AssignCoalesce {
        var: Vec<u8>,
        value: Child,
    },
}

/// One child of an expression, php's `zend_ast.child[i]`: the expression
/// itself behind a pointer, dropped as `zend_ast_destroy` frees it. That
/// walk recurses on every child but the first, and takes the first in a
/// loop (`tail_call`, `Z/Zend/zend_ast.c:1367-1379`), so a left-nested
/// chain, `1+1+1+…` as the grammar groups it, is freed in constant stack
/// however long it is. A derived drop would recurse on both, and the
/// chain the compiler refuses at its stack limit ([`StackLimit`]) would
/// then take the thread down on the way out instead.
///
/// Beside the expression it carries the line php's `zend_ast.lineno`
/// carries: the smallest line of the node's own children, and the line
/// the reader stands on for a node with none (`zend_ast_create_from_va_list`,
/// `Z/Zend/zend_ast.c:759-779`). `zend_compile_expr` sets
/// `CG(zend_lineno)` from it before compiling the node, so an opline
/// carries the line of the expression it came from and not the line its
/// statement began on.
#[derive(PartialEq)]
pub(crate) struct Child {
    expr: Box<Expr>,
    line: u32,
}

impl Child {
    /// One node at the line php's `zend_ast_create` would give it.
    pub(crate) fn at(line: u32, expr: Expr) -> Child {
        Child {
            expr: Box::new(expr),
            line,
        }
    }

    /// The line `zend_compile_expr` sets `CG(zend_lineno)` to before
    /// this node's oplines are emitted.
    pub(crate) fn line(&self) -> u32 {
        self.line
    }

    /// The expression out, `Null` left in its place for the drop to find.
    pub(crate) fn take(mut self) -> Expr {
        std::mem::replace(&mut self.expr, Expr::Null)
    }
}

impl Deref for Child {
    type Target = Expr;

    fn deref(&self) -> &Expr {
        &self.expr
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Expr {
        &mut self.expr
    }
}

impl fmt::Debug for Child {
    /// The expression and the line it carries, since two nodes that
    /// differ only in their line are one tree here and two to
    /// [`PartialEq`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.expr.fmt(f)?;
        write!(f, "@{}", self.line)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let mut first = std::mem::replace(&mut *self.expr, Expr::Null);
        loop {
            first = match first {
                Expr::Unary { operand, .. }
                | Expr::Cast { operand, .. }
                | Expr::Not(operand)
                | Expr::BwNot(operand)
                | Expr::Silence(operand) => operand.take(),
                Expr::Binary { left, right, .. }
                | Expr::Coalesce { left, right }
                | Expr::And { left, right }
                | Expr::Or { left, right } => {
                    drop(right);
                    left.take()
                }
                Expr::Assign { value, .. }
                | Expr::AssignOp { value, .. }
                | Expr::AssignCoalesce { value, .. } => value.take(),
                Expr::Isset { var, .. } => var.take(),
                Expr::AssignProp { object, value, .. }
                | Expr::AssignObjOp { object, value, .. } => {
                    drop(value);
                    object.take()
                }
                Expr::Dim { container, index } => {
                    drop(index);
                    container.take()
                }
                Expr::AssignDim { target, value } | Expr::AssignDimOp { target, value, .. } => {
                    drop(value);
                    target.take()
                }
                Expr::AssignRef { target, source } => {
                    drop(source);
                    target.take()
                }
                Expr::Ternary {
                    cond,
                    then,
                    otherwise,
                    ..
                } => {
                    drop(then);
                    drop(otherwise);
                    cond.take()
                }
                _ => return,
            };
        }
    }
}

/// Which of php's four step nodes stands over a target: whether the
/// value the step leaves is the one before it (`POST`) or after it
/// (`PRE`), and whether it counts up or down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// `++$x`, php's `ZEND_AST_PRE_INC`
    PreInc,
    /// `--$x`, php's `ZEND_AST_PRE_DEC`
    PreDec,
    /// `$x++`, php's `ZEND_AST_POST_INC`
    PostInc,
    /// `$x--`, php's `ZEND_AST_POST_DEC`
    PostDec,
}

/// The binary operators this reads, each the opcode php's
/// `zend_compile_binary_op` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    /// `ZEND_ADD`
    Add,
    /// `ZEND_SUB`
    Sub,
    /// `ZEND_MUL`
    Mul,
    /// `ZEND_DIV`
    Div,
    /// `ZEND_MOD`
    Mod,
    /// `ZEND_POW`
    Pow,
    /// `ZEND_SL`
    Sl,
    /// `ZEND_SR`
    Sr,
    /// `ZEND_CONCAT`
    Concat,
    /// `ZEND_IS_SMALLER`
    IsSmaller,
    /// `ZEND_IS_SMALLER_OR_EQUAL`
    IsSmallerOrEqual,
    /// `>`, php's `ZEND_AST_GREATER`, which `zend_compile_greater` emits
    /// as `ZEND_IS_SMALLER` with the operands swapped.
    Greater,
    /// `>=`, php's `ZEND_AST_GREATER_EQUAL`, `ZEND_IS_SMALLER_OR_EQUAL`
    /// the same way.
    GreaterOrEqual,
    /// `ZEND_IS_EQUAL`
    IsEqual,
    /// `ZEND_IS_NOT_EQUAL`, which `<>` is as well.
    IsNotEqual,
    /// `ZEND_IS_IDENTICAL`
    IsIdentical,
    /// `ZEND_IS_NOT_IDENTICAL`
    IsNotIdentical,
    /// `ZEND_SPACESHIP`
    Spaceship,
    /// `ZEND_BW_OR`
    BwOr,
    /// `ZEND_BW_XOR`
    BwXor,
    /// `ZEND_BW_AND`
    BwAnd,
    /// `ZEND_BOOL_XOR`, which `xor` is: the one word spelling
    /// `zend_compile_binary_op` serves, with no jump in it, since both
    /// sides are read whatever either answers.
    BoolXor,
}

impl BinaryOp {
    /// The operator as php's grammar spells it, what a constant
    /// expression's `Binary` node names it by (design §4.7).
    pub(crate) fn spelling(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Pow => "**",
            BinaryOp::Sl => "<<",
            BinaryOp::Sr => ">>",
            BinaryOp::Concat => ".",
            BinaryOp::IsSmaller => "<",
            BinaryOp::IsSmallerOrEqual => "<=",
            BinaryOp::Greater => ">",
            BinaryOp::GreaterOrEqual => ">=",
            BinaryOp::IsEqual => "==",
            BinaryOp::IsNotEqual => "!=",
            BinaryOp::IsIdentical => "===",
            BinaryOp::IsNotIdentical => "!==",
            BinaryOp::Spaceship => "<=>",
            BinaryOp::BwOr => "|",
            BinaryOp::BwXor => "^",
            BinaryOp::BwAnd => "&",
            BinaryOp::BoolXor => "xor",
        }
    }
}

/// The type a cast is to, php's `attr` on `ZEND_AST_CAST`: the scanner's
/// `T_INT_CAST`, `T_DOUBLE_CAST`, `T_STRING_CAST`, `T_ARRAY_CAST`,
/// `T_OBJECT_CAST` and `T_BOOL_CAST`, each scalar one with the canonical
/// spelling and the one 8.5 deprecates (`zend_language_scanner.l` at the
/// pin). `(unset)` is refused as the compile error `zend_compile_cast`
/// raises for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastTo {
    /// `(int)` and `(integer)`: `IS_LONG`.
    Long,
    /// `(float)` and `(double)`: `IS_DOUBLE`.
    Double,
    /// `(string)` and `(binary)`: `IS_STRING`.
    Str,
    /// `(array)`: `IS_ARRAY`.
    Array,
    /// `(object)`: `IS_OBJECT`.
    Object,
    /// `(bool)` and `(boolean)`: `_IS_BOOL`, which php compiles to
    /// `ZEND_BOOL` and never to `ZEND_CAST`.
    Bool,
}

impl CastTo {
    /// php's type constant, `ZEND_CAST`'s `extended_value`.
    pub(crate) fn type_code(self) -> u32 {
        match self {
            CastTo::Long => 4,
            CastTo::Double => 5,
            CastTo::Str => 6,
            CastTo::Array => 7,
            CastTo::Object => 8,
            // `_IS_BOOL` is 18 and reaches no opline: `ZEND_BOOL` carries
            // nothing in `extended_value`.
            CastTo::Bool => 0,
        }
    }
}

/// `zend_is_reserved_class_name`: the names `reserved_class_names` lists
/// in `zend_compile.c`, which no class may take, whatever the case.
pub(crate) fn is_reserved_class_name(name: &[u8]) -> bool {
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
    )
}

/// `zend_is_auto_global`: the names `php_startup_auto_globals` and
/// `zend_startup` register, which no CV holds.
pub(crate) fn is_auto_global(name: &[u8]) -> bool {
    matches!(
        name,
        b"GLOBALS"
            | b"_GET"
            | b"_POST"
            | b"_COOKIE"
            | b"_SERVER"
            | b"_ENV"
            | b"_REQUEST"
            | b"_FILES"
    )
}
