# Distribution inputs

Build with the checkout as the context, without a php-src checkout:

```sh
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target binaries --output /tmp/dist-arm64 .
docker buildx build -f dist/Dockerfile --platform linux/amd64 --target binaries --output /tmp/dist-amd64 .
```

Run those builds sequentially on small builders. The default Cargo job count is
one. The installer runs on BUILDARCH; the compiled hosts and their libraries
follow TARGETARCH. Product naming remains governed by ADR-0016.

## The compiled tier

The hosts carry the compiled tier: `sapi/cli`, `sapi/fpm` and `sapi/makala`
each turn `php_bundle`'s `jit` feature on by default, so the release build
needs no flag for it. `opcache.jit` defaults to `disable`, as php's does, so a
request runs on the interpreter until `opcache.jit=tracing` (with
`opcache.enable_cli=1` in the CLI) asks for the tier. Tracing is experimental:
it sits outside every supported combination (ADR-0017). A host built with
`--no-default-features` carries no tier.

## Verify and refresh

`dist/inputs.toml` holds the full php-src commit, both ini SHA-256 values, the
versioned rustup installer SHA-256 for each build architecture, and the SHA-256
of `rust-toolchain.toml`. `dist/inputs.py` compares the source identity and
toolchain file before downloading, then verifies each download before use.
A mismatch exits nonzero with the file to refresh. Docker runs that same
verifier; a cached layer is invalidated when an input or pin changes.

Run the offline tests and the checkout's pin comparison with Python 3.11+:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s dist -p 'test_*.py' -v
python3 dist/inputs.py check
```

The tests exercise byte rejection, incomplete downloads, oracle commit/tag/version
drift, toolchain channel/component drift, and both installer architectures.
They do not build or run platform binaries.

To exercise real downloads without installing anything:

```sh
python3 dist/inputs.py ini /tmp/dist-inputs/ini
python3 dist/inputs.py rustup amd64 /tmp/dist-inputs/amd64/rustup-init
python3 dist/inputs.py rustup arm64 /tmp/dist-inputs/arm64/rustup-init
docker buildx build -f dist/Dockerfile --target ini .
```

When a person moves the oracle, resolve its source tag to the full commit in a
trusted php-src checkout (`git rev-parse '<tag>^{commit}'`). Confirm that commit
extends `oracle.toml`'s abbreviated commit and that the tag matches the oracle
version. Hash each ini file at that commit with SHA-256 and review the bytes.
Update the tag, full commit and ini hashes together in `dist/inputs.toml`.
The verifier compares all three source/version fields, so a tag-only move
also refuses stale pins. It does not contact a mutable remote tag at build time;
the reviewed tag-to-commit relationship is recorded in the pin file.

After a reviewed toolchain change, update its file SHA-256 in `dist/inputs.toml`.
Docker installs the channel and components from that exact toolchain file.
For a rustup update, use the versioned
`https://static.rust-lang.org/rustup/archive/<version>/<host>/rustup-init.sha256`
for both hosts listed in the pin file, then verify the downloaded installers
against those published hashes. Do not derive a new trusted hash solely from
the download being installed.

The Dockerfile frontend and Ubuntu base use multi-platform image index digests
in `dist/Dockerfile`. Inspect replacement indices for both linux/amd64 and
linux/arm64 before updating them. An intentional build-argument override of
UBUNTU supplies a different base and is the caller's responsibility.

## Verification bounds

The direct curl downloads (installers and ini files) must match repository-held
SHA-256 values before use. HTTPS is required across redirects. Docker verifies
the frontend and base image content through their index digests.

Apt verifies signed repository metadata and package SHA-256 values. Package
versions and repository snapshots are not frozen here. Rustup verifies component
archives against the selected release manifest's hashes; that manifest is
retrieved over HTTPS and is not independently pinned in this repository.
Cargo uses `--locked` and the lockfile's registry checksums. These checks do
not make the whole build hermetic or guarantee byte-identical binaries.

Successful input tests do not supply the release's binary smoke evidence.
Both `binaries` targets must still be built and their `bin/php -v` run with
the matching runtime libraries on the release runners. Apache validation
also requires building its target and exercising the produced image.
