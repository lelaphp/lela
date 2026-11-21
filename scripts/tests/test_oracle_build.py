"""The oracle build recipe's tree lookup, over a pin written to a scratch directory, with no build and no oracle execution."""
import os
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
RECIPE = runpy.run_path(str(ROOT / "scripts/oracle-build.py"))

PIN = """[oracle]
path = "/nowhere/php"
version = "8.5.12"

[source]
tag = "php-8.5.12"
commit = "136e049a"
phpt_files = 22280
"""


class TreeLookup(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.home = Path(scratch.name)
        self.named = self.home / "php-src-8.5.12"
        self.named.mkdir()

    def pin(self, keys="", tree=None):
        path = self.home / "oracle.toml"
        path.write_text(PIN + keys)
        return RECIPE["Pin"](path, "main", tree)

    def tree(self, pin, environ):
        with mock.patch.dict(os.environ, environ, clear=True):
            return pin.tree()

    def test_with_no_checkout_named_the_tree_is_the_one_the_variable_names_over_a_pin_that_names_none(self):
        self.assertEqual(self.tree(self.pin(), {"TEST_PHP_SRCDIR": str(self.named)}), self.named)
        environ = {"HOME": str(self.home), "TEST_PHP_SRCDIR": "$HOME/php-src-8.5.12"}
        self.assertEqual(self.tree(self.pin(), environ), self.named)

    def test_a_checkout_named_by_tree_comes_before_the_variable(self):
        other = self.home / "corpus"
        self.assertEqual(self.tree(self.pin(tree=other), {"TEST_PHP_SRCDIR": str(self.named)}), other)

    def test_where_neither_names_a_tree_the_refusal_names_both_and_the_flag(self):
        with self.assertRaises(SystemExit) as refused:
            self.tree(self.pin(), {})
        message = str(refused.exception)
        self.assertIn("TEST_PHP_SRCDIR is unset", message)
        self.assertIn("the pin states no `[source].path`", message)
        self.assertIn("--tree", message)
        absent = self.home / "absent"
        with self.assertRaises(SystemExit) as refused:
            self.tree(self.pin(f'path = "{absent}"\n'), {"TEST_PHP_SRCDIR": str(absent)})
        self.assertIn(f"TEST_PHP_SRCDIR={absent} is not a directory", str(refused.exception))
        self.assertIn(f"the pin's path {absent} is not a directory", str(refused.exception))


if __name__ == "__main__":
    unittest.main()
