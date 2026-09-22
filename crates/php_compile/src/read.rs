//! The shapes a declaration reaches the emitter in (design §4.9,
//! #595), and the `attr` php's parser puts on a node.
//!
//! `php_syntax::Parsed` carries the tree and [`crate::emit`] walks it.
//! Every statement and every expression is read from its node where the
//! emitter compiles it; what is here is the shape a declaration takes
//! on the way, a class, a function, a closure and the members and
//! parameters under them, each named after the tree node it stands for
//! and the `zend_ast` kind that node is, with the expressions under it
//! still the tree's own nodes. Beside them are the enums php keeps in
//! `zend_ast.attr`, which the walk reads from the token that decided
//! it: which file construct an `include` is, which magic constant a
//! `__LINE__` is, which operator a `+` is, which type a cast is to,
//! which of the four step nodes a `++` is, and how a variable piece of
//! an interpolated string was written. The reader that once built a
//! shape for every node is gone; these go as the emitter learns to read
//! the declarations from the tree, and this file with the last.

use php_syntax::magic_const_name;
use php_unit::decl::{ClassKind, Type, Visibility};

/// Which of the four file constructs an `include` is: php's
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
    pub(crate) arguments: Vec<php_syntax::tree::Expr>,
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
    pub(crate) default: Option<php_syntax::tree::Expr>,
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
    pub(crate) default: Option<php_syntax::tree::Expr>,
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
    pub(crate) value: php_syntax::tree::Expr,
    pub(crate) doc_comment: Option<Vec<u8>>,
    pub(crate) line: u32,
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
