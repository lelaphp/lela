"""Negative controls for the fileinfo proof's pin check, tree lookup and receipt comparison, with no oracle execution."""
import copy
import json
from pathlib import Path
import runpy
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
PROOF = runpy.run_path(str(ROOT / "scripts/fileinfo-proof.py"))

# The digest of the bytes `php`, as `shasum -a 256` prints it.
PHP_SHA256 = "sha256:12a5d18ee896e59954bdce0f4acc7212eebe03dae1834ef4ce160ac5afa5c4a8"


def oracle(keys):
    """A pin's `[oracle]` table as the file spells it, with `keys` its digest lines."""
    return tomllib.loads('[oracle]\npath = "/nowhere/php"\nversion = "8.5.0"\n' + keys)["oracle"]


class PinCheck(unittest.TestCase):
    def test_a_sha256_pin_holds_its_binary_and_refuses_another(self):
        table = oracle(f'sha256 = "{PHP_SHA256}"\n')
        self.assertEqual(PROOF["held"](table, b"php"), PHP_SHA256)
        with self.assertRaises(SystemExit) as refused:
            PROOF["held"](table, b"cgi")
        self.assertIn(PHP_SHA256, str(refused.exception))

    def test_a_pin_with_no_sha256_is_refused_by_its_table(self):
        with self.assertRaises(SystemExit) as refused:
            PROOF["held"](oracle(""), b"php")
        self.assertIn("[oracle]", str(refused.exception))


def source(keys=""):
    """A pin's `[source]` table as oracle.toml spells it, with no `path`, and `keys` beside it."""
    return tomllib.loads('[source]\ntag = "php-8.5.12"\ncommit = "136e049a"\nphpt_files = 22280\n' + keys)["source"]


class TreeLookup(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.home = Path(scratch.name)
        self.named = self.home / "php-src-8.5.12"
        self.named.mkdir()
        self.pinned = self.home / "pinned"
        self.pinned.mkdir()

    def test_the_tree_is_the_one_the_variable_names_over_a_pin_that_names_none(self):
        found = PROOF["php_src"](source(), {"TEST_PHP_SRCDIR": str(self.named)})
        self.assertEqual(found, self.named)

    def test_a_leading_home_in_the_variable_is_read_as_the_home_directory(self):
        environ = {"HOME": str(self.home), "TEST_PHP_SRCDIR": "~/php-src-8.5.12"}
        self.assertEqual(PROOF["php_src"](source(), environ), self.named)
        environ["TEST_PHP_SRCDIR"] = "$HOME/php-src-8.5.12"
        self.assertEqual(PROOF["php_src"](source(), environ), self.named)

    def test_the_variable_comes_before_the_pins_path_and_the_path_stands_in_where_it_names_no_directory(self):
        pin = source(f'path = "{self.pinned}"\n')
        self.assertEqual(PROOF["php_src"](pin, {"TEST_PHP_SRCDIR": str(self.named)}), self.named)
        self.assertEqual(PROOF["php_src"](pin, {"TEST_PHP_SRCDIR": str(self.home / "absent")}), self.pinned)
        self.assertEqual(PROOF["php_src"](pin, {}), self.pinned)

    def test_where_neither_names_a_tree_the_refusal_names_both(self):
        for environ in [{}, {"TEST_PHP_SRCDIR": ""}]:
            with self.assertRaises(SystemExit) as refused:
                PROOF["php_src"](source(), environ)
            self.assertIn("TEST_PHP_SRCDIR is unset", str(refused.exception))
            self.assertIn("the pin states no `[source].path`", str(refused.exception))
        absent = self.home / "absent"
        with self.assertRaises(SystemExit) as refused:
            PROOF["php_src"](source(f'path = "{absent}"\n'), {"TEST_PHP_SRCDIR": str(absent)})
        self.assertIn(f"TEST_PHP_SRCDIR={absent} is not a directory", str(refused.exception))
        self.assertIn(f"the pin's path {absent} is not a directory", str(refused.exception))
        with self.assertRaises(SystemExit) as refused:
            PROOF["php_src"](source(), {"HOME": str(self.home), "TEST_PHP_SRCDIR": "~/absent"})
        self.assertIn(f"TEST_PHP_SRCDIR=~/absent, read as {absent}, is not a directory", str(refused.exception))


class ReceiptComparison(unittest.TestCase):
    def setUp(self):
        self.receipt = json.loads((ROOT / "data/fileinfo/oracle.json").read_text())

    def test_changed_defined_transaction_is_rejected(self):
        changed = copy.deepcopy(self.receipt)
        changed["observations"][0]["status"] = 99
        self.assertNotEqual(PROOF["comparable"](changed), PROOF["comparable"](self.receipt))

    def test_undefined_sample_is_evidence_not_expected_behaviour(self):
        changed = copy.deepcopy(self.receipt)
        case = next(case for case in changed["observations"] if case["standing"] == "undefined-stat-copy")
        case.update(status=-11, stdout="different freed/uninitialised bytes", stderr="signal evidence")
        self.assertEqual(PROOF["comparable"](changed), PROOF["comparable"](self.receipt))
        self.assertIn("stdout", case)  # the retained evidence was not mutated

    def test_undefined_identity_and_pin_still_bind_the_receipt(self):
        changed = copy.deepcopy(self.receipt)
        case = next(case for case in changed["observations"] if case["standing"] == "undefined-stat-copy")
        case["standing"] = "defined"
        self.assertNotEqual(PROOF["comparable"](changed), PROOF["comparable"](self.receipt))
        changed = copy.deepcopy(self.receipt)
        changed["oracle"]["pin_sha256"] = "unreviewed pin"
        self.assertNotEqual(PROOF["comparable"](changed), PROOF["comparable"](self.receipt))


if __name__ == "__main__":
    unittest.main()
