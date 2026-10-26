//! The tier's SSA dumps held to the oracle's: each program of
//! `types/goldens` served under the settings `capture.sh` ran php with,
//! and each op array's dump compared, as far as php knows it before type
//! inference, with the one php wrote, `goldens/<program>.ssa`.

use std::collections::BTreeSet;
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

/// The length of the SSA variable `line` names at its start, `#3.CV0($n)`,
/// `#7.T2`, `#?.V4`, with a ` NOVAL` after it.
fn ssa_var(line: &str) -> Option<usize> {
    let rest = line.strip_prefix('#')?;
    let dot = rest.find('.')?;
    let number = rest.get(..dot)?;
    if number != "?" && !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let var = rest.get(dot + 1..)?;
    let len = if var.starts_with("CV") {
        var.find(')')? + 1
    } else if var.starts_with(['T', 'V', 'X']) {
        1 + var.get(1..)?.bytes().take_while(u8::is_ascii_digit).count()
    } else {
        return None;
    };
    let mut end = 1 + dot + 1 + len;
    if line.get(end..)?.starts_with(" NOVAL") {
        end += " NOVAL".len();
    }
    Some(end)
}

/// What inference prints after a variable at the start of `rest`: its
/// types, ` [long]`, and its range, ` RANGE[MIN..MAX]`; their length.
fn inferred(rest: &str) -> usize {
    let mut at = 0;
    if rest.starts_with(" [") {
        let mut depth = 0;
        for (i, b) in rest.bytes().enumerate() {
            match b {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        at = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(range) = rest.get(at..).filter(|r| r.starts_with(" RANGE["))
        && let Some(close) = range.find(']')
    {
        at += close + 1;
    }
    at
}

/// `line` as php prints it before inference: no line per CV, and no types
/// or range after a variable or the return; a pi's constraint kept.
fn projected(line: &str) -> Option<String> {
    if line.starts_with("     ; #") && line.contains(".CV") {
        return None;
    }
    let mut out = String::new();
    let mut rest = line;
    while !rest.is_empty() {
        let end = if rest.starts_with("; return ") {
            Some("; return ".len())
        } else {
            ssa_var(rest)
        };
        if let Some(end) = end {
            out.push_str(rest.get(..end).unwrap_or_default());
            let after = rest.get(end..).unwrap_or_default();
            rest = after.get(inferred(after)..).unwrap_or_default();
            continue;
        }
        let mut chars = rest.chars();
        out.extend(chars.next());
        rest = chars.as_str();
    }
    Some(out)
}

/// The dumps of a `jit_debug` stream, each op array's once, projected,
/// the file of each named by `file` alone.
fn dumps(stream: &str, file: &str) -> BTreeSet<String> {
    let lines: Vec<&str> = stream.lines().collect();
    let mut dumps = BTreeSet::new();
    let mut current: Option<Vec<String>> = None;
    let mut close = |current: &mut Option<Vec<String>>| {
        if let Some(mut dump) = current.take() {
            while dump.last().is_some_and(String::is_empty) {
                dump.pop();
            }
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
            dump.extend(projected(&named));
        }
    }
    close(&mut current);
    dumps
}

#[test]
fn projection_drops_what_inference_prints() {
    assert_eq!(
        projected("0001 #2.T1 [bool] = IS_SMALLER #1.CV0($n) [long] RANGE[MIN..MAX] int(2)")
            .as_deref(),
        Some("0001 #2.T1 = IS_SMALLER #1.CV0($n) int(2)")
    );
    assert_eq!(
        projected("     #4.CV0($n) [long] RANGE[MIN..1] = Pi<BB1>(#1.CV0($n) [long, ref] & RANGE[-- .. 1])")
            .as_deref(),
        Some("     #4.CV0($n) = Pi<BB1>(#1.CV0($n) & RANGE[-- .. 1])")
    );
    assert_eq!(
        projected("     ; return  [] RANGE[0..0]").as_deref(),
        Some("     ; return ")
    );
    assert_eq!(projected("     ; #0.CV0($n) NOVAL [undef]"), None);
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
    let mut programs: Vec<PathBuf> = std::fs::read_dir(goldens::types())
        .map_err(|e| e.to_string())?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "php"))
        .collect();
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
