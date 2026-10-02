//! Breakpoints, as `Z/sapi/phpdbg/phpdbg_bp.c` keeps them: numbered in
//! the order they were asked for, one count shared by every kind, each
//! with its hits; found at an opline in php's order (a file's line, a
//! function's or a method's first body opline, an opline of a named op
//! array, an opcode); printed as php prints them when hit and when
//! listed.

use crate::out;

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

/// The breakpoints of the session.
#[derive(Debug, Default, Clone)]
pub struct Breakpoints {
    pub list: Vec<Break>,
    /// `PHPDBG_G(bp_count)`.
    pub count: u32,
    /// `PHPDBG_IS_BP_ENABLED`.
    pub disabled: bool,
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

    /// A breakpoint added under `id`.
    pub fn push(&mut self, id: u32, kind: Kind) {
        self.list.push(Break {
            id,
            kind,
            hits: 0,
            disabled: false,
        });
    }

    fn has(&self, test: impl Fn(&Kind) -> bool) -> bool {
        self.list.iter().any(|brake| test(&brake.kind))
    }

    /// `phpdbg_set_breakpoint_file` past its path resolution: `file` the
    /// path a compiled file carries, or none where the path named no
    /// compiled file and is left pending under the name as given.
    pub fn set_file(&mut self, file: Option<Vec<u8>>, named: &[u8], line: i64) {
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
            out::error(format!("Breakpoint at {}:{line} exists", lossy(file)).as_bytes());
            return;
        }
        let id = self.next_id();
        let message = if pending {
            format!("Pending breakpoint #{id} added at {}:{line}", lossy(file))
        } else {
            format!("Breakpoint #{id} added at {}:{line}", lossy(file))
        };
        self.push(id, kind);
        out::notice(message.as_bytes());
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
    pub fn set_symbol(&mut self, name: &[u8]) {
        let name = name.strip_prefix(b"\\").unwrap_or(name);
        if self.has(|k| matches!(k, Kind::Symbol { name: n } if n == name)) {
            out::error(format!("Breakpoint exists at {}", lossy(name)).as_bytes());
            return;
        }
        let id = self.next_id();
        self.push(
            id,
            Kind::Symbol {
                name: name.to_vec(),
            },
        );
        out::notice(format!("Breakpoint #{id} added at {}", lossy(name)).as_bytes());
    }

    /// `phpdbg_set_breakpoint_method`.
    pub fn set_method(&mut self, class: &[u8], name: &[u8]) {
        let class = class.strip_prefix(b"\\").unwrap_or(class);
        if self.has(
            |k| matches!(k, Kind::Method { class: c, name: n } if same(c, class) && same(n, name)),
        ) {
            out::error(
                format!("Breakpoint exists at {}::{}", lossy(class), lossy(name)).as_bytes(),
            );
            return;
        }
        let id = self.next_id();
        self.push(
            id,
            Kind::Method {
                class: class.to_vec(),
                name: name.to_vec(),
            },
        );
        out::notice(
            format!(
                "Breakpoint #{id} added at {}::{}",
                lossy(class),
                lossy(name)
            )
            .as_bytes(),
        );
    }

    /// `phpdbg_set_breakpoint_opcode`.
    pub fn set_opcode(&mut self, name: &[u8]) {
        if self.has(|k| matches!(k, Kind::Opcode { name: n } if n == name)) {
            out::error(format!("Breakpoint exists for {}", lossy(name)).as_bytes());
            return;
        }
        let id = self.next_id();
        self.push(
            id,
            Kind::Opcode {
                name: name.to_vec(),
            },
        );
        out::notice(format!("Breakpoint #{id} added at {}", lossy(name)).as_bytes());
    }

    /// `phpdbg_set_breakpoint_opline_ex`: the opline `opline` of the op
    /// array `at` names (its file, function and class), numbered as php
    /// numbers it by address.
    pub fn set_opline(&mut self, at: &At<'_>, opline: u32) {
        let address = address(at, opline);
        if self.has(|k| matches!(k, Kind::Opline { address: a, .. } if *a == address)) {
            out::error(format!("Breakpoint exists for opline #{address}").as_bytes());
            return;
        }
        let id = self.next_id();
        self.push(
            id,
            Kind::Opline {
                file: at.file.to_vec(),
                function: at.function.map(<[u8]>::to_vec),
                class: at.class.map(<[u8]>::to_vec),
                opline,
                address,
            },
        );
        out::notice(format!("Breakpoint #{id} added at #{address}").as_bytes());
    }

    /// `phpdbg_clear()`'s `zend_hash_clean` of every table but the
    /// opcodes', the count left as it stands.
    pub fn clean(&mut self) {
        self.list
            .retain(|brake| matches!(brake.kind, Kind::Opcode { .. }));
    }

    /// `phpdbg_delete_breakpoint`.
    pub fn delete(&mut self, id: i64) {
        match self.list.iter().position(|brake| i64::from(brake.id) == id) {
            Some(at) => {
                self.list.remove(at);
                out::notice(format!("Deleted breakpoint #{id}").as_bytes());
            }
            None => out::error(format!("Failed to find breakpoint #{id}").as_bytes()),
        }
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
                    Kind::Opline { address, .. } => *address == self::address(at, at.opline),
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
    pub fn hit(&mut self, found: usize, at: &At<'_>) {
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
        out::notice(message.as_bytes());
    }

    /// `phpdbg_clear_breakpoints`, after `clear`'s count of each table.
    pub fn clear(&mut self) {
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
        out::out(b"Clearing Breakpoints\n");
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
            out::writeln(format!("{label}{n}").as_bytes());
        }
        self.list.clear();
        self.count = 0;
    }

    /// `info break`: each table in `phpdbg_info_break`'s order, a
    /// separator and its title before a table with rows.
    pub fn print(&self) {
        const SEPARATE: &[u8] = b"------------------------------------------------\n";
        let tables: [(&str, Row); 9] = [
            ("File Breakpoints", |k| match k {
                Kind::File { file, line } => Some(format!("{}:{line}", lossy(file))),
                _ => None,
            }),
            ("Pending File Breakpoints", |k| match k {
                Kind::PendingFile { file, line } => Some(format!("{}:{line}", lossy(file))),
                _ => None,
            }),
            ("Function Breakpoints", |k| match k {
                Kind::Symbol { name } => Some(lossy(name)),
                _ => None,
            }),
            ("Method Breakpoints", |k| match k {
                Kind::Method { class, name } => Some(format!("{}::{}", lossy(class), lossy(name))),
                _ => None,
            }),
            ("Opline Breakpoints", |k| match k {
                Kind::Opline { address, .. } => Some(format!("#{address}")),
                _ => None,
            }),
            ("File opline Breakpoints", |k| match k {
                Kind::FileOpline { file, opline } => {
                    Some(format!("{} opline {opline}", lossy(file)))
                }
                _ => None,
            }),
            ("Function opline Breakpoints", |k| match k {
                Kind::FunctionOpline { name, opline } => {
                    Some(format!("{} opline {opline}", lossy(name)))
                }
                _ => None,
            }),
            ("Method opline Breakpoints", |k| match k {
                Kind::MethodOpline {
                    class,
                    name,
                    opline,
                } => Some(format!("{}::{} opline {opline}", lossy(class), lossy(name))),
                _ => None,
            }),
            ("Opcode Breakpoints", |k| match k {
                Kind::Opcode { name } => Some(lossy(name)),
                _ => None,
            }),
        ];
        for (title, row) in tables {
            let rows: Vec<String> = self
                .list
                .iter()
                .filter_map(|brake| {
                    row(&brake.kind).map(|text| {
                        let disabled = if brake.disabled { " [disabled]" } else { "" };
                        format!("#{}\t\t{text}{disabled}", brake.id)
                    })
                })
                .collect();
            if rows.is_empty() {
                continue;
            }
            out::out(SEPARATE);
            out::out(format!("{title}:\n").as_bytes());
            for line in rows {
                out::writeln(line.as_bytes());
            }
        }
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

    /// Whether a line waits on a file not compiled yet.
    pub fn has_pending(&self) -> bool {
        self.has(|k| matches!(k, Kind::PendingFile { .. }))
    }

    /// `phpdbg_set_breakpoint_file_opline`: the opline `opline` of the
    /// file's own code, pending until the file stands.
    pub fn set_file_opline(&mut self, file: &[u8], opline: u32) {
        let id = self.next_id();
        if self.has(
            |k| matches!(k, Kind::FileOpline { file: f, opline: o } if f == file && *o == opline),
        ) {
            out::error(
                format!("Breakpoint already exists for {}:#{opline}", lossy(file)).as_bytes(),
            );
            self.count -= 1;
            return;
        }
        out::notice(format!("Pending breakpoint #{id} at {}:{opline}", lossy(file)).as_bytes());
        self.push(
            id,
            Kind::FileOpline {
                file: file.to_vec(),
                opline,
            },
        );
    }
}

/// The number php prints for an opline, its address: here one read off
/// the op array's file, function and class and the opline's number, the
/// same for the same opline and apart for two.
fn address(at: &At<'_>, opline: u32) -> u64 {
    // FNV-1a over the four, as a pointer is one number per opline.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let parts = [
        at.file,
        at.function.unwrap_or_default(),
        at.class.unwrap_or_default(),
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
