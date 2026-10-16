"""#2831 oracle-only observations; NO wall time, bench lane or replacement verdict.

The caller supplies the pinned binary/digest explicitly. This does not read or
change oracle.toml, select a new regime, or author witness verdicts.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
DATA = ROOT / "data/mbstring-2831"
INI = ["error_reporting=30719", "display_errors=1", "log_errors=0",
       "memory_limit=128M", "date.timezone=UTC"]


def sha(data):
    return hashlib.sha256(data).hexdigest()


def capture(binary, md5):
    binary = Path(binary)
    before = binary.read_bytes()
    if hashlib.md5(before).hexdigest() != md5:
        raise ValueError("oracle identity differs from caller's pin")
    args = [str(binary), "-n"]
    for value in INI:
        args += ["-d", value]
    env = dict(TZ="UTC", LANG="C", LC_ALL="C", PHPRC="/dev/null")
    cases = [(f"ext/mbstring/tests/{name}.php", [], filename, 0) for name, filename in
             [("inventory", "inventory-oracle.json"), ("profiles", "profiles.hex"),
              ("profiles_throw", "profiles_throw.stdout"),
              ("allocation_profiles", "allocation_profiles.stdout")]]
    cases += [("ext/mbstring/tests/allocation_limit.php", [kind], f"limit-{kind}.stdout", 255)
              for kind in ("initial", "growth")]
    cases += [("docs/perf/workloads/mbstring_profiles_2831.php", [mode, size],
               f"workload-{mode}-{size}.stdout", 0)
              for mode in ("convert", "mime", "cut", "http") for size in ("small", "large")]
    receipts = []
    DATA.mkdir(exist_ok=True, parents=True)
    for path, arguments, filename, expected_status in cases:
        source = (ROOT / path).read_bytes()
        command = args + [path] + arguments
        run = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, timeout=60)
        if (ROOT / path).read_bytes() != source or binary.read_bytes() != before:
            raise ValueError("content moved during observation")
        if run.returncode != expected_status or run.stderr:
            raise ValueError(f"unexpected observation: {path}: {run.returncode}: {run.stderr!r}")
        encoding = "hex" if filename.endswith(".hex") else "raw-utf8"
        stored = (run.stdout.hex() + "\n").encode() if encoding == "hex" else run.stdout
        (DATA / filename).write_bytes(stored)
        receipts.append(dict(input=path, input_sha256=sha(source), arguments=arguments,
                             command=command, env=env, exit_status=run.returncode,
                             stdout=filename, stdout_encoding=encoding, stdout_sha256=sha(run.stdout),
                             stored_sha256=sha(stored), stderr_sha256=sha(run.stderr)))
    return dict(kind="non-timed bounded oracle observation, not witness or speed acceptance",
                oracle_path=str(binary), oracle_md5=md5, oracle_sha256=sha(before),
                host=os.uname().machine + "-apple-darwin", cases=receipts)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", required=True)
    parser.add_argument("--md5", required=True)
    args = parser.parse_args()
    (DATA / "receipts.json").write_text(json.dumps(capture(args.oracle, args.md5), indent=2) + "\n")
