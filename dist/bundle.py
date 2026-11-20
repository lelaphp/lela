"""The libraries the macOS hosts load, bundled beside them in the tarball.

dist/release.py's darwin staging calls `bundle(stage, hosts)` once the hosts
are in the stage; its dry run calls `closure(hosts)`, which copies nothing.
Each Mach-O is read with `otool -l`, the one reading of a binary here: the
libraries it loads (LC_LOAD_DYLIB and its weak, re-export, lazy and upward
kinds), its own install name (LC_ID_DYLIB), its run paths (LC_RPATH) and the
oldest macOS it runs on (LC_BUILD_VERSION's minos, which every arm64 binary
carries). Of a universal file otool reads this machine's slice alone, and
install_name_tool rewrites every slice.

A name under /usr/lib or /System is the system's and stays as it is. Every
other is resolved as dyld resolves it: an absolute path as it is,
`@loader_path` from the directory of the image that names it,
`@executable_path` from the host's, and `@rpath` through the run paths of
that image and then of each image that loaded it, back to the host. The real
file it names, through every symlink, is copied into lib/ under the file
name of its own install name, the name the references use, and is walked in
turn, so lib/ holds the closure. A name found nowhere is refused, saying
which image names it.

Each name is then rewritten relative to the image that carries it: a host's
to `@loader_path/../lib/<name>`, a library's to `@loader_path/<name>`, a
library's own install name to `@rpath/<name>`, and every run path that is
not the system's is deleted, since no name needs one. arm64 refuses a binary
whose signature no longer matches, so every rewritten file is signed again,
ad hoc. The stage is then read again, and a name still outside it and the
system's is refused (`outside`), the hold dist/smoke.sh makes again over the
unpacked tarball.

Each library comes from a Homebrew keg, `<prefix>/Cellar/<keg>/<version>/`,
which gives `bundled-libraries.txt` the keg and the version and
`licenses/<keg>/` the licence text: the keg's LICENSE*, LICENCE*, COPYING*
and NOTICE* files. A library in no keg, and a keg with no licence file, are
refused by name. The list's first line is `macos <version>`, the oldest
macOS the tarball runs on, the highest minimum any Mach-O in it names; each
line after it is `lib/<name> <keg> <version>`.
"""

import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from publish import Refusal  # noqa: E402

SYSTEM = ("/usr/lib/", "/System/")
LOADS = ("LC_LOAD_DYLIB", "LC_LOAD_WEAK_DYLIB", "LC_REEXPORT_DYLIB", "LC_LAZY_LOAD_DYLIB", "LC_LOAD_UPWARD_DYLIB")
LICENCES = ("LICENSE", "LICENCE", "COPYING", "NOTICE")
LIST = "bundled-libraries.txt"
# The first bytes of a Mach-O: thin, 64-bit or 32-bit, or universal.
MAGIC = (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf")


@dataclass(frozen=True)
class MachO:
    """What one Mach-O names: its own install name, the libraries it
    loads, its run paths and the oldest macOS it runs on."""

    id: str | None
    names: tuple[str, ...]
    rpaths: tuple[str, ...]
    minos: str


def parse(path, text):
    """`otool -l`'s text for `path`: a field line is `<key> <value>`, a
    name or a path followed by `(offset <n>)`, under `Load command <n>`."""
    commands = []
    for line in text.splitlines()[1:]:
        if line.startswith("Load command "):
            commands.append({})
            continue
        field = re.match(r"\s*(\S+) (.*)$", line)
        if commands and field:
            value = re.sub(r" \(offset \d+\)$", "", field.group(2))
            commands[-1].setdefault(field.group(1), value)
    ids = [c["name"] for c in commands if c.get("cmd") == "LC_ID_DYLIB"]
    minos = [c["minos"] for c in commands if c.get("cmd") == "LC_BUILD_VERSION"]
    if not minos:
        raise Refusal(f"{path} carries no LC_BUILD_VERSION, which names the oldest macOS it runs on")
    return MachO(
        id=ids[0] if ids else None,
        names=tuple(c["name"] for c in commands if c.get("cmd") in LOADS),
        rpaths=tuple(c["path"] for c in commands if c.get("cmd") == "LC_RPATH"),
        minos=minos[0],
    )


def read(path):
    run = subprocess.run(["otool", "-l", str(path)], check=True, capture_output=True, text=True)
    return parse(path, run.stdout)


def system(name):
    return name.startswith(SYSTEM)


def expand(name, image, host):
    """`name` with its `@loader_path` or `@executable_path` made the
    directory of `image` or of `host`; None for another `@` form."""
    for prefix, base in (("@loader_path/", image), ("@executable_path/", host)):
        if name.startswith(prefix):
            return os.path.normpath(os.path.join(os.path.dirname(base), name.removeprefix(prefix)))
    return None if name.startswith("@") else name


def resolve(name, chain, host):
    """The file `name` loads from the last image of `chain`, each image a
    (path, MachO) from the host down, as dyld finds it; None where it is
    nowhere."""
    if name.startswith("@rpath/"):
        rest = name.removeprefix("@rpath/")
        candidates = [
            expand(f"{rpath}/{rest}", path, host) for path, macho in reversed(chain) for rpath in macho.rpaths
        ]
    else:
        candidates = [expand(name, chain[-1][0], host)]
    return next((Path(c) for c in candidates if c and os.path.isfile(c)), None)


@dataclass(frozen=True)
class Image:
    """A host or a library, by the path it was first reached by, with what
    it names and the real file each name it loads outside the system is."""

    path: Path
    macho: MachO
    links: dict


@dataclass(frozen=True)
class Library:
    """A library to bundle: its real file, its file name under lib/, and
    the keg and version it is copied from, whose directory holds its
    licence."""

    real: Path
    name: str
    keg: str
    version: str
    root: Path


def walk(hosts):
    """Every image the hosts load, by real path, the hosts first."""
    images = {}
    queue = [(Path(host), (), Path(host)) for host in hosts]
    while queue:
        path, parents, host = queue.pop(0)
        real = Path(os.path.realpath(path))
        if real in images:
            continue
        macho = read(path)
        chain = (*parents, (path, macho))
        links = {}
        for name in macho.names:
            if system(name):
                continue
            found = resolve(name, chain, host)
            if found is None:
                raise Refusal(f"{path} loads {name}, which is not found from there")
            links[name] = Path(os.path.realpath(found))
            queue.append((found, chain, host))
        images[real] = Image(path, macho, links)
    return images


def keg(real):
    """The keg a library's real file is in: <prefix>/Cellar/<keg>/<version>/."""
    parts = real.parts
    if "Cellar" not in parts[:-3]:
        raise Refusal(f"{real} is in no Homebrew keg, <prefix>/Cellar/<keg>/<version>/, "
                      "so its version and its licence are not known")
    at = parts.index("Cellar")
    return parts[at + 1], parts[at + 2], Path(*parts[:at + 3])


def libraries(images, hosts):
    """The images that are no host, each with its name under lib/: its own
    install name's file name, or the name it was first loaded by."""
    hosts = {Path(os.path.realpath(h)) for h in hosts}
    loaded = {}
    for image in images.values():
        for name, real in image.links.items():
            loaded.setdefault(real, name)
    placed = {}
    for real, image in images.items():
        if real in hosts:
            continue
        name = os.path.basename(image.macho.id or loaded[real])
        other = next((l.real for l in placed.values() if l.name == name), None)
        if other is not None:
            raise Refusal(f"{real} and {other} would both be lib/{name}")
        placed[real] = Library(real, name, *keg(real))
    return placed


def closure(hosts):
    """What `bundle` would copy for the hosts: the libraries, by name."""
    return sorted(libraries(walk(hosts), hosts).values(), key=lambda library: library.name)


def plan(stage, images, placed):
    """The `install_name_tool` arguments for each file of the stage it
    rewrites: every name the file loads outside the system made relative
    to the file, a library's own install name made `@rpath/<name>`, and
    every run path outside the system's deleted."""
    lib = stage / "lib"
    rewrites = {}
    for real, image in images.items():
        library = placed.get(real)
        file = lib / library.name if library else Path(image.path)
        here = os.path.relpath(lib, file.parent)
        arguments = [] if library is None or image.macho.id is None else ["-id", f"@rpath/{library.name}"]
        for name, target in image.links.items():
            to = placed[target].name
            arguments += ["-change", name, f"@loader_path/{to}" if here == "." else f"@loader_path/{here}/{to}"]
        for rpath in image.macho.rpaths:
            if not system(rpath):
                arguments += ["-delete_rpath", rpath]
        if arguments:
            rewrites[file] = arguments
    return rewrites


def machos(directory):
    """Every Mach-O file under `directory`, by its first bytes, sorted."""
    found = []
    for path in sorted(Path(directory).rglob("*")):
        if path.is_file() and not path.is_symlink():
            with path.open("rb") as file:
                if file.read(4) in MAGIC:
                    found.append(path)
    return found


def outside(stage):
    """Each name a Mach-O of the stage carries that is neither the system's
    nor a file of the stage by `@loader_path`, nor a library's own install
    name `@rpath/<its file name>`: `<file>: <name>` lines."""
    stage = Path(os.path.realpath(stage))
    left = []
    for path in machos(stage):
        macho = read(path)
        relative = path.relative_to(stage)
        if macho.id is not None and macho.id != f"@rpath/{path.name}":
            left.append(f"{relative}: {macho.id}")
        for name in macho.names:
            target = expand(name, path, path) if name.startswith("@loader_path/") else None
            inside = target is not None and os.path.isfile(target) and \
                Path(os.path.realpath(target)).is_relative_to(stage)
            if not (system(name) or inside):
                left.append(f"{relative}: {name}")
        left += [f"{relative}: run path {rpath}" for rpath in macho.rpaths if not system(rpath)]
    return left


def oldest(stage):
    """The highest minimum macOS any Mach-O of the stage names."""
    versions = [read(path).minos for path in machos(stage)]
    return max(versions, key=lambda version: tuple(int(part) for part in version.split(".")))


def licences(library):
    texts = sorted(
        path for path in library.root.iterdir()
        if path.is_file() and path.name.upper().startswith(LICENCES)
    )
    if not texts:
        raise Refusal(f"the keg {library.keg} {library.version} carries no licence text, "
                      f"no {', '.join(name + '*' for name in LICENCES[:-1])} or {LICENCES[-1]}* file in {library.root}")
    return texts


def bundle(stage, hosts):
    """The hosts' libraries copied into lib/ of the stage, every name
    rewritten and every rewritten file signed again ad hoc, the kegs'
    licences under licenses/ and the list beside them. Returns the list's
    lines."""
    stage = Path(stage)
    images = walk(hosts)
    placed = libraries(images, hosts)
    for library in placed.values():
        licences(library)
    (stage / "lib").mkdir(exist_ok=True)
    for library in placed.values():
        shutil.copyfile(library.real, stage / "lib" / library.name)
        (stage / "lib" / library.name).chmod(0o644)
    rewrites = plan(stage, images, placed)
    for file, arguments in rewrites.items():
        subprocess.run(["install_name_tool", *arguments, str(file)], check=True, capture_output=True, text=True)
    for file in rewrites:
        subprocess.run(["codesign", "--force", "--sign", "-", str(file)], check=True, capture_output=True, text=True)
    left = outside(stage)
    if left:
        raise Refusal("the stage still names what is outside it and the system's:\n" + "\n".join(left))
    for library in placed.values():
        (stage / "licenses" / library.keg).mkdir(parents=True, exist_ok=True)
        for text in licences(library):
            shutil.copyfile(text, stage / "licenses" / library.keg / text.name)
    lines = [f"macos {oldest(stage)}"] + [
        f"lib/{library.name} {library.keg} {library.version}"
        for library in sorted(placed.values(), key=lambda library: library.name)
    ]
    (stage / LIST).write_text("".join(f"{line}\n" for line in lines))
    return lines
