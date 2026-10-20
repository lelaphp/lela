//! Op-array reads for extensions (#1829): an observer reads the op
//! array of a frame it is handed ([`FrameView::op_array`]) or of a
//! function ([`Frames::op_array`]), its oplines' opcodes, lines,
//! operands and jump targets, and the constants they name, as php's
//! coverage tools and profilers read `op_array->opcodes`.
//!
//! What each test pins is php's own: the executable lines are what the
//! oracle's `phpdbg -qrr` answers to `phpdbg_get_executable(["functions"
//! => true])` over the same file, and the jumps and the names a call
//! opline carries are what the oracle's `php -d opcache.enable_cli=1 -d
//! opcache.opt_debug_level=0x10000 -l` prints for the same function.

use crate::api::abi::{ExtensionBuilder, Observed, ObserverHandlers};
use crate::api::request::{OpArrayView, Opcode, Operand, OplineView, Request};
use crate::engine::Engine;
use crate::test::Capture;

/// The name `phpdbg_get_executable` keys a function by: its own, and a
/// method's as `class::name` with the class lowercased.
fn keyed(observed: &Observed<'_>) -> Option<String> {
    let function = observed.function()?;
    let name =
        String::from_utf8_lossy(&observed.symbols().function_decl(function)?.name.0).into_owned();
    Some(match observed.function_scope() {
        Some(class) => format!(
            "{}::{name}",
            observed.symbols().class_name(class).to_lowercase()
        ),
        None => name,
    })
}

/// `phpdbg_is_ignored_opcode` (`Z/sapi/phpdbg/phpdbg.c:423-433`).
fn ignored(opcode: Opcode) -> bool {
    matches!(
        opcode.name(),
        "ZEND_NOP"
            | "ZEND_OP_DATA"
            | "ZEND_FE_FREE"
            | "ZEND_FREE"
            | "ZEND_ASSERT_CHECK"
            | "ZEND_VERIFY_RETURN_TYPE"
            | "ZEND_DECLARE_CONST"
            | "ZEND_DECLARE_ATTRIBUTED_CONST"
            | "ZEND_DECLARE_CLASS"
            | "ZEND_DECLARE_FUNCTION"
            | "ZEND_DECLARE_CLASS_DELAYED"
            | "ZEND_DECLARE_ANON_CLASS"
            | "ZEND_FAST_RET"
            | "ZEND_TICKS"
            | "ZEND_EXT_STMT"
            | "ZEND_EXT_FCALL_BEGIN"
            | "ZEND_EXT_FCALL_END"
            | "ZEND_BIND_GLOBAL"
            | "ZEND_BIND_INIT_STATIC_OR_JMP"
    )
}

/// `phpdbg_oplog_fill_executable` (`Z/sapi/phpdbg/phpdbg.c:435-469`)
/// over the read op array: past the `RECV*` oplines, which open it one
/// per parameter as php's `num_args` plus the variadic counts them, the
/// line of each opline not ignored, in the order first met, a `NEW`'s
/// constructor call counted with it; the compiler's own `RETURN` at the
/// end left out, which `zend_emit_final_return` lays down with
/// `extended_value` -1 at the end of every op array.
fn executable(code: OpArrayView<'_>) -> Vec<u32> {
    let ops: Vec<OplineView<'_>> = code.oplines().collect();
    let returns = |op: &OplineView<'_>| {
        matches!(
            op.opcode().name(),
            "ZEND_RETURN" | "ZEND_RETURN_BY_REF" | "ZEND_GENERATOR_RETURN"
        )
    };
    let end = ops.len() - usize::from(ops.last().is_some_and(returns));
    let mut at = ops
        .iter()
        .take_while(|op| {
            matches!(
                op.opcode().name(),
                "ZEND_RECV" | "ZEND_RECV_INIT" | "ZEND_RECV_VARIADIC"
            )
        })
        .count();
    let mut lines = Vec::new();
    while at < end {
        let op = &ops[at];
        if !ignored(op.opcode()) {
            if op.opcode().name() == "ZEND_NEW"
                && ops
                    .get(at + 1)
                    .is_some_and(|next| next.opcode().name() == "ZEND_DO_FCALL")
            {
                at += 1;
            }
            if !lines.contains(&op.line()) {
                lines.push(op.line());
            }
        }
        at += 1;
    }
    lines
}

/// A coverage tool's question, asked of each user function on its first
/// call: which of its lines carry an opline phpdbg counts. The lines
/// are written, and the function declined.
#[test]
fn a_functions_executable_lines_are_phpdbgs() {
    fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
        let read = observed.frame().and_then(|frame| frame.op_array());
        if let (Some(name), Some(code)) = (keyed(observed), read) {
            let lines: Vec<String> = executable(code).iter().map(u32::to_string).collect();
            let line = format!("[{name} {}]", lines.join(" "));
            observed.response_mut().write(line.as_bytes());
        }
        ObserverHandlers::default()
    }
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default().observer(init).build("coverage")])
        .build();
    let served = engine.serve_capture(
        "<?php
function shape($n, ...$rest) {
    $total = 0;
    for ($i = 0; $i < $n; $i++) {
        if ($i % 2) {
            $total += $i;
        } else {
            $total -= 1;
        }
    }
    static $calls = 0;
    $calls++;
    $o = new Exception('x', $n);
    try {
        $total += func_num_args() + $o->getCode();
    } finally {
        $total++;
    }
    return $total;
}
function picks(int $x, $y = PHP_INT_SIZE) {
    switch ($x) {
        case 1:
            return 'one';
        case 2:
            return 'two';
    }
    $m = match ($x) {
        3, 4 => 'few',
        default => get_class(new Error('many')),
    };
    try {
        throw new ErrorException($m);
    } catch (ErrorException $e) {
        return $e->getMessage();
    }
}
function counts() {
    foreach ([1, 2] as $v) {
        yield $v;
    }
}
class K {
    public function m(int $a = 1): int
    {
        foreach ([1, 2] as $v) {
            $a += $v;
        }
        return $a;
    }
    public static function s(): string
    {
        return self::class . K::class;
    }
}
echo shape(3, 1, 2), picks(5), (new K)->m(), K::s();
foreach (counts() as $c) echo $c;
echo \"\\n\";
",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "[shape 3 4 5 6 8 11 12 13 15 16 17 19]6\
         [picks 23 25 24 26 28 29 30 33 34 35]Error\
         [k::m 46 47 49]4\
         [k::s 53]KK\
         [counts 38 39 40]12\n"
    );
}

/// One opline as the dump shows what it jumps to and what it names;
/// none for one that does neither.
fn render(code: OpArrayView<'_>, op: &OplineView<'_>) -> Option<String> {
    let named = |operand: Operand| match operand {
        Operand::Const(at) => code
            .constant(at)
            .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        Operand::Cv(at) => code
            .variable(at)
            .map(|name| format!("${}", String::from_utf8_lossy(name))),
        Operand::Unused | Operand::Tmp(_) | Operand::Var(_) => None,
    };
    let opcode = op.opcode().name().trim_start_matches("ZEND_");
    let targets: Vec<String> = op.jump_targets().map(|at| format!("{:04}", at.0)).collect();
    let called: Vec<String> = match opcode {
        "NEW" => named(op.op1()).into_iter().collect(),
        _ if opcode.starts_with("INIT_") && opcode.contains("CALL") => {
            [named(op.op1()), named(op.op2())]
                .into_iter()
                .flatten()
                .collect()
        }
        _ => Vec::new(),
    };
    if targets.is_empty() && called.is_empty() {
        return None;
    }
    let mut line = format!("{:04} {opcode}", op.at().0);
    if !targets.is_empty() {
        line.push_str(" -> ");
        line.push_str(&targets.join(" "));
    }
    for name in called {
        line.push(' ');
        line.push_str(&name);
    }
    Some(line)
}
/// The profiler's initialiser: `route`'s op array listed, and every
/// function declined.
fn profile(observed: &mut Observed<'_>) -> ObserverHandlers {
    if keyed(observed).as_deref() != Some("route") {
        return ObserverHandlers::default();
    }
    let req: &Request = observed;
    let read = observed
        .function()
        .and_then(|function| req.frames().op_array(function));
    if let Some(code) = read {
        let mut lines = vec![format!("{} oplines", code.len())];
        lines.extend(code.oplines().filter_map(|op| render(code, &op)));
        let text = format!("[{}]", lines.join("\n"));
        observed.response_mut().write(text.as_bytes());
    }
    ObserverHandlers::default()
}
/// A profiler's question, asked of one function through
/// [`Frames::op_array`]: how many oplines it holds, where each jump may
/// land, a switch's and a match's tables, a `foreach`'s exits and a
/// `finally`'s call among them, a `CATCH` that is its try's last landing
/// nowhere, and what each call opline names, its function, its class and
/// method, or the compiled variable it calls a method on.
#[test]
fn a_functions_jumps_and_the_names_its_calls_carry_are_its_dumps() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .observer(profile)
            .build("profiler")])
        .build();
    let served = engine.serve_capture(
        "<?php
function route(string $s, int $n) {
    switch ($s) {
        case 'a':
            $n++;
            break;
        case 'b':
            $n--;
            break;
    }
    foreach ([$n] as $k => $v) {
        $n += $k;
    }
    $t = match ($n) { 1, 2 => 'low', 3 => 'mid', default => 'high' };
    try {
        $r = $n > 1 ? new Exception($t, $v) : null;
    } catch (Error $e) {
        $r = null;
    } finally {
        $n++;
    }
    return strcmp($s, 'b') . K::s(...[]) . $r?->getCode() . $r?->getMessage() . $n;
}
class K { static function s() { return '|'; } }
echo route('a', 2), \"\\n\";
",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "[64 oplines
0002 SWITCH_STRING -> 0008 0010 0012
0004 JMPNZ -> 0008
0006 JMPNZ -> 0010
0007 JMP -> 0012
0009 JMP -> 0012
0011 JMP -> 0012
0013 FE_RESET_R -> 0018
0014 FE_FETCH_R -> 0018
0017 JMP -> 0014
0019 MATCH -> 0020 0020 0022 0024
0021 JMP -> 0026
0023 JMP -> 0026
0025 JMP -> 0026
0028 JMPZ -> 0035
0029 NEW Exception
0034 JMP -> 0036
0037 JMP -> 0040
0040 FAST_CALL -> 0042
0041 JMP -> 0044
0044 INIT_FCALL strcmp
0048 INIT_STATIC_METHOD_CALL K s
0053 JMP_NULL -> 0056
0054 INIT_METHOD_CALL $r getCode
0057 JMP_NULL -> 0060
0058 INIT_METHOD_CALL $r getMessage]-1|3mid4\n"
    );
}
