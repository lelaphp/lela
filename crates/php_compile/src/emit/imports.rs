//! What php keeps per compiled file, `CG(file_context)`: the import
//! tables and the names read through them, `FC(imports)`,
//! `FC(imports_function)` and `FC(imports_const)`, which
//! `zend_compile_use` and `zend_compile_group_use` fill
//! (`Z/Zend/zend_compile.c:9574-9667`), and `FC(seen_symbols)`, which a
//! declaration of the file registers its name in so an import after it
//! can refuse the name. `zend_resolve_class_name` and
//! `zend_resolve_non_class_name` read the tables, so every name is
//! resolved once, here, while the file compiles; the unit carries the
//! resolved name and nothing reads an import after it. A `namespace`
//! statement empties all four (`zend_reset_import_tables`), and so does
//! the end of a bracketed one (`zend_end_namespace`), so each namespace
//! block reads its own imports. Beside them, the namespace the file
//! stands in and how its `namespace` statements are written,
//! `FC(current_namespace)`, `FC(in_namespace)` and
//! `FC(has_bracketed_namespaces)`, which `zend_compile_namespace` holds
//! each new one to.

use std::collections::HashMap;

use php_syntax::SyntaxKind;
use php_syntax::tree::{self, NodeId};
use php_unit::Level;
use php_unit::decl::NameKind;

use super::Emit;
use super::types::is_reserved_class_name;
use super::walk::Namespace;
use crate::CompileError;

/// How the file's `namespace` statements are written so far, php's
/// `FC(has_bracketed_namespaces)` and `FC(in_namespace)` as the one
/// value `zend_compile_namespace` and `zend_verify_namespace` read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum Namespacing {
    /// No `namespace` statement yet: the global namespace.
    #[default]
    Global,
    /// `namespace N;`, the form every later one must take.
    Unbracketed,
    /// `namespace N { … }` or `namespace { … }`, the form every later
    /// one must take; `open` while the braces of one are compiling.
    Bracketed { open: bool },
}

/// What a `use` imports, php's `ZEND_SYMBOL_CLASS`, `ZEND_SYMBOL_FUNCTION`
/// and `ZEND_SYMBOL_CONST`: which table the name goes in, whether the
/// alias is looked up whatever its case, and the word the refusal names
/// the import with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Symbol {
    Class,
    Function,
    Const,
}

impl Symbol {
    /// The bit `zend_register_seen_symbol` sets for the kind.
    const fn bit(self) -> u8 {
        match self {
            Symbol::Class => 1,
            Symbol::Function => 2,
            Symbol::Const => 4,
        }
    }

    /// `zend_get_use_type_str`: the word after `use` in the refusal.
    const fn use_type(self) -> &'static str {
        match self {
            Symbol::Class => "",
            Symbol::Function => " function",
            Symbol::Const => " const",
        }
    }

    /// The kind a `function` or `const` keyword before the names says,
    /// and a class where none stands.
    fn of(kind: Option<SyntaxKind>) -> Symbol {
        match kind {
            Some(SyntaxKind::FUNCTION_KW) => Symbol::Function,
            Some(SyntaxKind::CONST_KW) => Symbol::Const,
            _ => Symbol::Class,
        }
    }
}

/// The names `CG(file_context)` keeps: the three import tables, each from the
/// alias as it is looked up (lowered but for a constant's) to the name
/// it imports, and the symbols the file has declared so far in the
/// current namespace, by the key each declaration registered, with the
/// kinds as bits.
#[derive(Default)]
pub(super) struct Imports {
    classes: ByName<Vec<u8>>,
    functions: ByName<Vec<u8>>,
    consts: ByName<Vec<u8>>,
    seen: ByName<u8>,
}

/// One of the file's tables keyed by a name, hashed as every table by
/// name is ([`php_link::NameHasher`]).
type ByName<V> = HashMap<Vec<u8>, V, php_link::NameHasher>;

impl Imports {
    fn table(&mut self, kind: Symbol) -> &mut ByName<Vec<u8>> {
        match kind {
            Symbol::Class => &mut self.classes,
            Symbol::Function => &mut self.functions,
            Symbol::Const => &mut self.consts,
        }
    }

    /// `zend_reset_import_tables`, which a `namespace` statement runs.
    pub(super) fn reset(&mut self) {
        *self = Imports::default();
    }

    /// `zend_register_seen_symbol`.
    fn see(&mut self, key: Vec<u8>, kind: Symbol) {
        *self.seen.entry(key).or_default() |= kind.bit();
    }

    /// `zend_have_seen_symbol`.
    fn have_seen(&self, key: &[u8], kind: Symbol) -> bool {
        self.seen
            .get(key)
            .is_some_and(|bits| bits & kind.bit() != 0)
    }

    /// The class name `alias` imports, whatever its case.
    fn class(&self, alias: &[u8]) -> Option<&[u8]> {
        self.classes
            .get(&alias.to_ascii_lowercase())
            .map(Vec::as_slice)
    }

    /// The function name `alias` imports, whatever its case.
    fn function(&self, alias: &[u8]) -> Option<&[u8]> {
        self.functions
            .get(&alias.to_ascii_lowercase())
            .map(Vec::as_slice)
    }

    /// The constant name `alias` imports, the case as written.
    fn constant(&self, alias: &[u8]) -> Option<&[u8]> {
        self.consts.get(alias).map(Vec::as_slice)
    }
}

/// How a name was written, php's `ZEND_NAME_FQ`, `ZEND_NAME_RELATIVE` and
/// `ZEND_NAME_NOT_FQ`, with the name as the scanner hands it over: the
/// leading `\` and the `namespace\` dropped.
enum Written<'n> {
    FullyQualified(&'n [u8]),
    Relative(&'n [u8]),
    NotQualified(&'n [u8]),
}

impl Written<'_> {
    fn of(name: &[u8]) -> Written<'_> {
        if let Some(rest) = name.strip_prefix(b"\\") {
            return Written::FullyQualified(rest);
        }
        if name.len() > 10 && name[..10].eq_ignore_ascii_case(b"namespace\\") {
            return Written::Relative(&name[10..]);
        }
        Written::NotQualified(name)
    }
}

/// `zend_concat_names`.
fn concat(first: &[u8], second: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(first.len() + 1 + second.len());
    name.extend_from_slice(first);
    name.push(b'\\');
    name.extend_from_slice(second);
    name
}

impl Emit<'_> {
    /// `zend_compile_namespace` (`Z/Zend/zend_compile.c:9741`): the form
    /// held to the file's earlier `namespace` statements, a first one
    /// held to `zend_is_first_statement`, the name to `namespace`, then
    /// the name is the namespace of what follows and the import tables
    /// start empty. No opline. A bracketed one compiles its braces as
    /// top statements, which is where a class or a function binds at
    /// compile time, and `zend_end_namespace` after them leaves the file
    /// in no namespace with the imports empty again, so each block reads
    /// its own `use` statements and `namespace { … }` is the global one.
    ///
    /// ```php
    /// namespace A {
    ///     use ArrayObject as AO;
    ///     function f() { return __FUNCTION__; }
    ///     echo f(), ' ', AO::class, "\n";
    /// }
    /// namespace {
    ///     echo __NAMESPACE__, '|', A\f(), ' ', AO::class, "\n";
    /// }
    /// ```
    /// ```expect
    /// A\f ArrayObject
    /// |A\f AO
    /// ```
    ///
    /// The refusals are php's `E_COMPILE_ERROR`s, in the order
    /// `zend_compile_namespace` makes them:
    ///
    /// ```php
    /// namespace A;
    /// namespace B {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot mix bracketed namespace declarations with unbracketed namespace declarations in %s on line 3
    /// ```
    ///
    /// ```php
    /// namespace A {
    ///     namespace B {}
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Namespace declarations cannot be nested in %s on line 3
    /// ```
    ///
    /// ```php
    /// echo 1;
    /// namespace B {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Namespace declaration statement has to be the very first statement or after any declare call in the script in %s on line 3
    /// ```
    ///
    /// `namespace` is no namespace's name, whatever its case, since
    /// `namespace\X` is the relative name:
    ///
    /// ```php
    /// namespace NAMEspace;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use 'NAMEspace' as namespace name in %s on line 2
    /// ```
    ///
    /// The braces are not the file's own statement list, which is where
    /// `zend_is_first_statement` looks for a `strict_types`:
    ///
    /// ```php
    /// namespace A {
    ///     declare(strict_types=1);
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: strict_types declaration must be the very first statement in the script in %s on line 3
    /// ```
    pub(super) fn namespace(&mut self, id: NodeId<tree::NamespaceDef>) -> Result<(), CompileError> {
        let declared = self.walk_namespace(id)?;
        let bracketed = matches!(declared, Namespace::Bracketed { .. });
        match (self.namespacing, bracketed) {
            (Namespacing::Unbracketed, true) | (Namespacing::Bracketed { .. }, false) => {
                return Err(self.compile_error(
                    "Cannot mix bracketed namespace declarations with unbracketed namespace declarations",
                ));
            }
            (Namespacing::Bracketed { open: true }, true) => {
                return Err(self.compile_error("Namespace declarations cannot be nested"));
            }
            _ => {}
        }
        // A first one of either form: `zend_is_first_statement` with
        // `allow_nop`, which steps over a `declare` and an empty
        // statement and over nothing else.
        if self.namespacing == Namespacing::Global && self.statements_seen > 0 {
            return Err(self.compile_error(
                "Namespace declaration statement has to be the very first statement or after any declare call in the script",
            ));
        }
        let (name, body) = match declared {
            Namespace::Unbracketed(name) => (Some(name), None),
            Namespace::Bracketed { name, body } => (name, Some(body)),
        };
        if let Some(name) = &name
            && name.eq_ignore_ascii_case(b"namespace")
        {
            return Err(self.compile_error(format!(
                "Cannot use '{}' as namespace name",
                String::from_utf8_lossy(name)
            )));
        }
        self.namespace = name;
        self.imports.reset();
        // php's `zend_is_first_statement` steps over a `ZEND_AST_DECLARE`
        // before the node it is asked about and over nothing else, so a
        // `namespace` statement is one a later `strict_types` finds.
        self.statements_seen = self.statements_seen.saturating_add(1);
        let Some(body) = body else {
            self.namespacing = Namespacing::Unbracketed;
            return Ok(());
        };
        self.namespacing = Namespacing::Bracketed { open: true };
        self.top_statements(&body)?;
        // `zend_end_namespace`.
        self.namespacing = Namespacing::Bracketed { open: false };
        self.imports.reset();
        self.namespace = None;
        Ok(())
    }

    /// `zend_verify_namespace` (`Z/Zend/zend_compile.c:2166`), after
    /// every top statement but a `namespace` and a `__halt_compiler`:
    /// once the file's namespaces are bracketed, a statement outside
    /// them is php's `E_COMPILE_ERROR`, at the line the statement left.
    ///
    /// ```php
    /// namespace A {}
    /// if (true) {
    ///     echo 1;
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: No code may exist outside of namespace {} in %s on line 4
    /// ```
    pub(super) fn verify_namespace(&self) -> Result<(), CompileError> {
        if self.namespacing == (Namespacing::Bracketed { open: false }) {
            return Err(self.compile_error("No code may exist outside of namespace {}"));
        }
        Ok(())
    }

    /// `zend_compile_use` and `zend_compile_group_use` over one `use`
    /// statement: each name imported in written order under the kind
    /// the statement's keyword, or a group item's own, says. php's
    /// grammar takes a `use` at the file's top level alone, which the
    /// parser holds, and hands the compiler every name without its
    /// leading `\`; a name written `namespace\…` and a group item
    /// written with a leading `\` are its parse errors, in its words.
    ///
    /// ```php
    /// namespace N;
    /// use ArrayObject as AO, Countable;
    /// use function strlen as len;
    /// use const PHP_INT_SIZE as SIZE;
    /// var_dump(new AO([1]) instanceof Countable, len("abc"), SIZE);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(3)
    /// int(8)
    /// ```
    ///
    /// The group form puts its prefix before each item, and an item may
    /// name its own kind where the statement names none:
    ///
    /// ```php
    /// namespace A;
    /// function f() { return __FUNCTION__; }
    /// const K = 'k';
    /// class C {}
    /// namespace B;
    /// use A\{C, function f, const K};
    /// use function A\{f as g};
    /// echo C::class, ' ', f(), ' ', g(), ' ', K, "\n";
    /// ```
    /// ```expect
    /// A\C A\f A\f k
    /// ```
    ///
    /// ```php
    /// use namespace\B;
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected namespace-relative name "namespace\B" in %s on line 2
    /// ```
    pub(super) fn use_stmt(&mut self, id: NodeId<tree::UseStmt>) -> Result<(), CompileError> {
        let node = &self.parsed.tree[id];
        let statement_kind = node.kind.map(|token| self.kind(token));
        if let Some(group) = node.use_group {
            let group = &self.parsed.tree[group];
            self.line = group.line;
            let prefix = self.text(group.prefix).to_vec();
            let prefix = match Written::of(&prefix) {
                Written::Relative(_) => return Err(self.unexpected_name(&prefix)),
                Written::FullyQualified(rest) | Written::NotQualified(rest) => rest.to_vec(),
            };
            for item in group.use_group_items.clone() {
                let item = &self.parsed.tree[item];
                let name = self.text(item.name).to_vec();
                if !matches!(Written::of(&name), Written::NotQualified(_)) {
                    let expecting = if statement_kind.is_some() {
                        "identifier or namespaced name"
                    } else {
                        "identifier or namespaced name or \"function\" or \"const\""
                    };
                    let refused = self.unexpected_name(&name);
                    return Err(self.fatal(format!("{}, expecting {expecting}", refused.message)));
                }
                let kind = Symbol::of(statement_kind.or(item.kind.map(|token| self.kind(token))));
                let alias = item.alias.map(|alias| self.alias_of(alias));
                self.import(kind, concat(&prefix, &name), alias)?;
            }
            return Ok(());
        }
        let kind = Symbol::of(statement_kind);
        for clause in node.use_clauses.clone() {
            let clause = &self.parsed.tree[clause];
            self.line = clause.line;
            let written = self.text(clause.name).to_vec();
            let name = match Written::of(&written) {
                Written::Relative(_) => return Err(self.unexpected_name(&written)),
                Written::FullyQualified(rest) | Written::NotQualified(rest) => rest.to_vec(),
            };
            let alias = clause.alias.map(|alias| self.alias_of(alias));
            self.import(kind, name, alias)?;
        }
        Ok(())
    }

    /// The name after an `as`.
    fn alias_of(&self, alias: NodeId<tree::UseAlias>) -> Vec<u8> {
        self.text(self.parsed.tree[alias].name).to_vec()
    }

    /// php's parse error for a name its `use` grammar has no place for,
    /// named as the scanner's token names it.
    fn unexpected_name(&self, name: &[u8]) -> CompileError {
        let what = if name.starts_with(b"\\") {
            "fully qualified name"
        } else {
            "namespace-relative name"
        };
        self.fatal(format!(
            "syntax error, unexpected {what} \"{}\"",
            String::from_utf8_lossy(name)
        ))
    }

    /// `zend_compile_use` over one `ZEND_AST_USE_ELEM`: the alias is the
    /// one written, or the last segment of the name; a name with no
    /// segment to take is its own alias, which outside a namespace is
    /// php's warning that the import does nothing. The alias is refused
    /// where it is `self`, `parent` or another reserved class name, where
    /// the file has declared a symbol of the kind under it in the current
    /// namespace, and where the table already has it, each in php's
    /// words; a declaration's own name imported again is no conflict.
    ///
    /// ```php
    /// use A\B;
    /// use C\B;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use C\B as B because the name is already in use in %s on line 3
    /// ```
    ///
    /// ```php
    /// namespace N;
    /// function f() {}
    /// use function A\f;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use function A\f as f because the name is already in use in %s on line 4
    /// ```
    ///
    /// ```php
    /// use A\self;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use A\self as self because 'self' is a special class name in %s on line 2
    /// ```
    ///
    /// ```php
    /// use Foo;
    /// echo "ran\n";
    /// ```
    /// ```expect
    /// Warning: The use statement with non-compound name 'Foo' has no effect in %s on line 2
    /// ran
    /// ```
    ///
    /// A constant's alias keeps its case, so two that differ in case are
    /// two imports, where two classes are one:
    ///
    /// ```php
    /// use const A\B;
    /// use const C\b;
    /// echo "ran\n";
    /// ```
    /// ```expect
    /// ran
    /// ```
    fn import(
        &mut self,
        kind: Symbol,
        name: Vec<u8>,
        alias: Option<Vec<u8>>,
    ) -> Result<(), CompileError> {
        let alias = match (alias, name.iter().rposition(|byte| *byte == b'\\')) {
            (Some(alias), _) => alias,
            (None, Some(slash)) => name[slash + 1..].to_vec(),
            (None, None) => {
                if self.namespace.is_none() {
                    self.raise(
                        Level::Warning,
                        &format!(
                            "The use statement with non-compound name '{}' has no effect",
                            String::from_utf8_lossy(&name)
                        ),
                    );
                }
                name.clone()
            }
        };
        let lookup = if kind == Symbol::Const {
            alias.clone()
        } else {
            alias.to_ascii_lowercase()
        };
        if kind == Symbol::Class && is_reserved_class_name(&alias) {
            let alias = String::from_utf8_lossy(&alias);
            return Err(self.compile_error(format!(
                "Cannot use {} as {alias} because '{alias}' is a special class name",
                String::from_utf8_lossy(&name)
            )));
        }
        let key = match &self.namespace {
            Some(namespace) => concat(&namespace.to_ascii_lowercase(), &lookup),
            None => lookup.clone(),
        };
        let already = |this: &Self| {
            this.compile_error(format!(
                "Cannot use{} {} as {} because the name is already in use",
                kind.use_type(),
                String::from_utf8_lossy(&name),
                String::from_utf8_lossy(&alias)
            ))
        };
        // `zend_check_already_in_use`: the declaration the name belongs
        // to is no conflict with an import of that very name.
        if self.imports.have_seen(&key, kind) && !name.eq_ignore_ascii_case(&key) {
            return Err(already(self));
        }
        if self.imports.table(kind).contains_key(&lookup) {
            return Err(already(self));
        }
        self.imports.table(kind).insert(lookup, name);
        Ok(())
    }

    /// `zend_is_not_imported`: whether no class import takes the
    /// unqualified `name` as its alias, whatever its case.
    pub(super) fn not_imported(&self, name: &[u8]) -> bool {
        self.imports.class(name).is_none()
    }

    /// `zend_resolve_class_name` for a name written without `self`,
    /// `parent` or `static`: a fully qualified name is itself, a
    /// relative one the current namespace's, and any other has its
    /// first segment read through the class imports, the whole name
    /// where it has one segment; what no import names takes the current
    /// namespace before it.
    ///
    /// ```php
    /// namespace N;
    /// use ArrayObject as A;
    /// use Random as R;
    /// var_dump(A::class, R\Randomizer::class, namespace\A::class, \A::class, new A instanceof \ArrayObject);
    /// ```
    /// ```expect
    /// string(11) "ArrayObject"
    /// string(17) "Random\Randomizer"
    /// string(3) "N\A"
    /// string(1) "A"
    /// bool(true)
    /// ```
    pub(super) fn resolve_class(&self, name: &[u8]) -> Vec<u8> {
        self.resolve_class_marked(name).0
    }

    /// [`Self::resolve_class`] with the mark php's name carries after it:
    /// `ZEND_NAME_FQ` where the name was written after a `\` or where
    /// resolution rewrote it, through an import or the namespace
    /// (`zend_compile_const_expr_class_const` tests the string it gets
    /// back against the one it gave), and otherwise the mark it was
    /// written with.
    ///
    /// ```php
    /// namespace App;
    /// use Other\Baz;
    /// function f($a = Bar::BAZ, $b = Baz::Y, $c = \Q::X) {}
    /// foreach ((new \ReflectionFunction('App\f'))->getParameters() as $p) { echo $p, "\n"; }
    /// ```
    /// ```expect
    /// Parameter #0 [ <optional> $a = \App\Bar::BAZ ]
    /// Parameter #1 [ <optional> $b = \Other\Baz::Y ]
    /// Parameter #2 [ <optional> $c = \Q::X ]
    /// ```
    pub(super) fn resolve_class_marked(&self, name: &[u8]) -> (Vec<u8>, NameKind) {
        let rewritten = |resolved: Vec<u8>| (resolved, NameKind::FullyQualified);
        let name = match Written::of(name) {
            Written::FullyQualified(rest) => return rewritten(rest.to_vec()),
            Written::Relative(rest) if self.namespace.is_some() => {
                return rewritten(self.prefix_with_ns(rest));
            }
            Written::Relative(rest) => return (rest.to_vec(), NameKind::Relative),
            Written::NotQualified(name) => name,
        };
        match name.iter().position(|byte| *byte == b'\\') {
            Some(slash) => {
                if let Some(import) = self.imports.class(&name[..slash]) {
                    return rewritten(concat(import, &name[slash + 1..]));
                }
            }
            None => {
                if let Some(import) = self.imports.class(name) {
                    return rewritten(import.to_vec());
                }
            }
        }
        if self.namespace.is_some() {
            return rewritten(self.prefix_with_ns(name));
        }
        (name.to_vec(), NameKind::NotFullyQualified)
    }

    /// `zend_resolve_non_class_name` over the function imports or the
    /// constant imports: the name, and whether php counts it fully
    /// qualified, which an unqualified name no import names is not. A
    /// leading `\` is dropped, `namespace\` is the current namespace, an
    /// unqualified name is read through the table of its kind and a
    /// qualified one has its first segment read through the class
    /// imports.
    ///
    /// ```php
    /// namespace N;
    /// use function str_repeat as rep;
    /// use const E_ALL as ALL;
    /// use Random as R;
    /// var_dump(rep("ab", 2), ALL === \E_ALL, R\Engine\Mt19937::class);
    /// ```
    /// ```expect
    /// string(4) "abab"
    /// bool(true)
    /// string(21) "Random\Engine\Mt19937"
    /// ```
    pub(super) fn resolve_non_class(&self, name: &[u8], kind: Symbol) -> (Vec<u8>, bool) {
        let name = match Written::of(name) {
            Written::FullyQualified(rest) => return (rest.to_vec(), true),
            Written::Relative(rest) => return (self.prefix_with_ns(rest), true),
            Written::NotQualified(name) => name,
        };
        let imported = match kind {
            Symbol::Function => self.imports.function(name),
            Symbol::Const => self.imports.constant(name),
            Symbol::Class => None,
        };
        if let Some(import) = imported {
            return (import.to_vec(), true);
        }
        let Some(slash) = name.iter().position(|byte| *byte == b'\\') else {
            return (self.prefix_with_ns(name), false);
        };
        if let Some(import) = self.imports.class(&name[..slash]) {
            return (concat(import, &name[slash + 1..]), true);
        }
        (self.prefix_with_ns(name), true)
    }

    /// The check `zend_compile_class_decl`, `zend_begin_func_decl` and
    /// `zend_compile_const_decl` make of a declaration's name against the
    /// imports, then `zend_register_seen_symbol`: a class or a function
    /// whose unqualified name an import of its kind names, where the
    /// import is not the declaration itself, is php's compile error, and
    /// so is a constant's. `unqualified` is the name written, and the
    /// answer the declaration's full name, `zend_prefix_with_ns` over it.
    ///
    /// ```php
    /// use A\B;
    /// class B {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot redeclare class B (previously declared as local import) in %s on line 3
    /// ```
    ///
    /// ```php
    /// namespace N;
    /// use function A\f;
    /// function f() {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot redeclare function N\f() (previously declared as local import) in %s on line 4
    /// ```
    ///
    /// ```php
    /// namespace N;
    /// use const A\K;
    /// const K = 1;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot declare const N\K because the name is already in use in %s on line 4
    /// ```
    ///
    /// A constant registers its name with the namespace as written and
    /// an import looks it up with the namespace lowered, so php refuses
    /// no import after a constant of the name in a namespace spelled
    /// with a capital, which is its behaviour and so this one's:
    ///
    /// ```php
    /// namespace N;
    /// const K = 1;
    /// use const A\K;
    /// echo "ran\n";
    /// ```
    /// ```expect
    /// ran
    /// ```
    pub(super) fn declare_symbol(
        &mut self,
        kind: Symbol,
        unqualified: &[u8],
    ) -> Result<Vec<u8>, CompileError> {
        let full = self.prefix_with_ns(unqualified);
        let name = full.as_slice();
        let shown = String::from_utf8_lossy(name);
        match kind {
            Symbol::Class => {
                if self
                    .imports
                    .class(unqualified)
                    .is_some_and(|import| !import.eq_ignore_ascii_case(name))
                {
                    return Err(self.compile_error(format!(
                        "Cannot redeclare class {shown} (previously declared as local import)"
                    )));
                }
                self.imports.see(name.to_ascii_lowercase(), kind);
            }
            Symbol::Function => {
                if self
                    .imports
                    .function(unqualified)
                    .is_some_and(|import| !import.eq_ignore_ascii_case(name))
                {
                    return Err(self.compile_error(format!(
                        "Cannot redeclare function {shown}() (previously declared as local import)"
                    )));
                }
                self.imports.see(name.to_ascii_lowercase(), kind);
            }
            Symbol::Const => {
                if self
                    .imports
                    .constant(unqualified)
                    .is_some_and(|import| import != name)
                {
                    return Err(self.compile_error(format!(
                        "Cannot declare const {shown} because the name is already in use"
                    )));
                }
                self.imports.see(name.to_vec(), kind);
            }
        }
        Ok(full)
    }
}
