#!/usr/bin/env bash
# census.sh — the Mahout extension-seam census, reproducibly.
#
# Usage: scripts/census.sh <path-to-mahout-tree>
#
# Prints, for the 19 crates/mahout_ext_* crates, every distinct engine item they
# call, with call-site counts, the number of extension crates using each item and,
# on the line under each row, which crates those are (`ext: date spl standard`,
# by the name the crate has under ext/, which is the port map's `ext` column;
# design §4.4), from six sources:
#   1. mahout_value        inherent `pub fn`s on the public value types, matched by
#                          method name on any receiver (see the rule below)
#   2. RequestCtx          inherent `pub fn`s in `impl RequestCtx` blocks, matched on
#                          the receivers Mahout's extensions bind it to:
#                          `ctx.`, `_ctx.`, `self.ctx.`, `.ctx().` and `RequestCtx::`
#   3. semantics::         every `semantics::<name>` path token plus the names a file
#                          imports with `use mahout_runtime::semantics::{..}` (and the
#                          one `semantics::*` glob) called bare in that file
#   4. mahout_native_abi   its public items, matched by name outside `use` lines
#   5. mahout_native_macros the attributes and their arguments (`sig`, `tentative`)
#   6. mahout_linker / mahout_code_meta / mahout_native
#                          every imported or path-qualified item, per crate
#                          (mahout_native is the macros' support crate; it is listed
#                          because 14 crates depend on it)
# plus the other mahout_runtime items extensions import (Reentrant's methods,
# ExtState, EngineBuilder, native_marshal, ...), which the port-map also needs.
#
# The mahout_value rule. Matching is receiver-agnostic (as the r2 census was), so a
# method name that std also defines (`len`, `get`, `as_bytes`, ...) would count every
# `String`/`Vec`/`str` call as a mahout_value call. Two reductions apply:
#   (a) the name set is the inherent `pub fn`s of the *public* value types only
#       (Value, ValueRef, RawValue, ScratchValue, TypeTag, PhpArray, ArrayIter, Key,
#       PhpString, PhpObject, PhpRef, Atom, SortGuard); a name defined only on an
#       internal type (GcNode, HeapHeader, CoroutineChildren, ...) or only in a trait
#       impl (`clone`, `next`, `fmt`, `hash`, ...) is std vocabulary from an
#       extension's point of view and is not counted;
#   (b) for a name std also defines (STD_COLLIDING below), a site is dropped when its
#       receiver is syntactically a std value: a string/byte literal, `format!(..)`,
#       `String::`/`Vec::`, or a chain ending in `.to_string()`, `.to_vec()`,
#       `.to_owned()`, `.as_str()`, `.collect..()`, `.iter()`, `.chars()`, `.bytes()`,
#       `.trim..()`, `.to_ascii_*()`, `.to_lowercase()`/`.to_uppercase()`.
#       The remaining count for such a name is an upper bound and is marked `~`.
# Free `pub fn`s of mahout_value are counted when called as `mahout_value::name` or
# imported by name; associated functions and enum variants (`Value::int`, `Key::Str`)
# are counted as `Type::name` tokens; both are listed under their own headings.
# RequestCtx's public fields (`ctx.classes.lookup(..)`) are listed as `<field>.<member>`.
#
# Everything is grep-shaped Python over the source text; no build is needed, so the
# numbers are reproducible on the pinned commit (1b3104a8) from a plain checkout.
set -euo pipefail
ROOT="${1:?usage: census.sh <mahout tree>}"
[ -d "$ROOT/crates" ] || { echo "not a Mahout tree: $ROOT" >&2; exit 2; }
exec python3 - "$ROOT" <<'PY'
import re, sys, os, glob, collections
root = sys.argv[1]
C = os.path.join(root, "crates")
EXT = sorted(d for d in os.listdir(C) if d.startswith("mahout_ext_"))

def rs_files(crate, tests=False):
    out = []
    for f in glob.glob(os.path.join(C, crate, "src", "**", "*.rs"), recursive=True):
        base = os.path.basename(f)
        if not tests and (base.startswith("tests") or "/tests/" in f or base.endswith("_tests.rs")):
            continue
        out.append(f)
    return sorted(out)

def strip(src):
    """drop comments and doc lines; keep line structure so counts are per site"""
    src = re.sub(r"/\*.*?\*/", " ", src, flags=re.S)
    return "\n".join(re.sub(r"//.*", "", l) for l in src.split("\n"))

ext_src = {}  # crate -> [(path, text)]
for c in EXT:
    ext_src[c] = [(f, strip(open(f).read())) for f in rs_files(c)]

def blocks(src, head_re):
    """yield (match, body) for every brace block whose header matches"""
    for m in re.finditer(head_re, src, re.M):
        i, d = m.end(), 1
        while d and i < len(src):
            d += (src[i] == "{") - (src[i] == "}"); i += 1
        yield m, src[m.end():i]

PUBFN = r"^\s*pub (?:const )?(?:unsafe )?fn ([a-z_0-9]+)"

def print_table(title, rows, note=""):
    """rows: [(name, sites, crates, flag, exts)] sorted by sites desc.

    An item may carry spaces (`ExtState::ext_teardown (impl)`), so the crates
    are not a trailing column: they are the line under the row, indented past
    the counts and marked `ext:`, which no row starts with and no per-crate
    list under the linker tables does either.
    """
    print(f"## {title}: {len(rows)} distinct items{note}")
    print(f"{'sites':>6} {'crates':>6}  item")
    for name, sites, crates, flag, exts in rows:
        print(f"{sites:>6} {crates:>6}  {flag}{name}")
        print(f"{'':>15}ext: {' '.join(exts)}")
    print()

def tally(counter_by_crate):
    """counter_by_crate: name -> Counter(crate -> n)  ->  rows"""
    rows = []
    for name, cc in counter_by_crate.items():
        s = sum(cc.values())
        exts = sorted(c.removeprefix("mahout_ext_") for c, n in cc.items() if n)
        if s: rows.append((name, s, len(exts), "", exts))
    rows.sort(key=lambda r: (-r[1], r[0]))
    return rows

# ---------------------------------------------------------------- 1. mahout_value
PUBLIC_VALUE_TYPES = {"Value", "ValueRef", "RawValue", "ScratchValue", "TypeTag", "PhpArray",
                      "ArrayIter", "Key", "KeyKind", "ArrayTag", "ValKind", "Edit", "SortGuard",
                      "PhpString", "PhpObject", "ClassId", "PhpRef", "Atom", "NativeState"}
STD_COLLIDING = {"len", "get", "set", "is_empty", "contains", "append", "as_bytes", "as_ref",
                 "index", "push", "iter", "insert", "remove", "clear", "new", "keys", "values",
                 "as_str", "swap", "first", "last", "pop", "sort", "extend", "truncate",
                 "with_capacity", "capacity", "take", "replace", "retain", "drain", "entry",
                 "find", "position", "split", "join", "get_mut", "reserve", "from", "into",
                 "as_slice", "as_mut", "eq", "cmp", "hash", "write", "read", "is_some", "unwrap",
                 "map", "filter", "count", "add", "sub", "mul", "div", "rem", "neg", "abs",
                 "start", "end", "kind", "name", "to_vec", "to_string", "clone", "is_null", "key",
                 "value", "parse", "get_or_insert", "get_or_insert_with", "matches", "id", "tag"}
STD_RECEIVER = re.compile(
    r'(?:"|b"|\'|format!\([^()]*\)|String::[a-z_]+\([^()]*\)|Vec::[a-z_]+\([^()]*\)'
    r'|\.(?:to_string|to_vec|to_owned|as_str|iter|chars|bytes|trim|trim_start|trim_end'
    r'|to_ascii_lowercase|to_ascii_uppercase|to_lowercase|to_uppercase|to_bytes|as_slice'
    r'|collect(?:::<[^>]*>)?)\(\)|\.collect::<[^>]*>\(\))\s*$')

value_names = collections.defaultdict(set)
value_free = set()
trait_fns = set()
for f in rs_files("mahout_value"):
    s = strip(open(f).read())
    for m, body in blocks(s, r"^impl(?:<[^>]*>)?\s+(?:([A-Za-z_:<>]+)\s+for\s+)?(?:crate::)?(?:[a-z_]+::)?([A-Za-z_]+)(?:<[^>]*>)?\s*(?:where[^{]*)?\{"):
        if m.group(1):
            trait_fns.update(re.findall(r"^\s*fn ([a-z_0-9]+)", body, re.M))
        else:
            for n in re.findall(PUBFN, body, re.M):
                value_names[n].add(m.group(2))
    for n in re.findall(r"^pub (?:const )?(?:unsafe )?fn ([a-z_0-9]+)", s, re.M):
        value_free.add(n)
value_all_inherent = set(value_names)
value_public = {n for n, ts in value_names.items() if ts & PUBLIC_VALUE_TYPES}
value_internal_only = value_all_inherent - value_public

def receiver_before(text, pos):
    """the ~80 chars before `.name(`, trimmed at the last statement boundary"""
    seg = text[max(0, pos - 80):pos]
    seg = re.split(r"[;{}]|\blet\b|=>", seg)[-1]
    return seg.rstrip()

val_calls = collections.defaultdict(collections.Counter)
val_dropped = collections.Counter()
for c, files in ext_src.items():
    for _, t in files:
        for m in re.finditer(r"\.([a-z_][a-z_0-9]*)\s*\(", t):
            n = m.group(1)
            if n not in value_public: continue
            if n in STD_COLLIDING and STD_RECEIVER.search(receiver_before(t, m.start())):
                val_dropped[n] += 1; continue
            val_calls[n][c] += 1
rows = tally(val_calls)
rows = [(n, s, k, "~" if n in STD_COLLIDING else "", e) for n, s, k, _, e in rows]
print("# Mahout extension-seam census (scripts/census.sh) over", root)
print(f"# ext crates: {len(EXT)}: {' '.join(c.removeprefix('mahout_ext_') for c in EXT)}")
print()
print(f"mahout_value inherent pub fn names: {len(value_all_inherent)} "
      f"(on public value types: {len(value_public)}; internal-only, not matched: {len(value_internal_only)}); "
      f"free pub fns: {len(value_free)}; trait-impl fns (not matched): {len(trait_fns)}")
print_table("mahout_value methods called from ext crates", rows,
            "  (~ = std-colliding name, upper bound after receiver filtering)")
print("std-receiver sites dropped for colliding names:",
      ", ".join(f"{n}={k}" for n, k in val_dropped.most_common()))
print()
free_calls = collections.defaultdict(collections.Counter)
for c, files in ext_src.items():
    for _, t in files:
        imported = set(re.findall(r"use mahout_value::(?:[a-z_]+::)?\{?([^;]*)\}?;", t))
        imp_names = set()
        for grp in imported:
            imp_names.update(x.strip().split(" as ")[-1].strip() for x in grp.split(","))
        for m in re.finditer(r"\bmahout_value::(?:[a-z_]+::)?([a-z_][a-z_0-9]*)\s*\(", t):
            free_calls[m.group(1)][c] += 1
        for n in imp_names & value_free:
            free_calls[n][c] += len(re.findall(r"(?<![:.\w])" + n + r"\s*\(", t))
print_table("mahout_value free functions called from ext crates", tally(free_calls))
assoc = collections.defaultdict(collections.Counter)
for c, files in ext_src.items():
    for _, t in files:
        for m in re.finditer(r"\b(Value|ValueRef|TypeTag|PhpString|PhpArray|ArrayIter|Key|PhpObject|PhpRef|ClassId|Atom)::([A-Za-z_][A-Za-z_0-9]*)\b", t):
            assoc[m.group(1) + "::" + m.group(2)][c] += 1
print_table("mahout_value associated functions and variants named from ext crates (Type::name)", tally(assoc))

# ---------------------------------------------------------------- 2. RequestCtx
rc_names = set()
for f in rs_files("mahout_runtime"):
    s = strip(open(f).read())
    for m, body in blocks(s, r"^impl(?:<[^>]*>)?\s+(?:crate::)?RequestCtx\s*\{"):
        rc_names.update(re.findall(PUBFN, body, re.M))
rc_calls = collections.defaultdict(collections.Counter)
RC_RECV = re.compile(r"(?:\b_?ctx|\.ctx\(\)|\bRequestCtx:)\s*[.:]\s*([a-z_][a-z_0-9]*)\s*\(")
for c, files in ext_src.items():
    for _, t in files:
        for m in RC_RECV.finditer(t):
            if m.group(1) in rc_names:
                rc_calls[m.group(1)][c] += 1
print(f"RequestCtx inherent pub fns: {len(rc_names)}")
print_table("RequestCtx methods called from ext crates", tally(rc_calls))
rc_fields = set()
for f in rs_files("mahout_runtime"):
    s_ = strip(open(f).read())
    for m, body in blocks(s_, r"^pub struct RequestCtx\s*\{"):
        rc_fields.update(re.findall(r"^\s*pub ([a-z_0-9]+):", body, re.M))
rc_field_use = collections.defaultdict(collections.Counter)
for c, files in ext_src.items():
    for _, t in files:
        for m in re.finditer(r"(?:\b_?ctx|\.ctx\(\))\s*\.\s*([a-z_][a-z_0-9]*)\s*\.\s*([a-z_][a-z_0-9]*)\b", t):
            if m.group(1) in rc_fields:
                rc_field_use[m.group(1) + "." + m.group(2)][c] += 1
print(f"RequestCtx public fields: {len(rc_fields)}: " + " ".join(sorted(rc_fields)))
print_table("RequestCtx public-field members reached from ext crates (ctx.<field>.<member>)", tally(rc_field_use))
any_note_alloc = sum(len(re.findall(r"\.note_alloc\s*\(", t)) for fs in ext_src.values() for _, t in fs)
print(f"note_alloc sites on any receiver: {any_note_alloc}")
print()

# ---------------------------------------------------------------- 3. semantics::
sem_pub = set()
for f in rs_files("mahout_runtime"):
    if "/semantics" not in f: continue
    s = strip(open(f).read())
    sem_pub.update(re.findall(r"^pub (?:const )?(?:unsafe )?fn ([a-z_0-9]+)", s, re.M))
    sem_pub.update(re.findall(r"^pub (?:struct|enum|trait|type|const|static) ([A-Za-z_0-9]+)", s, re.M))
sem_calls = collections.defaultdict(collections.Counter)
for c, files in ext_src.items():
    for _, t in files:
        for m in re.finditer(r"\bsemantics::([A-Za-z_][A-Za-z_0-9]*)", t):
            sem_calls[m.group(1)][c] += 1
        imp = set()
        globbed = False
        for m in re.finditer(r"use mahout_runtime::semantics::(\*|\{([^}]*)\}|[A-Za-z_0-9]+)\s*;", t):
            if m.group(1) == "*": globbed = True
            elif m.group(2): imp.update(x.strip().split(" as ")[-1] for x in m.group(2).split(","))
            else: imp.add(m.group(1))
        imp.discard("self")
        names = (sem_pub if globbed else imp & sem_pub)
        for n in names:
            k = len(re.findall(r"(?<![:.\w])" + re.escape(n) + r"\b(?!\s*::)", t))
            k -= len(re.findall(r"use mahout_runtime::semantics::[^;]*\b" + re.escape(n) + r"\b", t))
            if k > 0: sem_calls[n][c] += k
print(f"semantics:: pub items (fns and types): {len(sem_pub)}")
rows = tally(sem_calls)
print_table("semantics:: items used from ext crates", rows)
print("of which", sum(1 for n, *_ in rows if n[0].islower()), "functions and",
      sum(1 for n, *_ in rows if n[0].isupper()), "types/consts; via semantics:: path only:",
      len({m for fs in ext_src.values() for _, t in fs for m in re.findall(r'\bsemantics::([a-z_][a-z_0-9]*)', t)}))
print()

# ---------------------------------------------------------------- 4. mahout_native_abi
abi_items = {}
for f in rs_files("mahout_native_abi"):
    s = strip(open(f).read())
    for kind, n in re.findall(r"^\s*pub (struct|enum|trait|type|fn|const) ([A-Za-z_0-9]+)", s, re.M):
        abi_items.setdefault(n, kind)
abi_calls = collections.defaultdict(collections.Counter)
for c, files in ext_src.items():
    for _, t in files:
        body = "\n".join(l for l in t.split("\n") if not re.match(r"\s*(pub )?use\b", l))
        for n, kind in abi_items.items():
            if kind == "fn":
                k = len(re.findall(r"(?:NativeError|ArgCursor|InOut|Out|UnionMember)::" + n + r"\b", body))
                if n in {"cell", "into_parts", "next_for", "spelling"}:
                    k += len(re.findall(r"\." + n + r"\s*\(", body))
            else:
                k = len(re.findall(r"(?<![\w:])" + n + r"(?![\w])", body))
                if n in {"Out", "Str", "Res", "Arr", "This", "Obj", "True", "False", "Mixed", "Callback"}:
                    # only in files that import or path-qualify them
                    if not re.search(r"mahout_native_abi::.*\b" + n + r"\b", t): k = 0
            if k: abi_calls[n][c] += k
print(f"mahout_native_abi pub items: {len(abi_items)}: " + " ".join(sorted(abi_items)))
print_table("mahout_native_abi items used from ext crates", tally(abi_calls))

# ---------------------------------------------------------------- 5. macros
mac = collections.defaultdict(collections.Counter)
for c, files in ext_src.items():
    for _, t in files:
        for m in re.finditer(r"#\[(php_[a-z_]+)(\(([^\]]*)\))?\]", t):
            mac["#[" + m.group(1) + "]"][c] += 1
            args = m.group(3) or ""
            for key in re.findall(r"(?<![\w\"])([a-z_]+)\s*(?==|\(|,|$)", re.sub(r'"[^"]*"', '""', args)):
                mac[f"#[{m.group(1)}({key})]"][c] += 1
        for n in re.findall(r"\b(sig_tentative)\b", t):
            mac[n][c] += 1
print_table("mahout_native_macros attributes and their arguments", tally(mac))

# ---------------------------------------------------------------- 6. linker / code_meta
def expand_uses(t, crate):
    out = collections.Counter()
    for m in re.finditer(r"use " + crate + r"::([^;]*);", t):
        spec = re.sub(r"\s+", " ", m.group(1))
        def walk(prefix, s):
            s = s.strip()
            if s.startswith("{") and s.endswith("}"):
                depth, cur, parts = 0, "", []
                for ch in s[1:-1]:
                    if ch == "{": depth += 1
                    if ch == "}": depth -= 1
                    if ch == "," and depth == 0: parts.append(cur); cur = ""
                    else: cur += ch
                parts.append(cur)
                for p in parts:
                    if p.strip(): walk(prefix, p)
            else:
                s = s.split(" as ")[0].strip()
                if "::" in s and "{" in s:
                    head, rest = s.split("::{", 1)
                    walk(prefix + head + "::", "{" + rest)
                elif s and s != "self":
                    out[prefix + s] += 1
        walk("", spec)
    for m in re.finditer(r"\b" + crate + r"::([A-Za-z_][A-Za-z_0-9]*(?:::[A-Za-z_][A-Za-z_0-9]*)*)", t):
        path = m.group(1)
        segs = path.split("::")
        # a Type::Variant or Type::fn path names the type
        item = segs[0] if segs[0][0].isupper() or len(segs) == 1 else "::".join(segs[:2])
        if item == "msgs": item = path
        out[item] += 1
    return out
for crate in ("mahout_linker", "mahout_code_meta", "mahout_native"):
    per_item = collections.defaultdict(collections.Counter)
    per_crate = collections.defaultdict(set)
    dep = [c for c in EXT if re.search(r"^" + crate + r"\b", open(os.path.join(C, c, "Cargo.toml")).read(), re.M)]
    for c, files in ext_src.items():
        for _, t in files:
            for item, k in expand_uses(t, crate).items():
                per_item[item][c] += k; per_crate[c].add(item)
    print(f"## {crate}: Cargo dependents {len(dep)}, crates referencing it in source {len(per_crate)}")
    print_table(f"{crate} items imported or path-qualified by ext crates", tally(per_item))
    for c in sorted(per_crate):
        print(f"  {c}: " + ", ".join(sorted(per_crate[c])))
    print()

# ---------------------------------------------------------------- 7. other mahout_runtime items
other = collections.defaultdict(collections.Counter)
for c, files in ext_src.items():
    for _, t in files:
        for item, k in expand_uses(t, "mahout_runtime").items():
            if item in ("RequestCtx", "semantics") or item.startswith("semantics::"): continue
            other[item][c] += k
reent = set(re.findall(r"^\s*fn ([a-z_0-9]+)", next(b for _, b in blocks(strip(open(os.path.join(C, "mahout_runtime/src/reentrant.rs")).read()), r"^pub trait Reentrant\b[^{]*\{")), re.M))
for c, files in ext_src.items():
    for _, t in files:
        for m in re.finditer(r"(?:\b(?:r|re|reent|reentrant)\.|\bReentrant::)([a-z_][a-z_0-9]*)\s*\(", t):
            if m.group(1) in reent and m.group(1) != "ctx":
                other["Reentrant::" + m.group(1)][c] += 1
        for m in re.finditer(r"\b(?:r|reent|reentrant)\.ctx\(\)", t):
            other["Reentrant::ctx"][c] += 1
        for m in re.finditer(r"\bext_state_mut(?:::<[A-Za-z_]+>)?\s*\(", t):
            other["RequestCtx::ext_state_mut"][c] += 1
        for m in re.finditer(r"\bfn ext_teardown\b", t):
            other["ExtState::ext_teardown (impl)"][c] += 1
print_table("other mahout_runtime items imported or called from ext crates (Reentrant, ExtState, EngineBuilder, native_marshal, ...)", tally(other))

# ---------------------------------------------------------------- 8. Cargo dependents
deps = collections.Counter()
for c in EXT:
    for m in re.finditer(r"^(mahout_[a-z_0-9]+)\b", open(os.path.join(C, c, "Cargo.toml")).read(), re.M):
        deps[m.group(1)] += 1
print("## workspace crates the ext crates depend on (Cargo.toml)")
for n, k in deps.most_common(): print(f"{k:>6}  {n}")
PY
