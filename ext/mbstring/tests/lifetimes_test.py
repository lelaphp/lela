"""Compile-failure evidence against the actual built native face, not a mock."""
from pathlib import Path
import subprocess
import unittest
import hashlib
import json

ROOT = Path(__file__).resolve().parents[3]
DEPS = ROOT / "target/debug/deps"


class Lifetimes(unittest.TestCase):
    def test_borrows_must_end_before_growth_free_or_a_second_request_borrow(self):
        command = ["cargo", "build", "-p", "php_ext", "--features", "test", "--message-format=json"]
        built = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=120)
        self.assertEqual(built.returncode, 0, built.stderr)
        artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith("{")]
        libraries = [Path(filename) for row in artifacts if row.get("reason") == "compiler-artifact"
                     and row["target"]["name"] == "php_ext" for filename in row["filenames"]
                     if filename.endswith(".rlib")]
        self.assertEqual(len(libraries), 1)
        library = libraries[0]
        cases = {"borrow_then_grow": "E0505", "borrow_then_free": "E0505",
                 "borrow_across_request": "E0499", "valid_scope": None}
        out = ROOT / "target/t2831-proof/lifetimes"
        out.mkdir(parents=True, exist_ok=True)
        for name, diagnostic in cases.items():
            with self.subTest(name=name):
                source = Path(__file__).with_name("lifetimes") / (name + ".rs")
                run = subprocess.run(["rustc", "--edition=2024", "--crate-type=lib", "--emit=metadata",
                                      "--extern", f"php_ext={library}", "-L", f"dependency={DEPS}",
                                      "--out-dir", str(out), str(source)], capture_output=True, text=True)
                (out / (name + ".stderr")).write_text(run.stderr)
                if diagnostic is None:
                    self.assertEqual(run.returncode, 0, run.stderr)
                else:
                    self.assertNotEqual(run.returncode, 0)
                    self.assertIn(diagnostic, run.stderr)
        (out / "receipt.json").write_text(json.dumps(dict(build=command, library=str(library),
                                                        sha256=hashlib.sha256(library.read_bytes()).hexdigest(),
                                                        cases=cases), indent=2) + "\n")


if __name__ == "__main__":
    unittest.main()
