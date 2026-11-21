#!/usr/bin/env python3
"""Bounded fileinfo oracle observations; no engine switch or benchmark timing."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import runpy
import struct
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "data/fileinfo"
# The rule that finds the pinned php-src tree, loaded by path: under
# `python3 -I` this directory is off sys.path (scripts/php_src_tree.py).
PHP_SRC_TREE = runpy.run_path(str(ROOT / "scripts/php_src_tree.py"))


def digest(data):
    return hashlib.sha256(data).hexdigest()


def normalise(content, directory):
    text = content.decode(errors="backslashreplace")
    for path, label in [(str(directory), "<cwd>"), (str(DATA), "<probe>")]:
        text = text.replace(path.replace('/', r'\/'), label).replace(path, label)
    return text


def environment(recipe, directory):
    env = {} if recipe["env_clear"] else dict(os.environ)
    env.update({name: value.replace("{RUN}", str(directory)) for name, value in recipe["env"].items()})
    (directory / "home").mkdir()
    (directory / "home/.php_history").touch()
    (directory / "tmp").mkdir()
    return env


def compiled(endian, count=1, version=19):
    # A fixture, not a compiler: one FILE_STRING record. Layout is pinned
    # file.h:168-208,210-377; byteswap in apprentice.c:3441 onward.
    header = bytearray(432)
    struct.pack_into(endian + "4I", header, 0, 0xF11E041C, version, count, 0)
    record = bytearray(432)
    record[2] = 0x20  # BINTEST
    record[4:7] = bytes([ord("="), 6, 5])
    struct.pack_into(endian + "I", record, 20, 1)  # lineno
    # No text/binary restriction: short GIF prefixes look like ASCII text.
    struct.pack_into(endian + "I", record, 28, 0)
    record[32:38] = b"GIF89a"
    record[160:168] = b"compiled"
    record[224:246] = b"application/x-compiled"
    return bytes(header + record)


def builtin_inventory():
    """Decode the pinned C string data only; this is not product generation."""
    text = (ROOT / "ext/fileinfo/data_file.c").read_text()
    rows = re.findall(r'^"(.*)",$', text, re.MULTILINE)
    dimension = re.search(r'php_magic_database\[(\d+)\]\[1024\]', text)
    if dimension is None or len(rows) != int(dimension[1]):
        raise SystemExit("unrecognized database declaration")
    chunks = []
    simple = {'n': 10, '?': 63, '"': 34, '\\': 92}
    for row in rows:
        decoded = bytearray()
        at = 0
        while at < len(row):
            if row[at] != '\\':
                decoded.append(ord(row[at])); at += 1
            else:
                at += 1
                if row[at] in '01234567':
                    end = at + 1
                    while end < min(at + 3, len(row)) and row[end] in '01234567':
                        end += 1
                    decoded.append(int(row[at:end], 8)); at = end
                elif row[at] in simple:
                    decoded.append(simple[row[at]]); at += 1
                else:
                    raise SystemExit("unrecognized database escape")
        if len(decoded) != 1024:
            raise SystemExit("database chunk has unexpected length")
        chunks.append(decoded)
    data = b''.join(chunks)
    magic, version, binary, text_count = struct.unpack_from('<4I', data)
    if (magic, version) != (0xF11E041C, 19):
        raise SystemExit("database format changed")
    used = (1 + binary + text_count) * 432
    if used > len(data) or any(data[used:]):
        raise SystemExit("database counts/padding invalid")
    types = {}
    levels = {}
    for offset in range(432, used, 432):
        kind = data[offset + 6]
        if kind == 0 or kind >= 60:
            raise SystemExit("unrecognized rule type")
        types[str(kind)] = types.get(str(kind), 0) + 1
        level = struct.unpack_from('<H', data, offset)[0]
        levels[str(level)] = levels.get(str(level), 0) + 1
    return {"sha256": digest(data), "bytes": len(data), "used_bytes": used,
            "padding_bytes": len(data) - used, "record_bytes": 432,
            "version": version, "binary_rules": binary, "text_rules": text_count,
            "types": types, "continuation_levels": levels}


def fixtures(directory, source):
    lazy_regex = bytearray(compiled("<"))
    lazy_regex[432 + 2] = 0x40  # TEXTTEST
    lazy_regex[432 + 5:432 + 7] = bytes([1, 17])  # FILE_REGEX
    lazy_regex[432 + 32:432 + 160] = b"(" + bytes(127)
    missing_use = bytearray(compiled("<"))
    missing_use[432 + 5:432 + 7] = bytes([7, 46])  # FILE_USE
    missing_use[432 + 32:432 + 160] = b"missing" + bytes(121)
    files = {
        "native.mgc": compiled("<"), "swapped.mgc": compiled(">"),
        "bad-version.mgc": compiled("<", version=18),
        "bad-count.mgc": compiled("<", count=2),
        "truncated.mgc": compiled("<")[:-1],
        "bad-regex": b"0 regex [ invalid\n", "bad-type": b"0 unknown x invalid\n",
        "lazy-bad-regex.mgc": bytes(lazy_regex),
        "capture-zero": b"0 regex GIF89a capture\n",
        "capture-many": b"0 regex " + b"()" * 50 + b"GIF89a capture\n",
        "captures": b"0 regex (G)(I)(F)(89)(a) captures\n",
        "regex-limit": b"0 regex (a+)+$ limited\n",
        "continuations": b"0 string GIF base\n>3 string 89 version\n>>5 string a suffix\n!:strength + 10\n",
        "tail": b"-4 string TAIL tail\n",
        "buffer-failure": bytes(missing_use),
        "long-output": b"0 string GIF89a " + b"x" * 60 + b"\n" +
                       b">0 string GIF89a " + b"y" * 60 + b"\n",
        "tail-input": b"x" * (7 * 1024 * 1024 + 100) + b"TAIL",
        "cdf-input": (source / "ext/fileinfo/tests/resources/test.ppt").read_bytes(),
        # Valid compiled counts, actual large contents. The memory limit must
        # refuse the database allocation rather than only a shadow charge.
        "large.mgc": compiled("<", count=40000)[:432] + compiled("<")[432:] * 40000,
    }
    for name, content in files.items():
        (directory / name).write_bytes(content)
    return {name: {"bytes": len(content), "sha256": digest(content)}
            for name, content in files.items()}


def cases():
    for hook in ["open", "stat", "read", "cast", "close", "warning", "deprecation",
                 "identify-warning", "regex-warning", "buffer-failure"]:
        for override in ["0", "16"]:
            yield "transactions", [hook, override, "mutate"], []
            yield "transactions", [hook, override, "throw"], []
    # Reconstruction is tested only before magic acquisition (deprecation);
    # none is tested from a stream callback.
    yield "transactions", ["deprecation", "16", "replace"], []
    for mode in ["file", "mime"]:
        for kind in ["short", "stat-fail", "read-fail", "nonseekable", "rewind-fail", "restore-fail", "close-throw"]:
            yield "streams", [kind, mode], []
    for kind in ["plain", "filtered", "socket"]:
        yield "streams", [kind, "mime"], []
    for profile in ["profiles", "snapshot", "failure", "retained", "limit"]:
        yield "storage", [profile], ["-d", "memory_limit=8M"] if profile == "limit" else []
    yield "recognition", [], []
    yield "regex-storage", [], []
    for kind in ["tail", "tail-short", "tail-pipe", "cdf", "plain", "missing"]:
        yield "descriptors", [kind], []


def held(table, content):
    """The digest `[oracle]` holds the binary's `content` to, spelt as xtask's
    `oracle::Digest` names it: its `sha256` wherever it pins one, and its
    `md5` where it pins that alone."""
    if "sha256" in table:
        pinned, actual = table["sha256"], "sha256:" + digest(content)
    elif "md5" in table:
        pinned, actual = "md5 " + table["md5"], "md5 " + hashlib.md5(content).hexdigest()
    else:
        raise SystemExit("[oracle] names neither `md5` nor `sha256`, and its binary is held to one of them")
    if actual != pinned:
        raise SystemExit(f"oracle digest mismatch: the binary is {actual}, [oracle] pins {pinned}")
    return pinned


def php_src(source, environ):
    """The pinned php-src tree on this machine, found by xtask's rule as
    scripts/php_src_tree.py keeps it; where neither place is a directory,
    the refusal naming both."""
    tree = PHP_SRC_TREE["found"](source, environ)
    if tree is None:
        raise SystemExit(PHP_SRC_TREE["refusal"](source, environ))
    return tree


def observe(scratch):
    pin_bytes = (ROOT / "oracle.toml").read_bytes()
    pin = tomllib.loads(pin_bytes.decode())
    binary = Path(pin["oracle"]["path"])
    content = binary.read_bytes()
    pinned = held(pin["oracle"], content)
    source = php_src(pin["source"], os.environ)
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
    if not head.startswith(pin["source"]["commit"]):
        raise SystemExit("oracle source commit mismatch")
    # Reuse the harness's complete closure assertion, not a second keg walker.
    closure_call = ["cargo", "xtask", "witness", "--crate", "ext/fileinfo", "--item", "extension"]
    closure = subprocess.run(closure_call, cwd=ROOT, capture_output=True, text=True, timeout=120)
    if closure.returncode != 0 or "1 witnesses pass" not in closure.stdout:
        raise SystemExit("pinned oracle/closure assertion failed: " + closure.stdout + closure.stderr)
    recipe = pin["recipe"]
    call = [str(binary), *recipe["args"]]
    for setting in recipe["ini"]:
        call += ["-d", setting]
    observations = []
    fixture_pins = None
    for name, arguments, extra in cases():
        with tempfile.TemporaryDirectory(prefix="t2827-", dir=scratch) as temporary:
            directory = Path(temporary)
            fixture_pins = fixtures(directory, source)
            env = environment(recipe, directory)
            probe = DATA / (name + ".php")
            proc = subprocess.run([*call, *extra, str(probe), *arguments], cwd=directory,
                                  env=env, capture_output=True, timeout=recipe["limits"]["timeout_s"])
            observations.append({"probe": name, "arguments": arguments, "extra": extra,
                                 "standing": "undefined-stat-copy" if name == "streams" and arguments == ["stat-fail", "mime"] else "defined",
                                 "status": proc.returncode,
                                 "stdout": normalise(proc.stdout, directory),
                                 "stderr": normalise(proc.stderr, directory)})
    workload = ROOT / "docs/perf/workloads/fileinfo.php"
    workload_observations = []
    for profile in ["buffer", "file", "wrapper", "cold"]:
        with tempfile.TemporaryDirectory(prefix="t2827-workload-", dir=scratch) as temporary:
            directory = Path(temporary)
            env = environment(recipe, directory)
            proc = subprocess.run([*call, str(workload), profile], env=env, capture_output=True,
                                  timeout=recipe["limits"]["timeout_s"])
            workload_observations.append({"profile": profile, "status": proc.returncode,
                                          "stdout": normalise(proc.stdout, directory), "stderr": normalise(proc.stderr, directory)})
    sources = {}
    for path in sorted((ROOT / "ext/fileinfo").rglob("*")):
        if path.is_file() and path.suffix in {".c", ".h", ".m4", ".rs"}:
            body = path.read_bytes()
            sources[str(path.relative_to(ROOT))] = {"sha256": digest(body), "bytes": len(body),
                                                    "physical_lines": len(body.splitlines())}
            if path.parent.name == "libmagic" or path.name in {"config.m4", "data_file.c"}:
                upstream = source / path.relative_to(ROOT)
                if upstream.read_bytes() != body:
                    raise SystemExit("vendored input differs from pinned source: " + str(path))
    # Fail unknown config inputs rather than silently leaving a recognizer out.
    configured = re.findall(r"libmagic/[a-z_]+\.c", (ROOT / "ext/fileinfo/config.m4").read_text())
    for name in configured:
        if "ext/fileinfo/" + name not in sources:
            raise SystemExit("unaccounted configured source: " + name)
    if digest(binary.read_bytes()) != digest(content) or (ROOT / "oracle.toml").read_bytes() != pin_bytes:
        raise SystemExit("oracle or pin changed during observation")
    after_closure = subprocess.run(closure_call, cwd=ROOT, capture_output=True, text=True, timeout=120)
    if after_closure.returncode != 0 or "1 witnesses pass" not in after_closure.stdout:
        raise SystemExit("oracle closure changed during observation")
    return {"oracle": {"version": pin["oracle"]["version"], "digest": pinned,
                       "sha256": digest(content), "source_commit": head,
                       "pin_sha256": digest(pin_bytes), "recipe": recipe, "closure": pin["closure"],
                       "closure_assertion": {"command": closure_call, "status": closure.returncode, "stdout": closure.stdout,
                                             "after_status": after_closure.returncode, "after_stdout": after_closure.stdout}},
            "host": {"system": platform.system(), "machine": platform.machine()},
            "probe_pins": {p.name: digest(p.read_bytes()) for p in sorted(DATA.glob("*.php"))},
            "runner_sha256": digest(Path(__file__).read_bytes()),
            "fixtures": fixture_pins, "sources": sources, "configured": configured,
            "builtin_database": builtin_inventory(),
            "workload": {"sha256": digest(workload.read_bytes()), "observations": workload_observations},
            "observations": observations}


def comparable(receipt):
    value = json.loads(json.dumps(receipt))
    for case in value["observations"]:
        if case["probe"] == "streams" and case["arguments"] == ["stat-fail", "mime"]:
            # Preserve identity/standing, but never judge sampled undefined bytes.
            for key in ["status", "stdout", "stderr"]:
                case.pop(key)
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--accept", action="store_true", help="capture observations, never witness verdicts")
    parser.add_argument("--receipt", type=Path, default=DATA / "oracle.json")
    args = parser.parse_args()
    receipt = observe(args.scratch)
    if args.accept:
        args.receipt.write_text(json.dumps(receipt, indent=2) + "\n")
        print(f"captured {len(receipt['observations'])} bounded observations")
    else:
        expected = json.loads(args.receipt.read_text())
        if comparable(receipt) != comparable(expected):
            for old, new in zip(expected["observations"], receipt["observations"]):
                if old != new:
                    print("changed observation:", new["probe"], new["arguments"])
            raise SystemExit("receipt differs: inspect rather than treating this as a port pass")
        print(f"reproduced {len(receipt['observations']) - 1} defined oracle observations; one undefined case retained but not compared; not replacement certification")


if __name__ == "__main__":
    main()
