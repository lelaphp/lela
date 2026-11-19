#!/usr/bin/env python3
"""Delete the versions of ghcr.io/lelaphp/lela no tag will be pulled by.

    dist/prune.py [--dry-run] [--listing <file>] [--manifests <file>] [--now <time>]

A version is deleted when it was last updated more than 30 days before
now and each of its tags is a `sha-` tag, or it has none, unless a kept
version needs it: the images and attestations a kept manifest list names,
and a referrer whose subject is kept, such as an attestation pushed to the
registry. A version under any other tag, a release's `<version>-<variant>`,
a bare `<variant>`, an `edge-<variant>` or one this script did not write,
is never deleted, however old.

It lists the versions with `gh api` and reads each one's manifest with
`docker buildx imagetools inspect --raw`, and deletes through the API, so
it runs where gh holds a token with `delete:packages` and docker is logged
in to ghcr.io. --listing reads the versions from a file of the API's
answer instead, and --manifests the manifests from a file mapping each
digest to its manifest; --now fixes the time the age is counted from.
A manifest it cannot read deletes nothing. --dry-run names each version
it would delete, `delete <id> <digest> <tags> <updated>`, and deletes none.
"""

import argparse
import json
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

ORG = "lelaphp"
PACKAGE = "lela"
IMAGE = f"ghcr.io/{ORG}/{PACKAGE}"
VERSIONS = f"/orgs/{ORG}/packages/container/{PACKAGE}/versions"
AGE = timedelta(days=30)
# The tags a manifest list's own `sha-` tag deletes with it; any other kind
# of tag keeps its version.
OURS = "sha-"


class Refusal(Exception):
    pass


def when(text):
    return datetime.fromisoformat(text.replace("Z", "+00:00"))


def listed():
    pages = json.loads(subprocess.run(
        ["gh", "api", "--paginate", "--slurp", VERSIONS + "?per_page=100"],
        check=True, capture_output=True, text=True,
    ).stdout)
    return [version for page in pages for version in page]


def served(digest):
    run = subprocess.run(
        ["docker", "buildx", "imagetools", "inspect", "--raw", f"{IMAGE}@{digest}"],
        capture_output=True, text=True,
    )
    if run.returncode != 0:
        raise Refusal(f"no manifest for {digest}: {run.stderr.strip()}")
    return json.loads(run.stdout)


def doomed(versions, manifest, now):
    """The versions to delete, oldest first; `manifest` reads one digest's."""
    kept = set()
    by_digest = {version["name"]: version for version in versions}
    for version in versions:
        tags = version["metadata"]["container"]["tags"]
        if now - when(version["updated_at"]) <= AGE or any(not tag.startswith(OURS) for tag in tags):
            kept.add(version["name"])
    manifests = {digest: manifest(digest) for digest in by_digest}
    # What a kept version names is kept, and so is a referrer of one,
    # until nothing more is.
    while True:
        more = set()
        for digest in kept:
            more.update(entry["digest"] for entry in manifests.get(digest, {}).get("manifests", []))
        for digest, found in manifests.items():
            if found.get("subject", {}).get("digest") in kept:
                more.add(digest)
        if more <= kept:
            break
        kept |= more
    # A list before the manifests it names, so none is left naming one
    # already gone; then the oldest first.
    return sorted(
        (version for version in versions if version["name"] not in kept),
        key=lambda version: (
            "manifests" not in manifests[version["name"]], version["updated_at"], version["id"],
        ),
    )


def main(argv):
    parser = argparse.ArgumentParser(prog="dist/prune.py")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--listing", type=Path)
    parser.add_argument("--manifests", type=Path)
    parser.add_argument("--now", type=when, default=datetime.now(timezone.utc))
    arguments = parser.parse_args(argv)
    sys.stdout.reconfigure(line_buffering=True)
    try:
        versions = json.loads(arguments.listing.read_text()) if arguments.listing else listed()
        if arguments.manifests:
            fixture = json.loads(arguments.manifests.read_text())

            def manifest(digest):
                if digest not in fixture:
                    raise Refusal(f"no manifest for {digest} in {arguments.manifests}")
                return fixture[digest]
        else:
            manifest = served
        deleting = doomed(versions, manifest, arguments.now)
    except Refusal as refusal:
        print(f"dist/prune.py: {refusal}; nothing is deleted", file=sys.stderr)
        return 1
    print(f"now {arguments.now.strftime('%Y-%m-%dT%H:%M:%SZ')}, older than {(arguments.now - AGE).strftime('%Y-%m-%dT%H:%M:%SZ')}")
    for version in deleting:
        tags = ",".join(version["metadata"]["container"]["tags"]) or "untagged"
        print(f"delete {version['id']} {version['name']} {tags} {version['updated_at']}")
    print(f"{len(deleting)} of {len(versions)} versions to delete")
    if arguments.dry_run:
        print("dry run: nothing is deleted")
        return 0
    for version in deleting:
        subprocess.run(["gh", "api", "--method", "DELETE", f"{VERSIONS}/{version['id']}"], check=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
