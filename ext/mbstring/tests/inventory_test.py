"""Offline proof at the extraction boundary; never a product codec."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
import copy
import sys
import json

sys.dont_write_bytecode = True

SPEC = importlib.util.spec_from_file_location("inventory", Path(__file__).with_name("inventory.py"))
inventory = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(inventory)
ROOT = Path(__file__).resolve().parents[1]


class InventoryProof(unittest.TestCase):
    def test_numeric_identity_is_not_supported_position(self):
        rows = inventory.collect(ROOT)["encodings"]
        self.assertEqual(len(rows), 79)
        utf8 = next(row for row in rows if row["name"] == "UTF-8")
        self.assertEqual((utf8["identity"], utf8["supported_position"]), (21, 18))

    def test_missing_source_is_not_empty_input(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "missing input"):
                inventory.collect(Path(directory))

    def test_names_mime_and_ordered_aliases_match_independent_php_capture(self):
        rows = inventory.collect(ROOT)["encodings"]
        oracle = json.loads((ROOT.parents[1] / "data/mbstring-2831/inventory-oracle.json").read_text())
        self.assertEqual([{k: r[k] for k in ("name", "mime", "aliases")} for r in rows], oracle["encodings"])

    def test_unsupported_expression_is_not_dropped(self):
        for token in ("1 << 4", "UNICODE_MAX", "sizeof(int)", "0x10 + 1"):
            with self.assertRaisesRegex(ValueError, "unsupported expression"):
                inventory.integer(token)

    def test_stale_metadata_is_refused(self):
        current = inventory.collect(ROOT)
        pinned = inventory.receipt(copy.deepcopy(current))
        pinned["encodings"][0]["identity"] = 999
        with self.assertRaisesRegex(ValueError, "stale inventory"):
            inventory.validate(current, pinned)

    def test_changed_or_missing_fork_input_requires_classification(self):
        current = inventory.collect(ROOT)
        pinned = inventory.receipt(copy.deepcopy(current))
        pinned["sources"][0]["sha256"] = "changed"
        with self.assertRaisesRegex(ValueError, "unclassified fork change"):
            inventory.validate(current, pinned)
        path = pinned["sources"][0]["path"]
        with self.assertRaisesRegex(ValueError, "stale inventory"):
            inventory.validate(current, pinned, {path: "algorithm"})
        current["sources"] = current["sources"][1:]
        with self.assertRaisesRegex(ValueError, "missing input"):
            inventory.validate(current, pinned)

    def test_a_classification_is_not_a_metadata_waiver(self):
        current = inventory.collect(ROOT)
        pinned = inventory.receipt(copy.deepcopy(current))
        pinned["encodings"][0]["identity"] = 999
        with self.assertRaisesRegex(ValueError, "does not name a source change"):
            inventory.validate(current, pinned, {"unrelated": "data"})

    def test_unimplemented_and_new_families_never_pass_through(self):
        rows = inventory.collect(ROOT)["encodings"]
        families = {row["family"] for row in rows}
        identities = {row["identity"] for row in rows}
        with self.assertRaisesRegex(ValueError, "unimplemented family"):
            inventory.require_implemented(rows, families, identities)
        rows[0]["rust_implemented"] = True
        with self.assertRaisesRegex(ValueError, "unimplemented family"):
            inventory.require_implemented(rows[:1], set(), {rows[0]["identity"]})

    def test_empty_partial_and_duplicate_inventory_cannot_claim_completion(self):
        rows = inventory.collect(ROOT)["encodings"]
        families = {row["family"] for row in rows}
        identities = {row["identity"] for row in rows}
        for row in rows:
            row["rust_implemented"] = True
        for incomplete in ([], rows[:1], rows[:-1], rows + rows[:1]):
            with self.assertRaisesRegex(ValueError, "encoding coverage"):
                inventory.require_implemented(incomplete, families, identities)
        inventory.require_implemented(rows, families, identities)

    def test_control_flow_is_not_translated_function_coverage(self):
        functions = inventory.collect(ROOT)["functions"]
        self.assertFalse({row["symbol"] for row in functions} & {"if", "while", "for", "switch"})
        self.assertTrue(any(row["symbol"] == "mb_fast_convert" for row in functions))

    def test_numeric_generation_keeps_width_order_and_build_output(self):
        table = next(row for row in inventory.collect(ROOT)["tables"]
                     if row["symbol"] == "iso8859_2_ucs_table")
        self.assertEqual(table["width_bits"], 16)
        self.assertEqual(table["values"][:4], [0xA0, 0x104, 0x2D8, 0x141])
        with self.assertRaisesRegex(ValueError, "build output"):
            inventory.generate_numeric(table, ROOT / "src")
        with self.assertRaisesRegex(ValueError, "build output"):
            inventory.generate_numeric(table, ROOT / "src" / "target")
        with tempfile.TemporaryDirectory(dir=ROOT.parents[1] / "target", prefix="t2831-inventory-") as directory:
            generated = inventory.generate_numeric(table, Path(directory) / "target" / "out")
            self.assertIn("[u16; 96]", generated.read_text())
        table["values"][0] = 65536
        with self.assertRaisesRegex(ValueError, "width"):
            inventory.generate_numeric(table, ROOT / "target" / "out")

    def test_dimension_mismatch_is_not_flattened_away(self):
        table = next(row for row in inventory.collect(ROOT)["tables"]
                     if row["symbol"] == "iso8859_2_ucs_table")
        table["declared_dimensions"] = ["100"]
        with self.assertRaisesRegex(ValueError, "dimensions"):
            inventory.generate_numeric(table, ROOT / "target" / "out")


if __name__ == "__main__":
    unittest.main()
