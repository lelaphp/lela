"""Negative controls for #2827's receipt comparison, with no oracle execution."""
import copy
import json
from pathlib import Path
import runpy
import unittest

ROOT = Path(__file__).resolve().parents[2]
PROOF = runpy.run_path(str(ROOT / "scripts/fileinfo-proof.py"))


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
