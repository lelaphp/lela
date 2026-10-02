//! The op arrays and classes a tier reads by name between the steps it
//! drives: what phpdbg's `print`, `list`, `info` and its opline
//! breakpoints read off `EG(function_table)` and `EG(class_table)`. Each
//! is a copy taken when asked.

use php_heap::FrameRef;
use php_unit::Opcode;

use crate::api::request::Request;

/// A function or a method as its table holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    /// `function_name`, as declared.
    pub name: Vec<u8>,
    /// `scope`'s name, for a method.
    pub class: Option<Vec<u8>>,
    /// Whether it is user code, `ZEND_USER_FUNCTION`.
    pub user: bool,
    /// Where it was declared, and the lines its op array spans.
    pub file: Vec<u8>,
    pub lines: (u32, u32),
    /// `op_array->opcodes`, each opcode with its line; `op_array->last`
    /// is its length. Empty for a native.
    pub ops: Vec<(Opcode, u32)>,
    /// `num_args`, and one more for a variadic: how many `RECV*` oplines
    /// open the op array.
    pub params: u32,
    /// `last_var`: how many compiled variables the op array names; zero
    /// for a native.
    pub vars: u32,
    /// The op array as phpdbg's `print` writes it, and each function
    /// its body declares after it (`dynamic_func_defs`), as
    /// `phpdbg_print_function_helper` writes them; empty for a native.
    pub dump: Vec<u8>,
}

/// What a name found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    Code(Code),
    /// The class named is none the request knows.
    NoClass,
    /// The function or the method named is none it knows.
    NoFunction,
}

/// What a class is, as phpdbg's `print class` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Interface,
    AbstractClass,
    Class,
}

/// A class as its table holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    pub name: Vec<u8>,
    pub user: bool,
    pub shape: Shape,
    pub file: Vec<u8>,
    pub lines: (u32, u32),
    /// Its own methods, in the order it declares them.
    pub methods: Vec<Code>,
    /// The parent it names that no class of that name was declared for
    /// yet, where it names one: phpdbg's "not yet linked".
    pub unlinked_parent: Option<Vec<u8>>,
}

/// `EG(function_table)`'s entry under `name`, case-insensitive.
#[must_use]
pub fn function_named(req: &Request, name: &[u8]) -> Named {
    req.code_named(None, name)
}

/// The method `name` of the class `class`, both case-insensitive, a
/// leading `\` of the class's name dropped.
#[must_use]
pub fn method_named(req: &Request, class: &[u8], name: &[u8]) -> Named {
    req.code_named(Some(class), name)
}

/// The class under `name`, case-insensitive, a leading `\` dropped.
#[must_use]
pub fn class_named(req: &Request, name: &[u8]) -> Option<Class> {
    req.class_view(name)
}

/// The user functions of the function table in its order, no method and
/// no closure among them.
#[must_use]
pub fn user_functions(req: &Request) -> Vec<Code> {
    req.user_function_views()
}

/// The user classes the request has declared, in the order it declared
/// them.
#[must_use]
pub fn user_classes(req: &Request) -> Vec<Class> {
    req.user_class_views()
}

/// The op array `frame` serves; for a file's code, `name` is empty.
#[must_use]
pub fn frame_code(req: &Request, frame: FrameRef) -> Option<Code> {
    req.frame_code_view(frame)
}

/// How many entries each of the tables `clean` counts holds: classes,
/// functions and constants, the internal ones with the user ones, and
/// the included files.
#[must_use]
pub fn table_sizes(req: &Request) -> [usize; 4] {
    req.table_sizes()
}

#[cfg(test)]
mod tests {
    //! `clean`'s class count as `EG(class_table)` holds it.

    use std::cell::Cell;

    use php_heap::FrameRef;

    use crate::api::request::Request;
    use crate::engine::Engine;
    use crate::test::Capture;
    use crate::tier::{Flow, Site, Tier};

    thread_local! {
        /// The class count read at the entry of the last function called.
        static COUNTED: Cell<Option<usize>> = const { Cell::new(None) };
    }

    struct Counter;

    impl Tier for Counter {
        fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
            if site == Site::Entry && frame != crate::tier::entry(req) {
                COUNTED.set(Some(super::table_sizes(req)[0]));
            }
            Flow::Continue
        }
    }

    /// Every key of the table is counted, an alias's among them, the
    /// engine's (`Dom\DOMException`'s kind) and `class_alias()`'s, and
    /// the iterator wrapper php never adds is not: the count is what
    /// `get_declared_classes()`, `get_declared_interfaces()` and
    /// `get_declared_traits()` list together.
    #[test]
    fn the_class_count_is_the_class_tables_keys() {
        COUNTED.set(None);
        let engine = Engine::builder().tier::<Counter>().build();
        let served = engine.serve_capture(
            "<?php\nclass A {}\ninterface I {}\ntrait T {}\nclass_alias('A', 'B');\nclass_alias('A', 'C');\nfunction n(...$a) { return func_num_args(); }\nfunction probe() {}\necho n(...get_declared_classes(), ...get_declared_interfaces(), ...get_declared_traits());\nprobe();\n",
            Capture::default(),
        );
        let listed = String::from_utf8_lossy(&served.stdout).into_owned();
        assert_eq!(COUNTED.get().map(|n| n.to_string()), Some(listed));
    }
}
