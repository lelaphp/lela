//! The tier's SSA dumps held to the oracle's: each program of
//! `types/goldens` served under the settings `capture.sh` ran php with,
//! and each op array's dump compared with the one php wrote,
//! `goldens/<program>.ssa`, with the types and ranges inference gave
//! each variable. An op array with a call is compared as php knows it
//! before inference: php types what a call returns by what it knows of the
//! callee (`zend_get_func_info`), which the analysis does not read yet
//! (#3574).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::testing::{self, Lane, goldens};

/// The settings `capture.sh` runs php with that the tier reads, and the
/// compile inline, so the dumps are written in the request.
const SSA: [(&str, &str); 14] = [
    ("opcache.file_update_protection", "0"),
    ("opcache.enable_cli", "1"),
    ("opcache.jit", "tracing"),
    ("opcache.jit_buffer_size", "64M"),
    ("opcache.jit_hot_loop", "1"),
    ("opcache.jit_hot_func", "1"),
    ("opcache.jit_hot_return", "1"),
    ("opcache.jit_hot_side_exit", "1"),
    ("opcache.jit_max_root_traces", "100000"),
    ("opcache.jit_max_side_traces", "100000"),
    ("opcache.jit_max_exit_counters", "100000"),
    ("opcache.jit_debug", "258050"),
    ("html_errors", "0"),
    ("jit.compile", "inline"),
];

/// Where the oracle's dumps are.
fn oracle() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/analysis/goldens")
}

/// `line` without the number php's dump has been seen to print before
/// a `ref` among an array's element types, `array of [any, 893 ref]`,
/// which changes from one serving to the next: the one byte pattern dropped, digits
/// and a space after `, ` where `ref]` follows.
fn stray_digits(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(", ") {
        let (head, tail) = rest.split_at(at + 2);
        out.push_str(head);
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        rest = match tail.get(digits..) {
            Some(after) if digits > 0 && after.starts_with(" ref]") => {
                after.get(1..).unwrap_or_default()
            }
            _ => tail,
        };
    }
    out.push_str(rest);
    out
}

/// The dumps of a `jit_debug` stream, each op array's once, the file of
/// each named by `file` alone.
fn dumps(stream: &str, file: &str) -> BTreeSet<String> {
    let lines: Vec<&str> = stream.lines().collect();
    let mut dumps = BTreeSet::new();
    let mut current: Option<Vec<String>> = None;
    let mut close = |current: &mut Option<Vec<String>>| {
        if let Some(mut dump) = current.take() {
            while dump.last().is_some_and(String::is_empty) {
                dump.pop();
            }
            let dump: Vec<String> = dump.iter().map(|line| stray_digits(line)).collect();
            dumps.insert(dump.join("\n"));
        }
    };
    for (at, line) in lines.iter().enumerate() {
        let opens = line.ends_with(':')
            && lines
                .get(at + 1)
                .is_some_and(|next| next.starts_with("     ; (lines="));
        if opens {
            close(&mut current);
            current = Some(Vec::new());
        } else if line.starts_with("---- ")
            || line.starts_with("     TRACE ")
            || line.starts_with("JIT")
        {
            close(&mut current);
            continue;
        }
        if let Some(dump) = &mut current {
            let named = match line
                .strip_prefix("     ; ")
                .and_then(|l| l.rsplit_once(':'))
            {
                Some((path, lines)) if Path::new(path).extension().is_some_and(|e| e == "php") => {
                    format!("     ; {file}:{lines}")
                }
                _ => (*line).to_owned(),
            };
            dump.push(named);
        }
    }
    close(&mut current);
    dumps
}

#[test]
fn stray_digits_before_a_ref_go() {
    assert_eq!(
        stray_digits("     ; return  [rcn, array of [any, 893 ref]] RANGE[0..0]"),
        "     ; return  [rcn, array of [any, ref]] RANGE[0..0]"
    );
}

/// The seven kernels of `docs/perf/workloads`, each with the size its
/// `@bench-param` line is set to, as `capture.sh` sets it: the analysis
/// is of the op arrays, which the size does not change.
const KERNELS: [(&str, u32); 7] = [
    ("fib", 10),
    ("nbody", 100),
    ("spectralnorm", 10),
    ("mandelbrot", 20),
    ("binary_trees", 6),
    ("fannkuch", 6),
    ("string_munge", 200),
];

/// `program`, less its open tag, with its `@bench-param` line's size set
/// to `n`.
fn sized(program: &str, n: u32) -> String {
    let program = program.strip_prefix("<?php\n").unwrap_or(program);
    program
        .split('\n')
        .map(|line| match line.split_once("; // @bench-param") {
            Some((assign, rest)) if assign.starts_with("$N = ") => {
                format!("$N = {n}; // @bench-param{rest}")
            }
            _ => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The dumps of `stream`, the tier's for `name.php`, held to the
/// oracle's at `golden`; the difference, where there is one.
fn held(name: &str, stream: &[u8], golden: &Path) -> Result<Option<String>, String> {
    let file = format!("{name}.php");
    let php = std::fs::read_to_string(golden).map_err(|e| format!("{}: {e}", golden.display()))?;
    let engine = dumps(&String::from_utf8_lossy(stream), &file);
    let php = dumps(&php, &file);
    if php.is_empty() {
        return Err(format!("{name}: php dumped nothing"));
    }
    if engine == php {
        return Ok(None);
    }
    let only = |a: &BTreeSet<String>, b: &BTreeSet<String>| {
        a.difference(b).cloned().collect::<Vec<_>>().join("\n\n")
    };
    Ok(Some(format!(
        "{name}\n--- php alone\n{}\n--- the tier alone\n{}",
        only(&php, &engine),
        only(&engine, &php)
    )))
}

/// Each op array a trace of a golden program passes through is dumped as
/// php dumps it: its blocks, their marks, edges, dominators and loops, its
/// phis and pis, and its oplines as the SSA names their variables.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn each_op_array_a_trace_passes_is_dumped_as_php_dumps_it() -> Result<(), String> {
    let mut programs = Vec::new();
    for dir in [goldens::types(), oracle()] {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        programs.extend(
            entries
                .filter_map(|entry| Some(entry.ok()?.path()))
                .filter(|path| path.extension().is_some_and(|e| e == "php")),
        );
    }
    programs.sort();
    let mut differ = Vec::new();
    for path in programs {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let program = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let program = program.strip_prefix("<?php\n").unwrap_or(&program);
        let served = testing::serve::<Lane>(&SSA, program);
        let golden = oracle().join(format!("{name}.ssa"));
        differ.extend(held(name, &served.stderr, &golden)?);
    }
    if differ.is_empty() {
        Ok(())
    } else {
        Err(differ.join("\n\n"))
    }
}

/// The seven kernels' op arrays the traces pass through are dumped as php
/// dumps them under `[regime.jit-ssa]` (#3572), served by the command
/// line `PHP_SSA_ENGINE` names, built with the tier:
/// `cargo build -p php_sapi_cli --bin php --features php_bundle/jit`.
/// A kernel calls the bundle's natives, which a test's engine here does
/// not link, and php compiles a call to a native it knows otherwise.
#[test]
#[ignore = "serves the kernels through the command line built with the tier, which PHP_SSA_ENGINE names"]
fn the_seven_kernels_are_analysed_as_php_analyses_them() -> Result<(), String> {
    let engine = std::env::var_os("PHP_SSA_ENGINE").ok_or("PHP_SSA_ENGINE names no binary")?;
    let workloads = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/perf/workloads");
    let scratch = testing::Scratch::new();
    let mut differ = Vec::new();
    for (name, n) in KERNELS {
        let path = workloads.join(format!("{name}.php"));
        let program =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file = scratch.0.join(format!("{name}.php"));
        std::fs::write(&file, format!("<?php\n{}", sized(&program, n)))
            .map_err(|e| e.to_string())?;
        let mut command = std::process::Command::new(&engine);
        command
            .env_clear()
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .arg("-n");
        for (entry, value) in SSA {
            command.arg("-d").arg(format!("{entry}={value}"));
        }
        let served = command.arg(&file).output().map_err(|e| e.to_string())?;
        let golden = oracle().join(format!("kernels/{name}.ssa"));
        differ.extend(held(name, &served.stderr, &golden)?);
    }
    if differ.is_empty() {
        Ok(())
    } else {
        Err(differ.join("\n\n"))
    }
}

/// php analyses the op arrays a trace passes through from
/// `ZEND_JIT_LEVEL_OPT_FUNC` alone (`zend_jit_trace_build_tssa`): with
/// `opcache.jit`'s `O` digit at 2 the trace compiles and nothing is
/// dumped.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn below_the_function_level_nothing_is_analysed() {
    let program = "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }\necho f(20), \"\\n\";";
    let mut ini = SSA.to_vec();
    ini.push(("opcache.jit", "1252"));
    let low = testing::serve::<Lane>(&ini, program);
    let traced = testing::serve::<Lane>(&SSA, program);
    let (low, traced) = (
        String::from_utf8_lossy(&low.stderr),
        String::from_utf8_lossy(&traced.stderr),
    );
    assert!(traced.contains("     ; (lines="), "{traced}");
    assert!(low.contains("compiled"), "{low}");
    assert!(!low.contains("     ; (lines="), "{low}");
}

/// A trace through more than `ZEND_JIT_TRACE_MAX_FUNCS` op arrays is not
/// compiled, as php's `zend_jit_trace_build_tssa` fails on the
/// thirty-first: php aborts this loop, which calls thirty-one functions,
/// with `JIT compilation error` after dumping the first thirty, its file's
/// own code among them.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_trace_through_more_than_thirty_op_arrays_is_not_compiled() -> Result<(), std::fmt::Error> {
    let mut functions = String::new();
    let mut calls = String::new();
    for i in 0..31 {
        writeln!(functions, "function f{i}($x) {{ return $x + {i}; }}")?;
        writeln!(calls, "    $s += f{i}($i);")?;
    }
    let program =
        format!("{functions}$s = 0;\nfor ($i = 0; $i < 5; $i++) {{\n{calls}}}\necho $s, \"\\n\";");
    let served = testing::serve::<Lane>(&SSA, &program);
    let stderr = String::from_utf8_lossy(&served.stderr);
    let first = stderr
        .split("---- TRACE 1 abort (JIT compilation error)")
        .next()
        .and_then(|before| before.rsplit("---- TRACE 1 stop (loop)").next())
        .unwrap_or_default();
    assert!(
        stderr.contains("---- TRACE 1 abort (JIT compilation error)"),
        "{stderr}"
    );
    assert_eq!(first.matches("     ; (lines=").count(), 30, "{first}");
    Ok(())
}
