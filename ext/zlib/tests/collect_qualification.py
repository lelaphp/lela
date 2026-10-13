"""Reproduce #2826's bounded observations; never a bench or completion reading."""

import hashlib
import json
import pathlib
import platform
import subprocess
import tempfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[3]
SCRATCH = pathlib.Path("/private/var/folders/pk/b0gf4czd1xq427j6n__fxqzh0000gn/T/opencode")
DATA = ROOT / "data/compression-2826"
GENERATOR = ROOT / "ext/zlib/tests/qualification.php"
FIXTURE = ROOT / "ext/zlib/tests/data/oracle.tsv"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(args, **kwargs):
    return subprocess.run(args, capture_output=True, check=True, **kwargs)


def main():
    pin = tomllib.loads((ROOT / "oracle.toml").read_text())
    binary = pathlib.Path(pin["oracle"]["path"])
    assert hashlib.md5(binary.read_bytes()).hexdigest() == pin["oracle"]["md5"]
    source = pathlib.Path(pin["source"]["path"])
    commit = run(["git", "rev-parse", "HEAD"], cwd=source).stdout.decode().strip()
    assert commit.startswith(pin["source"]["commit"])
    with tempfile.TemporaryDirectory(prefix="t2826-", dir=SCRATCH) as scratch:
        args = [str(binary), "-n", str(GENERATOR), scratch]
        oracle = run(args, cwd=ROOT)
        assert not oracle.stderr, oracle.stderr
        limit = subprocess.run(args + ["limit"], cwd=ROOT, capture_output=True)
        assert limit.returncode != 0
        assert b"Allowed memory size" in limit.stdout + limit.stderr
    lines = oracle.stdout.decode().splitlines()
    assert all(line.split("\t")[0] in {
        "input", "deflate", "parameters", "flush", "dictionary", "inflate-dictionary",
        "inflate-step", "memory", "gzip-file", "zip-entry", "unavailable",
    } for line in lines)
    assert sum(line.startswith("deflate\t") for line in lines) == 180
    FIXTURE.parent.mkdir(exist_ok=True)
    FIXTURE.write_bytes(oracle.stdout)
    tests = run(["cargo", "test", "-p", "php_ext_zlib", "--lib", "qualification", "--",
                 "--nocapture", "--test-threads=1"], cwd=ROOT)
    observations = [line for line in tests.stderr.decode().splitlines()
                    if line.startswith("qualification\t")]
    assert sum(line.startswith("qualification\tdeflate\t") for line in observations) == 120
    DATA.mkdir(exist_ok=True)
    (DATA / "miniz.tsv").write_text("\n".join(observations) + "\n")
    (DATA / "tests.txt").write_bytes(tests.stdout)
    (DATA / "limit.stdout").write_bytes(limit.stdout)
    (DATA / "limit.stderr").write_bytes(limit.stderr)
    receipt = {
        "standing": "measured",
        "role": "bounded prerequisite, not witness/bench or complete corpus",
        "source_baseline": run(["git", "rev-parse", "HEAD"], cwd=ROOT).stdout.decode().strip(),
        "machine": platform.platform(),
        "rustc": run(["rustc", "--version"]).stdout.decode().strip(),
        "oracle_md5": pin["oracle"]["md5"],
        "php_source_commit": commit,
        "recipe": [str(binary), "-n", "ext/zlib/tests/qualification.php", "<isolated scratch>"],
        "test_command": "cargo test -p php_ext_zlib --lib qualification -- --nocapture --test-threads=1",
        "zlib_ri": run([str(binary), "-n", "--ri", "zlib"]).stdout.decode(),
        "test_scope": "miniz 0.8.9; linked legacy zlib system-allocation trace; no adapted backend",
        "request_limit": {"exit": limit.returncode, "stdout_sha256": digest(DATA / "limit.stdout"),
                          "stderr_sha256": digest(DATA / "limit.stderr")},
        "inputs": {str(path.relative_to(ROOT)): digest(path) for path in [
            GENERATOR, FIXTURE, ROOT / "ext/zlib/src/ffi/qualification.rs", ROOT / "Cargo.lock",
            ROOT / "ext/zlib/Cargo.toml", pathlib.Path(__file__),
        ]},
        "observations_sha256": digest(DATA / "miniz.tsv"),
    }
    (DATA / "oracle.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"{len(lines)} oracle rows; {len(observations)} candidate/library observations")


if __name__ == "__main__":
    main()
