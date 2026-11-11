//! What a run holds besides its corpus: programs the inherited grammar
//! generator builds (design §8, taken from
//! Mahout's `mahout_fuzz_gen` as design §13 lists it), and programs one
//! to three mutations make of a corpus program. Every program is a
//! function of the run's seed and its place in the run, so a finding is
//! named by the two and runs again from them.
//!
//! The generator builds by construction, so each program it emits parses:
//! hoisted functions, assignments, echoes, bounded loops, `foreach` over a
//! literal, `try`/`catch`/`finally`, element writes through a copy and
//! string offsets, over literals weighted toward the traps php's key
//! normalisation and loose comparison set (`"08"`, `"8"`, `"-0"`, `" 1"`).
//! A mutation works on lines, the unit a generated program is written in
//! and the one [`super`]'s minimiser removes: it drops, repeats or swaps a
//! line, puts a generated statement before one, or puts a trap value in
//! place of an integer literal in one.

use std::fmt::Write as _;

/// The deterministic generator every program of a run is drawn from:
/// xorshift64*, seeded through splitmix64 so that neighbouring seeds draw
/// unrelated programs.
pub(crate) struct Rng(u64);

impl Rng {
    /// The generator for `seed`.
    pub(crate) fn new(seed: u64) -> Rng {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Rng((z ^ (z >> 31)) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n`, `n` above 0.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n.max(1) as u64).unwrap_or_default()
    }

    /// True one time in `den`.
    fn one_in(&mut self, den: usize) -> bool {
        self.below(den) == 0
    }

    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.below(from.len())]
    }
}

/// How deep an expression nests, so a program stays small enough to read.
const DEPTH: u32 = 4;

/// The values a mutation puts in place of an integer literal.
const TRAPS: [&str; 10] = [
    "0",
    "-1",
    "PHP_INT_MAX",
    "PHP_INT_MIN",
    "\"08\"",
    "\" 1\"",
    "1.5",
    "null",
    "[]",
    "\"\"",
];

/// Where the variables of a statement a mutation puts into a corpus
/// program are numbered from, past any `$v<n>` a program of the corpus
/// names.
const SPLICED: usize = 100;

/// One program the grammar generator builds from `rng`.
pub(crate) fn program(rng: &mut Rng) -> String {
    let mut g = Gen::new(rng, 0);
    for _ in 0..g.rng.below(3) {
        g.function();
    }
    for _ in 0..3 + g.rng.below(6) {
        g.statement();
    }
    g.out.push_str("echo \"|end\";\n");
    format!("<?php\n{}{}", g.functions, g.out)
}

/// `code` after one to three line mutations drawn from `rng`; the opening
/// tag line is kept where it is.
pub(crate) fn mutated(code: &str, rng: &mut Rng) -> String {
    let mut lines: Vec<String> = code.lines().map(str::to_owned).collect();
    let head = usize::from(lines.first().is_some_and(|l| l.starts_with("<?php")));
    for _ in 0..=rng.below(3) {
        let body = lines.len().saturating_sub(head);
        let at = head + rng.below(body.max(1));
        match rng.below(5) {
            0 if body > 1 => {
                lines.remove(at);
            }
            1 if body > 0 => lines.insert(at, lines[at].clone()),
            2 if body > 1 => lines.swap(at, head + rng.below(body)),
            3 => {
                let mut g = Gen::new(rng, SPLICED);
                g.statement();
                lines.insert(at.min(lines.len()), g.out.trim_end().to_owned());
            }
            _ if body > 0 => lines[at] = trapped(&lines[at], rng),
            _ => {}
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `line` with one of its integer literals, a run of digits that no
/// letter, digit, `$`, `_`, `.` or quote stands against, put out for a
/// [`TRAPS`] value; the line unchanged where it has none.
fn trapped(line: &str, rng: &mut Rng) -> String {
    let bytes = line.as_bytes();
    let joins = |b: u8| b.is_ascii_alphanumeric() || b"$_.\"'".contains(&b);
    let mut runs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let before = start.checked_sub(1).map(|b| bytes[b]);
        if !before.is_some_and(joins) && !bytes.get(i).copied().is_some_and(joins) {
            runs.push(start..i);
        }
    }
    if runs.is_empty() {
        return line.to_owned();
    }
    let run = runs.swap_remove(rng.below(runs.len()));
    format!(
        "{}{}{}",
        &line[..run.start],
        rng.pick(&TRAPS),
        &line[run.end..]
    )
}

/// One program under construction: its statements, the functions it
/// declares ahead of them, and the variables and functions a later
/// statement may name.
struct Gen<'a> {
    rng: &'a mut Rng,
    out: String,
    functions: String,
    depth: u32,
    vars: Vec<String>,
    fns: Vec<String>,
    /// Where this generator's variable names start, so a statement put
    /// into a corpus program names none of the program's own.
    base: usize,
}

impl<'a> Gen<'a> {
    fn new(rng: &'a mut Rng, base: usize) -> Gen<'a> {
        Gen {
            rng,
            out: String::new(),
            functions: String::new(),
            depth: 0,
            vars: Vec::new(),
            fns: Vec::new(),
            base,
        }
    }

    fn fresh(&mut self) -> String {
        let name = format!("v{}", self.base + self.vars.len());
        self.vars.push(name.clone());
        name
    }

    fn some_var(&mut self) -> Option<String> {
        (!self.vars.is_empty()).then(|| self.vars[self.rng.below(self.vars.len())].clone())
    }

    /// An integer literal in `-span..span`.
    fn int(&mut self, span: usize) -> String {
        let n = self.rng.below(2 * span);
        if n < span {
            format!("-{}", span - n)
        } else {
            (n - span).to_string()
        }
    }

    fn literal(&mut self) -> String {
        match self.rng.below(8) {
            0 => self.int(1000),
            1 => "\"a\"".into(),
            2 => format!("\"s{}\"", self.rng.below(10)),
            3 => "true".into(),
            4 => "false".into(),
            5 => "null".into(),
            6 => self.rng.pick(&["1.5", "2.25", "0.5", "-3.5"]).into(),
            _ => self
                .rng
                .pick(&["\"08\"", "\"8\"", "\"-0\"", "\" 1\""])
                .into(),
        }
    }

    fn function(&mut self) {
        let name = format!("f{}", self.fns.len());
        let params: Vec<String> = (0..self.rng.below(3)).map(|i| format!("$p{i}")).collect();
        let mut signature = params.clone();
        if let Some(last) = signature.last_mut()
            && self.rng.one_in(2)
        {
            let default = self.literal();
            write!(last, " = {default}").ok();
        }
        let body = match params.len() {
            0 => self.literal(),
            n => {
                let p = params[self.rng.below(n)].clone();
                format!("{p} . \"-\" . {}", self.literal())
            }
        };
        writeln!(
            self.functions,
            "function {name}({}) {{ return {body}; }}",
            signature.join(", ")
        )
        .ok();
        self.fns.push(name);
    }

    fn expr(&mut self) -> String {
        if self.depth >= DEPTH {
            return self.literal();
        }
        self.depth += 1;
        let e = match self.rng.below(14) {
            0 | 1 => self.literal(),
            2 => self
                .some_var()
                .map_or_else(|| self.literal(), |v| format!("${v}")),
            3 => format!("({} + {})", self.int_expr(), self.int_expr()),
            4 => format!("({} . {})", self.expr(), self.expr()),
            5 => format!("({} == {})", self.expr(), self.expr()),
            6 => format!("({} === {})", self.expr(), self.expr()),
            7 => format!("({} ?? {})", self.expr(), self.expr()),
            8 => format!("({} ? {} : {})", self.expr(), self.expr(), self.expr()),
            9 => self.array(),
            10 => self.call(),
            11 => self.closure(),
            12 => format!("({}[{}] ?? \"miss\")", self.array(), self.literal()),
            _ => {
                let [s, a, b, r1, r2, d] = std::array::from_fn(|_| self.literal());
                format!("match ({s}) {{ {a} => {r1}, {b} => {r2}, default => {d} }}")
            }
        };
        self.depth -= 1;
        e
    }

    fn int_expr(&mut self) -> String {
        if self.depth >= DEPTH || !self.rng.one_in(3) {
            return self.int(100);
        }
        self.depth += 1;
        let e = format!("({} * {})", self.int_expr(), self.int_expr());
        self.depth -= 1;
        e
    }

    fn array(&mut self) -> String {
        let items: Vec<String> = (0..self.rng.below(4))
            .map(|_| {
                if self.rng.one_in(2) {
                    format!("{} => {}", self.literal(), self.literal())
                } else {
                    self.literal()
                }
            })
            .collect();
        format!("[{}]", items.join(", "))
    }

    fn call(&mut self) -> String {
        if self.fns.is_empty() {
            return self.literal();
        }
        let f = self.fns[self.rng.below(self.fns.len())].clone();
        let [a, b, c] = std::array::from_fn(|_| self.literal());
        format!("{f}({a}, {b}, {c})")
    }

    fn closure(&mut self) -> String {
        let lit = self.literal();
        match self.some_var() {
            Some(v) if self.rng.one_in(2) => {
                format!("(function ($q) use (${v}) {{ return $q . ${v}; }})({lit})")
            }
            Some(v) => format!("(fn($q) => $q . ${v})({lit})"),
            None => format!("(function ($q) {{ return $q; }})({lit})"),
        }
    }

    fn statement(&mut self) {
        let line = match self.rng.below(10) {
            0..=2 => {
                let e = self.expr();
                format!("${} = {e};", self.fresh())
            }
            3 | 4 => {
                let e = self.expr();
                let t = self.fresh();
                format!("${t} = {e}; echo \"[\", ${t} === null ? \"N\" : ${t}, \"]\";")
            }
            5 => {
                let (a, k, v) = (self.array(), self.fresh(), self.fresh());
                format!(
                    "foreach ({a} as ${k} => ${v}) {{ echo \"<\", ${k}, \":\", ${v} === null ? \"N\" : ${v}, \">\"; }}"
                )
            }
            6 => {
                let (i, n) = (self.fresh(), 1 + self.rng.below(4));
                format!("${i} = 0; while (${i} < {n}) {{ echo \"w\", ${i}; ${i} = ${i} + 1; }}")
            }
            7 => {
                let thrown = self.rng.pick(&["true", "false"]);
                let m = self.rng.below(100);
                format!(
                    "try {{ if ({thrown}) {{ throw new Exception(\"e{m}\"); }} echo \"t\"; }} catch (Exception $ex) {{ echo \"c\"; }} finally {{ echo \"f\"; }}"
                )
            }
            8 => {
                let (a, b) = (self.fresh(), self.fresh());
                let k = self.rng.pick(&["3", "\"a\"", "\"08\"", "\"8\""]);
                let (init, val) = (self.array(), self.literal());
                format!(
                    "${a} = {init}; ${b} = ${a}; ${b}[{k}] = {val}; echo isset(${a}[{k}]) ? \"y\" : \"n\", isset(${b}[{k}]) ? \"y\" : \"n\";"
                )
            }
            _ => {
                let s = self.fresh();
                let at = self.rng.below(8);
                format!("${s} = \"abcdef\"; ${s}[{at}] = \"Z\"; echo ${s};")
            }
        };
        self.out.push_str(&line);
        self.out.push('\n');
    }
}
