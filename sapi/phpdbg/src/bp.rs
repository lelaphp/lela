//! Breakpoints, as `Z/sapi/phpdbg/phpdbg_bp.c` keeps them: numbered in
//! the order they were asked for, one count shared by every kind, each
//! with its hits; found at an opline in php's order (a file's line, a
//! function's or a method's first body opline, an opline of a named op
//! array, an opcode); printed as php prints them when hit and when
//! listed.

use crate::out::Out;

/// What a breakpoint stops at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `PHPDBG_BREAK_FILE`: a line of a compiled file.
    File { file: Vec<u8>, line: i64 },
    /// `PHPDBG_BREAK_FILE_PENDING`: a line of a file named relatively and
    /// not compiled yet, resolved against the files compiled after.
    PendingFile { file: Vec<u8>, line: i64 },
    /// `PHPDBG_BREAK_SYM`: a function by name, `main` for a file's code.
    Symbol { name: Vec<u8> },
    /// `PHPDBG_BREAK_METHOD`.
    Method { class: Vec<u8>, name: Vec<u8> },
    /// `PHPDBG_BREAK_FUNCTION_OPLINE`.
    FunctionOpline { name: Vec<u8>, opline: u32 },
    /// `PHPDBG_BREAK_METHOD_OPLINE`.
    MethodOpline {
        class: Vec<u8>,
        name: Vec<u8>,
        opline: u32,
    },
    /// `PHPDBG_BREAK_FILE_OPLINE`.
    FileOpline { file: Vec<u8>, opline: u32 },
    /// `PHPDBG_BREAK_OPLINE`: one opline of one op array, which php keys
    /// by the opline's address and prints as that number, `address` here.
    Opline {
        file: Vec<u8>,
        function: Option<Vec<u8>>,
        class: Option<Vec<u8>>,
        opline: u32,
        address: u64,
    },
    /// `PHPDBG_BREAK_OPCODE`: every opline of an opcode, by php's name.
    Opcode { name: Vec<u8> },
}

/// One breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Break {
    pub id: u32,
    pub kind: Kind,
    pub hits: u64,
    pub disabled: bool,
    /// `phpdbg_breakopline_t.opline`: the address an opline of a named
    /// op array was resolved to, none until it is.
    pub resolved: Option<u64>,
}

/// An op array as phpdbg's tables key it: its file, its function's name
/// as declared, none for a file's code, and the class it was declared in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpArray<'a> {
    pub file: &'a [u8],
    pub function: Option<&'a [u8]>,
    pub class: Option<&'a [u8]>,
}

/// The table of `PHPDBG_BP_RESOLVE_MASK` an opline of a named op array
/// is kept in, each with its flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    /// `PHPDBG_BREAK_FUNCTION_OPLINE`, `PHPDBG_HAS_FUNCTION_OPLINE_BP`.
    Function = 1,
    /// `PHPDBG_BREAK_METHOD_OPLINE`, `PHPDBG_HAS_METHOD_OPLINE_BP`.
    Method = 2,
    /// `PHPDBG_BREAK_FILE_OPLINE`, `PHPDBG_HAS_FILE_OPLINE_BP`.
    File = 4,
}

impl Table {
    /// The table an op array's opline resolves into: a method's where it
    /// has a scope, a function's where it has a name, a file's else, as
    /// `phpdbg_resolve_op_array_break` types the opline it keeps.
    fn of(op: OpArray<'_>) -> Table {
        match (op.function, op.class) {
            (None, _) => Table::File,
            (Some(_), None) => Table::Function,
            (Some(_), Some(_)) => Table::Method,
        }
    }

    /// The word `info break` writes for an opline resolved into it.
    fn word(self) -> &'static str {
        match self {
            Table::Function => "function",
            Table::Method => "method",
            Table::File => "file",
        }
    }
}

/// What `phpdbg_resolve_opline_break` found for an opline of a named op
/// array: the opline, at its address, or nothing yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    At(u64),
    Pending,
}

/// One row of `PHPDBG_BREAK_OPLINE`: the opline's address, the number of
/// the breakpoint php keeps under it, and the table of the breakpoint it
/// was resolved from, none for one set on the opline itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Kept {
    address: u64,
    id: u32,
    from: Option<Table>,
}

impl Kind {
    /// How php names an opline of a named op array in the notices it
    /// writes when it sets one: `foo#0`, `A::b#0`, `f.php:0`.
    fn shown(&self) -> Option<String> {
        match self {
            Self::FunctionOpline { name, opline } => Some(format!("{}#{opline}", lossy(name))),
            Self::MethodOpline {
                class,
                name,
                opline,
            } => Some(format!("{}::{}#{opline}", lossy(class), lossy(name))),
            Self::FileOpline { file, opline } => Some(format!("{}:{opline}", lossy(file))),
            _ => None,
        }
    }

    /// The resolve table the breakpoint is kept in, none for a kind php
    /// does not resolve.
    fn table(&self) -> Option<Table> {
        match self {
            Self::FunctionOpline { .. } => Some(Table::Function),
            Self::MethodOpline { .. } => Some(Table::Method),
            Self::FileOpline { .. } => Some(Table::File),
            _ => None,
        }
    }

    /// Whether two breakpoints are one key of their name's table, its
    /// opline, which php refuses a second of.
    fn same_opline(&self, other: &Self) -> bool {
        self.shares_names(other)
            && match (self, other) {
                (Self::FunctionOpline { opline, .. }, Self::FunctionOpline { opline: o, .. })
                | (Self::MethodOpline { opline, .. }, Self::MethodOpline { opline: o, .. })
                | (Self::FileOpline { opline, .. }, Self::FileOpline { opline: o, .. }) => {
                    opline == o
                }
                _ => false,
            }
    }

    /// Whether two breakpoints sit in one name's table of their resolve
    /// table, which php keys by the name as given: a function's, a
    /// class's and its method's, a file's.
    fn shares_names(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::FunctionOpline { name, .. }, Self::FunctionOpline { name: n, .. }) => name == n,
            (
                Self::MethodOpline { class, name, .. },
                Self::MethodOpline {
                    class: c, name: n, ..
                },
            ) => class == c && name == n,
            (Self::FileOpline { file, .. }, Self::FileOpline { file: f, .. }) => file == f,
            _ => false,
        }
    }
}

/// Where the loop stands, as a breakpoint is held to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct At<'a> {
    pub file: &'a [u8],
    pub line: u32,
    pub opline: u32,
    /// The first opline past the `RECV*` run.
    pub body: u32,
    pub opcode: &'a str,
    /// The function served, none for a file's code.
    pub function: Option<&'a [u8]>,
    /// The class the function was declared in.
    pub class: Option<&'a [u8]>,
}

/// How `info break` renders a breakpoint of one table, none for another's.
type Row = fn(&Kind) -> Option<String>;

/// Where one table of `info break` is read from.
enum Listed {
    /// The breakpoints of the kinds a row renders, titled where there is
    /// one.
    Kinds(Row),
    /// `PHPDBG_BREAK_OPLINE`, titled while `PHPDBG_HAS_OPLINE_BP` is set.
    Oplines,
}

/// The breakpoints of the session.
#[derive(Debug, Default, Clone)]
pub struct Breakpoints {
    pub list: Vec<Break>,
    /// `PHPDBG_G(bp_count)`.
    pub count: u32,
    /// `PHPDBG_IS_BP_ENABLED`.
    pub disabled: bool,
    /// `PHPDBG_BREAK_OPLINE` in its order: each address with the number
    /// of the breakpoint php keeps under it, a resolved one's or one set
    /// on the opline itself.
    oplines: Vec<Kept>,
    /// `PHPDBG_HAS_OPLINE_BP`: a row was kept in the opline table since
    /// the breakpoints were last cleared, which a delete does not take
    /// back, so `info break` titles the table while it is empty.
    has_oplines: bool,
    /// `PHPDBG_BP_RESOLVE_MASK`'s flags, a [`Table`]'s bit each.
    resolving: u8,
}

/// `strcasecmp` over two names.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `"name"` with `php_addcslashes_str(name, "\\\"\n")` between: a
/// backslash before each of the three, the newline written `\n`.
fn quoted(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 2);
    out.push(b'"');
    for byte in name {
        match byte {
            b'\\' | b'"' => out.extend_from_slice(&[b'\\', *byte]),
            b'\n' => out.extend_from_slice(b"\\n"),
            _ => out.push(*byte),
        }
    }
    out.push(b'"');
    out
}

impl Breakpoints {
    /// `PHPDBG_BREAK_INIT`: the next number.
    pub fn next_id(&mut self) -> u32 {
        let id = self.count;
        self.count += 1;
        id
    }

    /// A breakpoint added under `id`, resolved to the opline at
    /// `resolved` where it is one.
    fn push(&mut self, id: u32, kind: Kind, resolved: Option<u64>) {
        if let Some(table) = kind.table() {
            self.resolving |= table as u8;
        }
        self.list.push(Break {
            id,
            kind,
            hits: 0,
            disabled: false,
            resolved,
        });
    }

    /// `phpdbg_set_breakpoint_function_opline`, `_method_opline` and
    /// `_file_opline` past `phpdbg_resolve_opline_break`, which the
    /// caller ran for `id`, the number `next_id` gave, and which found
    /// the opline or left it pending: the opline table
    /// keeps the number under that opline first, then the notice, and a
    /// second breakpoint on one opline of one name is refused after it,
    /// its number given back.
    ///
    /// ```php
    /// function foo() {
    ///   return 1;
    /// }
    /// class A { function b() { return 2; } }
    /// ```
    /// ```request phpdbg
    /// b foo#0
    /// b foo#0
    /// b baz#0
    /// b baz#0
    /// b A::b#0
    /// b A::b#0
    /// b f.php:#0
    /// b f.php:#0
    /// q
    /// ```
    /// ```expect
    /// [Successful compilation of %SCRATCH%/witness.php]
    /// prompt> [Breakpoint #0 added at foo#0]
    /// prompt> [Breakpoint #1 added at foo#0]
    /// [Breakpoint already exists for foo#0]
    /// prompt> [Pending breakpoint #1 at baz#0]
    /// prompt> [Pending breakpoint #2 at baz#0]
    /// [Breakpoint already exists for baz#0]
    /// prompt> [Breakpoint #2 added at A::b#0]
    /// prompt> [Breakpoint #3 added at A::b#0]
    /// [Breakpoint already exists for A::b#0]
    /// prompt> [Pending breakpoint #3 at f.php:0]
    /// prompt> [Pending breakpoint #4 at f.php:0]
    /// [Breakpoint already exists for f.php:0]
    /// prompt>
    /// ```
    pub fn set_named_opline(&mut self, out: &mut Out, id: u32, kind: Kind, found: Resolution) {
        let (Some(shown), Some(table)) = (kind.shown(), kind.table()) else {
            return;
        };
        let resolved = match found {
            Resolution::At(address) => {
                self.keep(address, id, Some(table));
                out.notice(format!("Breakpoint #{id} added at {shown}").as_bytes());
                Some(address)
            }
            Resolution::Pending => {
                out.notice(format!("Pending breakpoint #{id} at {shown}").as_bytes());
                None
            }
        };
        if self.has(|k| k.same_opline(&kind)) {
            out.error(format!("Breakpoint already exists for {shown}").as_bytes());
            self.count -= 1;
            return;
        }
        self.push(id, kind, resolved);
    }

    /// `zend_hash_index_update` of the opline table: the row at
    /// `address` given the number `id`, in its place, or added at the end.
    fn keep(&mut self, address: u64, id: u32, from: Option<Table>) {
        self.has_oplines = true;
        let kept = Kept { address, id, from };
        match self.oplines.iter_mut().find(|row| row.address == address) {
            Some(row) => *row = kept,
            None => self.oplines.push(kept),
        }
    }

    /// Whether a resolve flag is set, `PHPDBG_BP_RESOLVE_MASK`.
    pub fn resolves(&self) -> bool {
        self.resolving != 0
    }

    /// `phpdbg_resolve_op_array_break` past its check: the breakpoint
    /// numbered `id` resolved to `address`, an opline of an op array of
    /// `table`, and kept in the opline table under it.
    fn resolve_to(&mut self, id: u32, address: u64, table: Table) {
        if let Some(brake) = self.list.iter_mut().find(|brake| brake.id == id) {
            brake.resolved = Some(address);
        }
        self.keep(address, id, Some(table));
    }

    /// `phpdbg_resolve_op_array_breaks`, before each opline the loop
    /// runs, while a breakpoint has been set on an opline of a function,
    /// a method or a file's code: each one set on `op`'s by the name it
    /// was declared with, the class first for a method, resolved there or
    /// refused where the op array holds no more than `last` oplines. php
    /// says so each time, naming the breakpoint and the address the
    /// opline table took last, which need not be the one just resolved.
    ///
    /// ```php
    /// function foo() {
    ///   $a = 1;
    ///   return $a;
    /// }
    /// if (true) {
    ///   function bar() {
    ///     return 2;
    ///   }
    /// }
    /// foo();
    /// bar();
    /// ```
    /// ```request phpdbg
    /// b foo#0
    /// b bar#8
    /// r
    /// c
    /// q
    /// ```
    /// ```expect
    /// [Successful compilation of %SCRATCH%/witness.php]
    /// prompt> [Breakpoint #0 added at foo#0]
    /// prompt> [Pending breakpoint #1 at bar#8]
    /// prompt> [Breakpoint #0 resolved at foo#0 (opline #%d)]
    /// [Breakpoint #0 in foo()#0 at %SCRATCH%/witness.php:3, hits: 1]
    /// >00003:   $a = 1;
    ///  00004:   return $a;
    ///  00005: }
    /// prompt> [Breakpoint #0 resolved at foo#0 (opline #%d)]
    /// [There are only 2 oplines in function bar (breaking at opline 8 impossible)]
    /// [Script ended normally]
    /// prompt>
    /// ```
    pub fn resolve(&mut self, out: &mut Out, op: OpArray<'_>, last: u32) {
        let table = Table::of(op);
        if self.resolving & table as u8 == 0 {
            return;
        }
        let set: Vec<(u32, Kind)> = self
            .list
            .iter()
            .filter(|brake| match &brake.kind {
                Kind::FunctionOpline { name, .. } => {
                    table == Table::Function && op.function == Some(name)
                }
                Kind::MethodOpline { class, name, .. } => {
                    op.class == Some(class) && op.function == Some(name)
                }
                Kind::FileOpline { file, .. } => table == Table::File && op.file == file,
                _ => false,
            })
            .map(|brake| (brake.id, brake.kind.clone()))
            .collect();
        for (id, kind) in set {
            let (what, shown, opline) = match &kind {
                Kind::FunctionOpline { name, opline } => ("function", lossy(name), *opline),
                Kind::MethodOpline {
                    class,
                    name,
                    opline,
                } => (
                    "method",
                    format!("{}::{}", lossy(class), lossy(name)),
                    *opline,
                ),
                Kind::FileOpline { file, opline } => ("file", lossy(file), *opline),
                _ => continue,
            };
            if last <= opline {
                out.error(
                    format!(
                        "There are only {last} oplines in {what} {shown} (breaking at opline {opline} impossible)"
                    )
                    .as_bytes(),
                );
                continue;
            }
            self.resolve_to(id, address(op, opline), table);
            let Some(Kept { address, id, .. }) = self.oplines.last() else {
                continue;
            };
            out.notice(
                format!("Breakpoint #{id} resolved at {shown}#{opline} (opline #{address})")
                    .as_bytes(),
            );
        }
    }

    fn has(&self, test: impl Fn(&Kind) -> bool) -> bool {
        self.list.iter().any(|brake| test(&brake.kind))
    }

    /// `phpdbg_set_breakpoint_file` past its path resolution: `file` the
    /// path a compiled file carries, or none where the path named no
    /// compiled file and is left pending under the name as given.
    pub fn set_file(&mut self, out: &mut Out, file: Option<Vec<u8>>, named: &[u8], line: i64) {
        let (kind, pending) = match file {
            Some(file) => (Kind::File { file, line }, false),
            None => (
                Kind::PendingFile {
                    file: named.to_vec(),
                    line,
                },
                true,
            ),
        };
        let exists = self.has(|k| match (k, &kind) {
            (Kind::File { file, line }, Kind::File { file: f, line: l })
            | (Kind::PendingFile { file, line }, Kind::PendingFile { file: f, line: l }) => {
                file == f && line == l
            }
            _ => false,
        });
        let (Kind::File { file, .. } | Kind::PendingFile { file, .. }) = &kind else {
            return;
        };
        if exists {
            out.error(format!("Breakpoint at {}:{line} exists", lossy(file)).as_bytes());
            return;
        }
        let id = self.next_id();
        let message = if pending {
            format!("Pending breakpoint #{id} added at {}:{line}", lossy(file))
        } else {
            format!("Breakpoint #{id} added at {}:{line}", lossy(file))
        };
        self.push(id, kind, None);
        out.notice(message.as_bytes());
    }

    /// `phpdbg_resolve_pending_file_break`: every pending line whose name
    /// is the tail of `file` after a `/`, or the whole of it, made a
    /// line of `file`.
    pub fn resolve_pending(&mut self, file: &[u8]) {
        for brake in &mut self.list {
            let Kind::PendingFile { file: named, line } = &brake.kind else {
                continue;
            };
            let tail = file.len() > named.len()
                && file.ends_with(named)
                && file[file.len() - named.len() - 1] == b'/';
            if tail || file == named.as_slice() {
                brake.kind = Kind::File {
                    file: file.to_vec(),
                    line: *line,
                };
            }
        }
    }

    /// `phpdbg_set_breakpoint_symbol`.
    pub fn set_symbol(&mut self, out: &mut Out, name: &[u8]) {
        let name = name.strip_prefix(b"\\").unwrap_or(name);
        if self.has(|k| matches!(k, Kind::Symbol { name: n } if n == name)) {
            out.error(format!("Breakpoint exists at {}", lossy(name)).as_bytes());
            return;
        }
        let id = self.next_id();
        self.push(
            id,
            Kind::Symbol {
                name: name.to_vec(),
            },
            None,
        );
        out.notice(format!("Breakpoint #{id} added at {}", lossy(name)).as_bytes());
    }

    /// `phpdbg_set_breakpoint_method`.
    pub fn set_method(&mut self, out: &mut Out, class: &[u8], name: &[u8]) {
        let class = class.strip_prefix(b"\\").unwrap_or(class);
        if self.has(
            |k| matches!(k, Kind::Method { class: c, name: n } if same(c, class) && same(n, name)),
        ) {
            out.error(format!("Breakpoint exists at {}::{}", lossy(class), lossy(name)).as_bytes());
            return;
        }
        let id = self.next_id();
        self.push(
            id,
            Kind::Method {
                class: class.to_vec(),
                name: name.to_vec(),
            },
            None,
        );
        out.notice(
            format!(
                "Breakpoint #{id} added at {}::{}",
                lossy(class),
                lossy(name)
            )
            .as_bytes(),
        );
    }

    /// `phpdbg_set_breakpoint_opcode`.
    pub fn set_opcode(&mut self, out: &mut Out, name: &[u8]) {
        if self.has(|k| matches!(k, Kind::Opcode { name: n } if n == name)) {
            out.error(format!("Breakpoint exists for {}", lossy(name)).as_bytes());
            return;
        }
        let id = self.next_id();
        self.push(
            id,
            Kind::Opcode {
                name: name.to_vec(),
            },
            None,
        );
        out.notice(format!("Breakpoint #{id} added at {}", lossy(name)).as_bytes());
    }

    /// `phpdbg_set_breakpoint_opline_ex`: the opline `opline` of the op
    /// array `at` names (its file, function and class), numbered as php
    /// numbers it by address.
    pub fn set_opline(&mut self, out: &mut Out, at: &At<'_>, opline: u32) {
        let address = address(at.op_array(), opline);
        if self.oplines.iter().any(|row| row.address == address) {
            out.error(format!("Breakpoint exists for opline #{address}").as_bytes());
            return;
        }
        let id = self.next_id();
        self.keep(address, id, None);
        self.push(
            id,
            Kind::Opline {
                file: at.file.to_vec(),
                function: at.function.map(<[u8]>::to_vec),
                class: at.class.map(<[u8]>::to_vec),
                opline,
                address,
            },
            None,
        );
        out.notice(format!("Breakpoint #{id} added at #{address}").as_bytes());
    }

    /// `phpdbg_clear()`'s `zend_hash_clean` of every table but the
    /// opcodes', the count left as it stands.
    pub fn clean(&mut self) {
        self.list
            .retain(|brake| matches!(brake.kind, Kind::Opcode { .. }));
        self.oplines.clear();
    }

    /// `phpdbg_delete_breakpoint`. A breakpoint on an opline of a named
    /// op array takes its resolve flag with it where it was the last of
    /// its name's table, every resolve flag where the opline table held
    /// one breakpoint, as php's `flags &= PHPDBG_HAS_OPLINE_BP` clears
    /// them, and its resolved opline.
    pub fn delete(&mut self, out: &mut Out, id: i64) {
        let Some(at) = self.list.iter().position(|brake| i64::from(brake.id) == id) else {
            out.error(format!("Failed to find breakpoint #{id}").as_bytes());
            return;
        };
        let brake = self.list.remove(at);
        let kept = match (&brake.kind, brake.kind.table()) {
            (Kind::Opline { address, .. }, _) => Some(*address),
            (kind, Some(table)) => {
                if !self.has(|k| k.shares_names(kind)) {
                    self.resolving &= !(table as u8);
                }
                if self.oplines.len() == 1 {
                    self.resolving = 0;
                }
                brake.resolved
            }
            _ => None,
        };
        if let Some(address) = kept {
            self.oplines.retain(|row| row.address != address);
        }
        out.notice(format!("Deleted breakpoint #{id}").as_bytes());
    }

    /// `phpdbg_enable_breakpoint` and `phpdbg_disable_breakpoint`: the
    /// breakpoint numbered `id`, where there is one, made to stop or not.
    pub fn enable(&mut self, id: i64, on: bool) {
        if let Some(brake) = self.list.iter_mut().find(|b| i64::from(b.id) == id) {
            brake.disabled = !on;
        }
    }

    /// Whether the breakpoint numbered `id` stops, none where there is no
    /// such breakpoint (`phpdbg_find_breakbase`).
    pub fn enabled(&self, id: i64) -> Option<bool> {
        self.list
            .iter()
            .find(|b| i64::from(b.id) == id)
            .map(|b| !b.disabled)
    }

    /// `phpdbg_reset_breakpoints`: every hit count back to zero.
    pub fn reset_hits(&mut self) {
        for brake in &mut self.list {
            brake.hits = 0;
        }
    }

    /// `phpdbg_find_breakpoint`: the breakpoint `at` stops at, php's order.
    pub fn find(&self, at: &At<'_>) -> Option<usize> {
        if self.disabled {
            return None;
        }
        let found = self
            .position(|k| matches!(k, Kind::File { file, line } if file == at.file && *line == i64::from(at.line)))
            .or_else(|| {
                if at.opline != at.body {
                    return None;
                }
                match (at.function, at.class) {
                    (Some(function), Some(class)) => self.position(|k| {
                        matches!(k, Kind::Method { class: c, name: n } if same(c, class) && same(n, function))
                    }),
                    (Some(function), None) => {
                        self.position(|k| matches!(k, Kind::Symbol { name } if same(name, function)))
                    }
                    (None, _) => self.position(|k| matches!(k, Kind::Symbol { name } if same(name, b"main"))),
                }
            })
            .or_else(|| {
                self.position(|k| match k {
                    Kind::FunctionOpline { name, opline } => {
                        at.class.is_none() && at.function.is_some_and(|f| same(f, name)) && *opline == at.opline
                    }
                    Kind::MethodOpline { class, name, opline } => {
                        at.class.is_some_and(|c| same(c, class))
                            && at.function.is_some_and(|f| same(f, name))
                            && *opline == at.opline
                    }
                    Kind::FileOpline { file, opline } => {
                        at.function.is_none() && file == at.file && *opline == at.opline
                    }
                    Kind::Opline { address, .. } => *address == self::address(at.op_array(), at.opline),
                    _ => false,
                })
            })
            .or_else(|| self.position(|k| matches!(k, Kind::Opcode { name } if name == at.opcode.as_bytes())))?;
        (!self.list[found].disabled).then_some(found)
    }

    fn position(&self, test: impl Fn(&Kind) -> bool) -> Option<usize> {
        self.list.iter().position(|brake| test(&brake.kind))
    }

    /// `phpdbg_hit_breakpoint` and `phpdbg_print_breakpoint`.
    pub fn hit(&mut self, out: &mut Out, found: usize, at: &At<'_>) {
        let Some(brake) = self.list.get_mut(found) else {
            return;
        };
        brake.hits += 1;
        let (id, hits) = (brake.id, brake.hits);
        let (file, line) = (lossy(at.file), at.line);
        let message = match &brake.kind {
            Kind::File { file, line } | Kind::PendingFile { file, line } => {
                format!("Breakpoint #{id} at {}:{line}, hits: {hits}", lossy(file))
            }
            Kind::Symbol { name } => {
                format!(
                    "Breakpoint #{id} in {}() at {file}:{line}, hits: {hits}",
                    lossy(name)
                )
            }
            Kind::Method { class, name } => format!(
                "Breakpoint #{id} in {}::{}() at {file}:{line}, hits: {hits}",
                lossy(class),
                lossy(name)
            ),
            Kind::MethodOpline {
                class,
                name,
                opline,
            } => format!(
                "Breakpoint #{id} in {}::{}()#{opline} at {file}:{line}, hits: {hits}",
                lossy(class),
                lossy(name)
            ),
            Kind::FunctionOpline { name, opline } => {
                format!(
                    "Breakpoint #{id} in {}()#{opline} at {file}:{line}, hits: {hits}",
                    lossy(name)
                )
            }
            Kind::FileOpline { opline, .. } => {
                format!("Breakpoint #{id} in #{opline} at {file}:{line}, hits: {hits}")
            }
            Kind::Opline { address, .. } => {
                format!("Breakpoint #{id} in #{address} at {file}:{line}, hits: {hits}")
            }
            Kind::Opcode { name } => {
                format!(
                    "Breakpoint #{id} in {} at {file}:{line}, hits: {hits}",
                    lossy(name)
                )
            }
        };
        out.notice(message.as_bytes());
    }

    /// `phpdbg_clear_breakpoints`, after `clear`'s count of each table.
    pub fn clear(&mut self, out: &mut Out) {
        let count =
            |test: &dyn Fn(&Kind) -> bool| self.list.iter().filter(|b| test(&b.kind)).count();
        let distinct = |key: &dyn Fn(&Kind) -> Option<Vec<u8>>| {
            let mut seen: Vec<Vec<u8>> = Vec::new();
            for key in self.list.iter().filter_map(|b| key(&b.kind)) {
                if !seen.contains(&key) {
                    seen.push(key);
                }
            }
            seen.len()
        };
        let files = distinct(&|k| match k {
            Kind::File { file, .. } => Some(file.clone()),
            _ => None,
        });
        let symbols = count(&|k| matches!(k, Kind::Symbol { .. }));
        let methods = distinct(&|k| match k {
            Kind::Method { class, .. } => Some(class.to_ascii_lowercase()),
            _ => None,
        });
        let oplines = count(&|k| {
            matches!(
                k,
                Kind::FunctionOpline { .. }
                    | Kind::MethodOpline { .. }
                    | Kind::FileOpline { .. }
                    | Kind::Opline { .. }
            )
        });
        let file_oplines = count(&|k| matches!(k, Kind::FileOpline { .. }));
        let function_oplines = count(&|k| matches!(k, Kind::FunctionOpline { .. }));
        let method_oplines = count(&|k| matches!(k, Kind::MethodOpline { .. }));
        out.out(b"Clearing Breakpoints\n");
        for (label, n) in [
            ("File              ", files),
            ("Functions         ", symbols),
            ("Methods           ", methods),
            ("Oplines           ", oplines),
            ("File oplines      ", file_oplines),
            ("Function oplines  ", function_oplines),
            ("Method oplines    ", method_oplines),
            ("Conditionals      ", 0),
        ] {
            out.writeln(format!("{label}{n}").as_bytes());
        }
        self.list.clear();
        self.oplines.clear();
        self.has_oplines = false;
        self.resolving = 0;
        self.count = 0;
    }

    /// `info break`: each table in `phpdbg_info_break`'s order, a
    /// separator and its title before a table with rows; the opline
    /// table's in its own order, a resolved row with the kind of the
    /// breakpoint it was resolved from, as `phpdbg_print_breakpoints`
    /// writes it.
    ///
    /// ```php
    /// function foo() {
    ///   return 1;
    /// }
    /// class A { function b() { return 2; } }
    /// foo();
    /// ```
    /// ```request phpdbg
    /// b foo
    /// r
    /// b foo#0
    /// b A::b#0
    /// b foo#0
    /// b ZEND_ECHO
    /// i b
    /// q
    /// ```
    /// ```expect
    /// [Successful compilation of %SCRATCH%/witness.php]
    /// prompt> [Breakpoint #0 added at foo]
    /// prompt> [Breakpoint #0 in foo() at %SCRATCH%/witness.php:3, hits: 1]
    /// >00003:   return 1;
    ///  00004: }
    ///  00005: class A { function b() { return 2; } }
    /// prompt> [Breakpoint #1 added at foo#0]
    /// prompt> [Breakpoint #2 added at A::b#0]
    /// prompt> [Breakpoint #3 added at foo#0]
    /// [Breakpoint already exists for foo#0]
    /// prompt> [Breakpoint #3 added at ZEND_ECHO]
    /// prompt> ------------------------------------------------
    /// Function Breakpoints:
    /// #0%sfoo
    /// ------------------------------------------------
    /// Opline Breakpoints:
    /// #3%s#%d%s(function breakpoint)
    /// #2%s#%d%s(method breakpoint)
    /// ------------------------------------------------
    /// Function opline Breakpoints:
    /// #1%sfoo opline 0
    /// ------------------------------------------------
    /// Method opline Breakpoints:
    /// #2%sA::b opline 0
    /// ------------------------------------------------
    /// Opcode Breakpoints:
    /// #3%sZEND_ECHO
    /// prompt>
    /// ```
    pub fn print(&self, out: &mut Out) {
        const SEPARATE: &[u8] = b"------------------------------------------------\n";
        let tables: [(&str, Listed); 9] = [
            (
                "File Breakpoints",
                Listed::Kinds(|k| match k {
                    Kind::File { file, line } => Some(format!("{}:{line}", lossy(file))),
                    _ => None,
                }),
            ),
            (
                "Pending File Breakpoints",
                Listed::Kinds(|k| match k {
                    Kind::PendingFile { file, line } => Some(format!("{}:{line}", lossy(file))),
                    _ => None,
                }),
            ),
            (
                "Function Breakpoints",
                Listed::Kinds(|k| match k {
                    Kind::Symbol { name } => Some(lossy(name)),
                    _ => None,
                }),
            ),
            (
                "Method Breakpoints",
                Listed::Kinds(|k| match k {
                    Kind::Method { class, name } => {
                        Some(format!("{}::{}", lossy(class), lossy(name)))
                    }
                    _ => None,
                }),
            ),
            ("Opline Breakpoints", Listed::Oplines),
            (
                "File opline Breakpoints",
                Listed::Kinds(|k| match k {
                    Kind::FileOpline { file, opline } => {
                        Some(format!("{} opline {opline}", lossy(file)))
                    }
                    _ => None,
                }),
            ),
            (
                "Function opline Breakpoints",
                Listed::Kinds(|k| match k {
                    Kind::FunctionOpline { name, opline } => {
                        Some(format!("{} opline {opline}", lossy(name)))
                    }
                    _ => None,
                }),
            ),
            (
                "Method opline Breakpoints",
                Listed::Kinds(|k| match k {
                    Kind::MethodOpline {
                        class,
                        name,
                        opline,
                    } => Some(format!("{}::{} opline {opline}", lossy(class), lossy(name))),
                    _ => None,
                }),
            ),
            (
                "Opcode Breakpoints",
                Listed::Kinds(|k| match k {
                    Kind::Opcode { name } => Some(lossy(name)),
                    _ => None,
                }),
            ),
        ];
        for (title, listed) in tables {
            let rows: Vec<String> = match listed {
                Listed::Oplines if !self.has_oplines => continue,
                Listed::Oplines => self
                    .oplines
                    .iter()
                    .map(|kept| self.opline_row(kept))
                    .collect(),
                Listed::Kinds(row) => self
                    .list
                    .iter()
                    .filter_map(|brake| {
                        row(&brake.kind).map(|text| {
                            let disabled = if brake.disabled { " [disabled]" } else { "" };
                            format!("#{}\t\t{text}{disabled}", brake.id)
                        })
                    })
                    .collect(),
            };
            if rows.is_empty() && !matches!(listed, Listed::Oplines) {
                continue;
            }
            out.out(SEPARATE);
            out.out(format!("{title}:\n").as_bytes());
            for line in rows {
                out.writeln(line.as_bytes());
            }
        }
    }

    /// One row of the opline table as `info break` writes it: a resolved
    /// row never disabled, since the breakpoint php disables is the one
    /// it was resolved from, and a row set on the opline itself as its
    /// breakpoint stands.
    fn opline_row(&self, kept: &Kept) -> String {
        let Kept { address, id, from } = *kept;
        if let Some(table) = from {
            return format!("#{id}\t\t#{address}\t\t({} breakpoint)", table.word());
        }
        let disabled = self
            .list
            .iter()
            .any(|brake| brake.id == id && brake.disabled);
        let disabled = if disabled { " [disabled]" } else { "" };
        format!("#{id}\t\t#{address}{disabled}")
    }

    /// `phpdbg_export_breakpoints_to_string`: a `break` line for each
    /// breakpoint in the order of its number, the file a pending line
    /// names as it was named, and none for an opline php keys by its
    /// address, which no line names; none where no line is written. The
    /// next cycle runs them into tables that start empty, so a deleted
    /// breakpoint leaves no gap in the numbers.
    ///
    /// ```php
    /// function foo() { echo "foo\n"; }
    /// function bar() { echo "bar\n"; }
    /// foo();
    /// bar();
    /// ```
    /// ```request phpdbg
    /// b foo
    /// b bar
    /// b del 0
    /// r
    /// c
    /// b ZEND_ECHO
    /// b bar
    /// b foo
    /// q
    /// ```
    /// ```expect
    /// [Successful compilation of %SCRATCH%/witness.php]
    /// prompt> [Breakpoint #0 added at foo]
    /// prompt> [Breakpoint #1 added at bar]
    /// prompt> [Deleted breakpoint #0]
    /// prompt> foo
    /// [Breakpoint #1 in bar() at %SCRATCH%/witness.php:3, hits: 1]
    /// >00003: function bar() { echo "bar\n"; }
    ///  00004: foo();
    ///  00005: bar();
    /// prompt> bar
    /// [Script ended normally]
    /// prompt> [Breakpoint #1 added at ZEND_ECHO]
    /// prompt> [Breakpoint exists at bar]
    /// prompt> [Breakpoint #2 added at foo]
    /// prompt>
    /// ```
    pub fn export(&self) -> Option<Vec<u8>> {
        let mut text = Vec::new();
        for brake in &self.list {
            let line: Vec<u8> = match &brake.kind {
                Kind::File { file, line } | Kind::PendingFile { file, line } => {
                    [&quoted(file)[..], format!(":{line}").as_bytes()].concat()
                }
                Kind::FileOpline { file, opline } => {
                    [&quoted(file)[..], format!(":#{opline}").as_bytes()].concat()
                }
                Kind::Symbol { name } | Kind::Opcode { name } => name.clone(),
                Kind::Method { class, name } => [&class[..], b"::", name].concat(),
                Kind::MethodOpline {
                    class,
                    name,
                    opline,
                } => [&class[..], b"::", name, format!("#{opline}").as_bytes()].concat(),
                Kind::FunctionOpline { name, opline } => {
                    [&name[..], format!("#{opline}").as_bytes()].concat()
                }
                Kind::Opline { .. } => continue,
            };
            text.extend_from_slice(b"break ");
            text.extend_from_slice(&line);
            text.push(b'\n');
        }
        (!text.is_empty()).then_some(text)
    }
}

impl At<'_> {
    /// The op array the loop stands in.
    pub fn op_array(&self) -> OpArray<'_> {
        OpArray {
            file: self.file,
            function: self.function,
            class: self.class,
        }
    }
}

/// The number php prints for an opline, its address: here one read off
/// the op array's file, function and class and the opline's number, the
/// same for the same opline and apart for two. Two oplines' 64-bit hashes
/// can in principle collide, which would read as a false "Breakpoint
/// exists"; at phpdbg's scale that is accepted.
pub fn address(op: OpArray<'_>, opline: u32) -> u64 {
    // FNV-1a over the four, as a pointer is one number per opline.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let parts = [
        op.file,
        op.function.unwrap_or_default(),
        op.class.unwrap_or_default(),
        &opline.to_le_bytes(),
    ];
    for part in parts {
        for byte in part.iter().chain(b"\0") {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash >> 1
}
