"""#2831 bounded, non-product extraction. No C preprocessing or codec execution.

Recognizes the pinned declaration grammar, not arbitrary C. Every input is
receipted; changed inputs require reviewed data/algorithm/orchestration classes.
The emitted Rust is a demonstration in target/, never committed or linked.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re


def digest(data):
    return hashlib.sha256(data).hexdigest()


def clean(text):
    return re.sub(r"/\*.*?\*/|//[^\n]*", "", text, flags=re.S)


def integer(token):
    token = token.strip()
    if not re.fullmatch(r"-?(?:0[xX][0-9a-fA-F]+|[0-9]+)[uUlL]*", token):
        raise ValueError(f"unsupported expression: {token}")
    return int(re.sub(r"[uUlL]+$", "", token), 16 if "0x" in token.lower() else 10)


def split(body):
    # Registration fields contain no nested expressions in this pinned grammar.
    return [part.strip() for part in re.split(r',(?=(?:[^\"]*\"[^\"]*\")*[^\"]*$)', body) if part.strip()]


def string(token):
    return None if token == "NULL" else json.loads(token)


def collect(root):
    source_paths = sorted({p for p in root.glob("libmbfl/**/*") if p.is_file()} |
                          {root / "unicode_data.h", root / "rare_cp_bitvec.h", root / "config.m4", root / "libmbfl/LICENSE"})
    for path in source_paths:
        if not path.is_file():
            raise ValueError(f"missing input: {path}")
    texts = {str(path.relative_to(root)): path.read_bytes().decode() for path in source_paths}
    for path in ("libmbfl/mbfl/mbfl_encoding.c", "libmbfl/mbfl/mbfl_encoding.h"):
        if path not in texts:
            raise ValueError(f"missing input: {path}")
    header = clean(texts["libmbfl/mbfl/mbfl_encoding.h"])
    enum = re.search(r"enum mbfl_no_encoding\s*\{([^}]+)\}", header).group(1)
    identities, value = {}, 0
    for field in split(enum):
        pair = field.split("=", 1)
        if len(pair) == 2:
            value = integer(pair[1])
        identities[pair[0].strip()] = value
        value += 1
    ordered = re.search(r"mbfl_encoding_ptr_list\[\]\s*=\s*\{([^}]+)\}",
                        clean(texts["libmbfl/mbfl/mbfl_encoding.c"])).group(1)
    order = re.findall(r"&([a-zA-Z0-9_]+)", ordered)
    registrations, aliases, tables, functions = {}, {}, [], []
    flags = {"0": 0}
    flags.update({name: integer(token) for name, token in re.findall(
        r"#define\s+(MBFL_ENCTYPE_\w+)\s+(\w+)", clean(texts["libmbfl/mbfl/mbfl_consts.h"]))})
    for path, original in texts.items():
        text = clean(original)
        for match in re.finditer(r"(?:static\s+)?const char\s*\*\s*(\w+)\[\]\s*=\s*\{([^}]+)\}", text):
            fields = split(match[2])
            if fields[-1] != "NULL":
                raise ValueError(f"unterminated aliases: {path}:{match[1]}")
            aliases[(path, match[1])] = [string(field) for field in fields[:-1]]
        for match in re.finditer(r"const mbfl_encoding\s+(\w+)\s*=\s*\{([^}]+)\}", text):
            registrations[match[1]] = (path, split(match[2]))
        for match in re.finditer(r"^DEF_SB(?:_TBL)?\(([^;]+)\);", text, re.M):
            args = split(match[1])
            ident, name, mime, alias = args[:4]
            registrations[f"mbfl_encoding_{ident}"] = (path, [
                f"mbfl_no_encoding_{ident}", name, mime, alias, "NULL", "MBFL_ENCTYPE_SBCS",
                f"&vtbl_{ident}_wchar", f"&vtbl_wchar_{ident}", f"mb_{ident}_to_wchar",
                f"mb_wchar_to_{ident}", "NULL", "NULL"])
        # Record all table declarations losslessly, including nonnumeric ones;
        # unsupported initializers are NOT fed through the numeric generator.
        pattern = r"(?:static\s+)?const\s+((?:unsigned\s+)?(?:short|int|char|long)|u?int\d+_t)\s+(\w+)\s*((?:\[[^\]]*\])+)(?:\s+ZEND_NONSTRING)?[ \t\n]*=\s*(\{.*?\});"
        for match in re.finditer(pattern, text, re.S):
            ctype, symbol, dimensions, body = match.groups()
            tokens = [s.strip() for s in re.split(r"[,{}\s]+", body) if s.strip()]
            try:
                values = [integer(token) for token in tokens]
            except ValueError:
                values = None
            width = {"char": 8, "short": 16, "int": 32}.get(ctype.replace("unsigned ", ""))
            if width is None and re.fullmatch(r"u?int\d+_t", ctype):
                width = int(re.search(r"\d+", ctype)[0])
            tables.append(dict(path=path, symbol=symbol, c_type=ctype, width_bits=width,
                               declared_dimensions=re.findall(r"\[([^\]]*)\]", dimensions),
                               scalar_count=None if values is None else len(values),
                               initializer_sha256=digest(body.encode()), order="source order",
                               extraction="numeric" if values is not None else "requires classified extractor",
                               licence_source=path, values=values))
        # Structured tables are inventoried, but this scalar generator refuses
        # them. An approved field schema/extractor is required before shipping.
        structured = r"(?:static\s+)?const\s+(struct\s*\{.*?\}|mbfl_html_entity_entry)\s+(\w+)\s*((?:\[[^\]]*\])+)[ \t\n]*=\s*(\{.*?\});"
        for match in re.finditer(structured, text, re.S):
            ctype, symbol, dimensions, body = match.groups()
            tables.append(dict(path=path, symbol=symbol, c_type=ctype, width_bits=None,
                               declared_dimensions=re.findall(r"\[([^\]]*)\]", dimensions),
                               scalar_count=None, initializer_sha256=digest(body.encode()),
                               order="source field/row order", extraction="requires classified structured extractor",
                               licence_source=path, values=None))
        if path.endswith(".c"):
            for match in re.finditer(r"^([\w *]+?)\s+(\w+)\s*\(([^;{}]*)\)\s*\{", text, re.M):
                if match[2] in {"if", "while", "for", "switch", "do", "else", "sizeof"}:
                    continue
                functions.append(dict(path=path, symbol=match[2],
                                      signature_sha256=digest(match[0].encode()),
                                      rust=None, proof=None, status="untranslated; no completion claim"))
    rows = []
    for position, symbol in enumerate(order + ["mbfl_encoding_pass", "mbfl_encoding_wchar"]):
        if symbol not in registrations:
            raise ValueError(f"missing registration: {symbol}")
        path, fields = registrations[symbol]
        if len(fields) != 12:
            raise ValueError(f"unsupported registration: {symbol}")
        identity, name, mime, alias, length, flag, *operations = fields
        bits = 0
        for token in flag.split("|"):
            if token.strip() not in flags:
                raise ValueError(f"unknown flag: {token}")
            bits |= flags[token.strip()]
        rows.append(dict(symbol=symbol, source=path, identity=identities[identity],
                         identity_symbol=identity, supported_position=position if position < len(order) else None,
                         name=string(name), mime=string(mime),
                         aliases=[] if alias == "NULL" else aliases[(path, alias)],
                         flags=bits, flag_expression=flag, length_table=length,
                         family=Path(path).stem.removeprefix("mbfilter_"),
                         operations=dict(zip(("filter_decode", "filter_encode", "decode", "encode", "check", "cut"), operations)),
                         check_profile="dedicated" if operations[4] != "NULL" else "decode/bad-input fallback",
                         cut_profile="dedicated" if operations[5] != "NULL" else
                                     "fixed-width" if bits & 7 else "lead-byte-table" if length != "NULL" else "stateful filters",
                         variant=identity,
                         rust_implemented=False))
    expanded = []
    for row in rows:
        if row["family"] == "singlebyte":
            short = row["identity_symbol"].removeprefix("mbfl_no_encoding_")
            for symbol in (f"mbfl_filt_conv_{short}_wchar", f"mbfl_filt_conv_wchar_{short}",
                           f"mb_{short}_to_wchar", f"mb_wchar_to_{short}"):
                if not any(f["path"] == row["source"] and f["symbol"] == symbol for f in functions):
                    expanded.append(dict(path=row["source"], symbol=symbol, rust=None, proof=None,
                                         status="DEF_SB_TBL expansion; untranslated"))
    return dict(schema=1, lookup_precedence=["canonical case-insensitive", "MIME supported order", "aliases supported/alias order"],
                identities=identities, encodings=rows[:len(order)], internal=rows[len(order):],
                tables=tables, functions=functions, macro_functions=expanded,
                counts=dict(c_physical_lines=sum(len(text.splitlines()) for path, text in texts.items() if path.endswith(".c")),
                            c_nonblank_without_comments=sum(sum(bool(line.strip()) for line in clean(text).splitlines())
                                                            for path, text in texts.items() if path.endswith(".c")),
                            header_physical_lines=sum(len(text.splitlines()) for path, text in texts.items() if path.endswith(".h")),
                            mapping_scalar_cells=sum(table["scalar_count"] or 0 for table in tables)),
                sources=[dict(path=path, sha256=digest(text.encode()), lines=len(text.splitlines())) for path, text in texts.items()])


def validate(current, pinned, classes=None):
    classes = classes or {}
    old = {row["path"]: row["sha256"] for row in pinned["sources"]}
    new = {row["path"]: row["sha256"] for row in current["sources"]}
    changed = {path for path in old.keys() | new.keys() if old.get(path) != new.get(path)}
    if classes.keys() - changed:
        raise ValueError("classification does not name a source change")
    for path in changed:
        if classes.get(path) not in {"data", "algorithm", "orchestration"}:
            raise ValueError(f"unclassified fork change or missing input: {path}")
    # Classification never blesses unrelated metadata, nor changes the pin.
    # An update still needs a newly extracted/reviewed snapshot and coverage.
    if receipt(current) != pinned:
        raise ValueError("stale inventory; regenerate and review the changed snapshot")


def receipt(collected):
    # Initializers already live losslessly in licensed source; do not duplicate
    # mapping data or commit generated Rust. Keep dimensions/order/digests here.
    return {**collected, "tables": [{k: v for k, v in row.items() if k != "values"}
                                  for row in collected["tables"]]}


def require_implemented(rows, families, required_identities):
    actual = {row["identity"] for row in rows}
    if not rows or actual != required_identities or len(actual) != len(rows):
        raise ValueError("missing or duplicate encoding coverage")
    for row in rows:
        if row["family"] not in families or not row["rust_implemented"]:
            raise ValueError(f"unimplemented family: {row['family']}")


def generate_numeric(table, out_dir):
    if table["values"] is None or table["width_bits"] is None:
        raise ValueError("unsupported expression or width")
    signed = not (table["c_type"].startswith("unsigned") or table["c_type"].startswith("uint"))
    width = table["width_bits"]
    lo, hi = (-(1 << (width - 1)), (1 << (width - 1)) - 1) if signed else (0, (1 << width) - 1)
    if any(not lo <= value <= hi for value in table["values"]):
        raise ValueError("value outside declared width")
    dimensions = table["declared_dimensions"]
    known_product = 1
    for dimension in dimensions:
        if dimension:
            known_product *= integer(dimension)
    if (all(dimensions) and known_product != len(table["values"])) or len(table["values"]) % known_product:
        raise ValueError("initializer dimensions differ")
    out_dir = Path(out_dir).resolve()
    workspace_target = Path(__file__).resolve().parents[3] / "target"
    if not out_dir.is_relative_to(workspace_target):
        raise ValueError("Rust demonstration belongs in this workspace's build output under target")
    out_dir.mkdir(parents=True, exist_ok=True)
    output = out_dir / f"{table['symbol']}.rs"
    output.write_text(f"// Non-product extraction proof; {table['path']}:{table['symbol']}\n"
                      f"const {table['symbol'].upper()}: [{'i' if signed else 'u'}{width}; {len(table['values'])}] = "
                      f"{table['values']};\n")
    return output


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--snapshot", type=Path)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--out-dir", type=Path)
    parser.add_argument("--upstream", type=Path, help="explicit pinned php-src/ext/mbstring, observation only")
    args = parser.parse_args()
    collected = collect(args.root)
    if args.upstream:
        origins = []
        for row in collected["sources"]:
            source = args.upstream / row["path"]
            upstream_digest = digest(source.read_bytes()) if source.is_file() else None
            origins.append(dict(path=row["path"], local_sha256=row["sha256"],
                                pinned_source_sha256=upstream_digest,
                                equal=row["sha256"] == upstream_digest))
        print(json.dumps({"pinned_source_observations": origins}))
    if args.compare:
        validate(collected, json.loads(args.compare.read_text()))
    if args.snapshot:
        args.snapshot.parent.mkdir(parents=True, exist_ok=True)
        args.snapshot.write_text(json.dumps(receipt(collected), indent=2) + "\n")
    if args.out_dir:
        generate_numeric(next(row for row in collected["tables"] if row["symbol"] == "iso8859_2_ucs_table"), args.out_dir)
    print(json.dumps({"supported": len(collected["encodings"]), "internal": len(collected["internal"]),
                      "tables": len(collected["tables"]), "functions": len(collected["functions"])}))
