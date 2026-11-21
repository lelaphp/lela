"""The pinned php-src tree on this machine, found as xtask's `Source::found`
(xtask/src/oracle/mod.rs) finds it: the directory `TEST_PHP_SRCDIR` names, a
leading `~/` or `$HOME/` read as the home directory and an empty value as
unset, else `[source]`'s `path` where the pin states one; where neither is a
directory, a refusal names both places.

The one copy of the rule in Python. scripts/fileinfo-proof.py,
scripts/oracle-build.py and dist/test_inputs.py load it by path with
`runpy.run_path`, which writes no bytecode beside it: they run under
`python3 -I`, which leaves a script's own directory off `sys.path`, so an
import of a sibling would not find it.
"""

from pathlib import Path

VARIABLE = "TEST_PHP_SRCDIR"


def places(source, environ):
    """What the rule reads, as `(named, read, pinned)`: the variable's value,
    None where it is unset or empty; that value with a leading `~/` or
    `$HOME/` read as `HOME`, taken as written otherwise and where `HOME` is
    unset or empty, as xtask's `expanded` reads it; and the pin's path, None
    where `source` states none."""
    named = environ.get(VARIABLE) or None
    home = environ.get("HOME") or None
    lead = next((lead for lead in ("~", "$HOME") if named and named.startswith(lead + "/")), None)
    if named is None:
        read = None
    elif lead and home:
        read = Path(home) / Path(named).relative_to(lead)
    else:
        read = Path(named)
    pinned = Path(source["path"]) if "path" in source else None
    return named, read, pinned


def found(source, environ):
    """The tree, the first of the variable's reading and the pin's path that
    is a directory; None where neither is, for a reader that skips."""
    _, read, pinned = places(source, environ)
    return next((tree for tree in (read, pinned) if tree is not None and tree.is_dir()), None)


def refusal(source, environ):
    """What a reader that cannot go on without the tree says where `found`
    is None, naming both places as xtask's refusal does."""
    named, read, pinned = places(source, environ)
    if named is None:
        variable = f"{VARIABLE} is unset"
    elif read != Path(named):
        variable = f"{VARIABLE}={named}, read as {read}, is not a directory"
    else:
        variable = f"{VARIABLE}={named} is not a directory"
    path = "the pin states no `[source].path`" if pinned is None else f"the pin's path {pinned} is not a directory"
    return f"the pinned php-src ({source['tag']}) is at neither place tried: {variable}, and {path}"
