"""The deletion script, held by its dry run over a listing fixture.

fixtures/ghcr-versions.json is the package's versions as GitHub's API
lists them, and fixtures/ghcr-manifests.json each version's manifest as
the registry serves it. Against 2026-11-20T00:00:00Z, thirty days back is
2026-10-21T00:00:00Z, and the versions are:

     1      an old commit of main's cli list, `sha-` tagged       deleted
     2-4    its two images and its attestation, untagged, old     deleted
     5      a young commit's list, `sha-` tagged                   kept
     6-7    its images, untagged, young                            kept
     8      an old release's list, `1.0.0-cli` and `cli`           kept
     9-10   its images, untagged, old                              kept, the list names them
     11     a referrer whose subject is that list, untagged, old   kept, it names the list
     12     an old list under `edge-fpm` and a `sha-` tag          kept
     13     an old untagged manifest nothing names                 deleted
     14     a young untagged manifest                              kept
     15     an old pre-release's list, `1.1.0-rc.1-apache`         kept
     16     a `sha-` list updated exactly thirty days back         kept, not older
     17     a referrer whose subject is list 1, untagged, old      deleted
     18     an old tag the publish script never writes             kept
     19     a `sha-` list created long ago and updated a week ago  kept, its age is the update's
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "dist/fixtures"
NOW = "2026-11-20T00:00:00Z"
DELETED = {1, 2, 3, 4, 13, 17}


def digest(n):
    return f"sha256:{n:064x}"


class Prune(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.bin = Path(self.temporary.name) / "bin"
        self.bin.mkdir()
        self.log = Path(self.temporary.name) / "calls"

    def prune(self, *arguments, listing=FIXTURES / "ghcr-versions.json"):
        # No gh and no docker on PATH: the fixtures are all it reads.
        return subprocess.run(
            [
                sys.executable, str(ROOT / "dist/prune.py"),
                "--listing", str(listing), "--manifests", str(FIXTURES / "ghcr-manifests.json"),
                "--now", NOW, *arguments,
            ],
            env={**os.environ, "PATH": str(self.bin), "CALLS": str(self.log)},
            capture_output=True, text=True,
        )

    def named(self, run):
        self.assertEqual(run.returncode, 0, run.stderr)
        return [line.split() for line in run.stdout.splitlines() if line.startswith("delete ")]

    def test_the_dry_run_names_only_old_sha_tags_and_old_untagged_manifests(self):
        run = self.prune("--dry-run")
        named = self.named(run)
        self.assertEqual({int(fields[1]) for fields in named}, DELETED)
        for fields in named:
            with self.subTest(version=fields[1]):
                self.assertEqual(fields[2], digest(int(fields[1])))
                tags = fields[3].split(",")
                self.assertTrue(tags == ["untagged"] or all(t.startswith("sha-") for t in tags), tags)
        self.assertIn("dry run: nothing is deleted", run.stdout)
        self.assertIn(f"6 of 19 versions to delete", run.stdout)
        self.assertFalse(self.log.exists())

    def test_no_version_tag_bare_variant_or_edge_is_deleted_however_old(self):
        listing = json.loads((FIXTURES / "ghcr-versions.json").read_text())
        for version in listing:
            version["created_at"] = version["updated_at"] = "2020-01-01T00:00:00Z"
        path = Path(self.temporary.name) / "old.json"
        path.write_text(json.dumps(listing))
        kept = {
            int(v["id"]) for v in listing
            if any(not t.startswith("sha-") for t in v["metadata"]["container"]["tags"])
        }
        self.assertEqual(kept, {8, 12, 15, 18})
        deleted = {int(fields[1]) for fields in self.named(self.prune("--dry-run", listing=path))}
        # The release's images and its referrer stay with it; the rest go.
        self.assertEqual(deleted, set(range(1, 20)) - kept - {9, 10, 11})

    def test_a_manifest_the_registry_does_not_serve_deletes_nothing(self):
        listing = json.loads((FIXTURES / "ghcr-versions.json").read_text())
        listing.append({**listing[0], "id": 99, "name": digest(99)})
        path = Path(self.temporary.name) / "unknown.json"
        path.write_text(json.dumps(listing))
        run = self.prune(listing=path)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"no manifest for {digest(99)}", run.stderr)
        self.assertFalse(self.log.exists())

    def test_a_real_run_deletes_what_the_dry_run_names_through_the_api(self):
        gh = self.bin / "gh"
        gh.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "with open(os.environ['CALLS'], 'a') as log:\n"
            "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
        )
        gh.chmod(0o755)
        run = self.prune()
        self.assertEqual(run.returncode, 0, run.stderr)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        prefix = ["api", "--method", "DELETE"]
        self.assertTrue(all(call[:3] == prefix for call in calls), calls)
        order = [int(call[3].removeprefix("/orgs/lelaphp/packages/container/lela/versions/")) for call in calls]
        self.assertEqual(set(order), DELETED)
        self.assertEqual(len(order), len(DELETED))
        # A list goes before the images it names, so none is left naming
        # a manifest already gone.
        self.assertLess(order.index(1), min(order.index(n) for n in (2, 3, 4)))


# A gh that records each call, answers a listing with GH_LISTING's file or
# fails it as an expired token does, and fails the DELETE of version FAILS.
GH = r'''#!/usr/bin/env python3
import json, os, sys
arguments = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(arguments) + "\n")
if "--paginate" in arguments:
    if "GH_LISTING" not in os.environ:
        sys.exit("gh: HTTP 401: Bad credentials (https://api.github.com/orgs/lelaphp/packages)")
    print(json.dumps([json.load(open(os.environ["GH_LISTING"]))]))
elif arguments[-1].endswith("/" + os.environ.get("FAILS", "-")):
    sys.exit("gh: HTTP 404: Not Found")
'''


class Diagnostics(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.bin = Path(self.temporary.name) / "bin"
        self.bin.mkdir()
        self.log = Path(self.temporary.name) / "calls"

    def gh(self):
        (self.bin / "gh").write_text(GH.replace("/usr/bin/env python3", sys.executable))
        (self.bin / "gh").chmod(0o755)

    def prune(self, *arguments, **env):
        return subprocess.run(
            [sys.executable, str(ROOT / "dist/prune.py"), "--now", NOW, *arguments],
            env={**os.environ, "PATH": str(self.bin), "CALLS": str(self.log), **env},
            capture_output=True, text=True,
        )

    def test_a_listing_gh_cannot_give_deletes_nothing_and_says_why(self):
        self.gh()
        run = self.prune("--manifests", str(FIXTURES / "ghcr-manifests.json"))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("exit 1: gh: HTTP 401: Bad credentials", run.stderr)
        self.assertIn("nothing is deleted", run.stderr)
        self.assertNotIn("Traceback", run.stderr)

    def test_a_gh_not_on_path_is_said_and_no_traceback(self):
        run = self.prune("--manifests", str(FIXTURES / "ghcr-manifests.json"))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertEqual(run.stderr, "dist/prune.py: gh: not found; nothing is deleted\n")

    def test_a_deletion_that_fails_partway_names_what_went_and_what_did_not(self):
        self.gh()
        run = self.prune(
            "--manifests", str(FIXTURES / "ghcr-manifests.json"),
            GH_LISTING=str(FIXTURES / "ghcr-versions.json"), FAILS="13",
        )
        self.assertEqual(run.returncode, 1, run.stdout)
        planned = [int(line.split()[1]) for line in run.stdout.splitlines() if line.startswith("delete ")]
        went = [int(line.split()[1]) for line in run.stdout.splitlines() if line.startswith("deleted ")]
        self.assertEqual(sorted(planned), sorted(DELETED))
        at = planned.index(13)
        self.assertGreater(at, 0)
        self.assertEqual(went, planned[:at])
        self.assertIn(
            f"{at} of {len(planned)} versions deleted ({' '.join(map(str, went))}), "
            f"and these not: {' '.join(map(str, planned[at:]))}",
            run.stderr,
        )
        self.assertIn("gh: HTTP 404: Not Found", run.stderr)
        deletes = [json.loads(line) for line in self.log.read_text().splitlines() if "DELETE" in line]
        self.assertEqual(len(deletes), at + 1)


if __name__ == "__main__":
    unittest.main()
