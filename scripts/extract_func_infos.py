#!/usr/bin/env python3
"""extract_func_infos.py -- derive data/func-infos.toml from a php-src checkout.

Usage:  python3 scripts/extract_func_infos.py /path/to/php-src > data/func-infos.toml

Reads (relative to the php-src root given in argv[1]):
  main/php_version.h                  the php version, printed as the tag
  Zend/Optimizer/zend_func_infos.h    func_infos[], what php's optimizer knows an
                                      internal function returns beyond its
                                      declared type (zend_get_internal_func_info)

Each row is one F1(...) or FN(...) of the table, in the table's order: the
function's name, the reference counts the macro adds (F1: MAY_BE_RC1, FN:
MAY_BE_RC1|MAY_BE_RCN), and the MAY_BE_* words of its mask with the prefix
dropped. A row inside an `#if` keeps the condition as `when`; php_ssa's
build.rs renders every row, since a function the engine does not declare is
never looked up. The one callback php keeps beside the table, range()'s
(zend_func_info.c, old_func_infos), is code and stays in php_ssa.
Nothing here is a judgment; the output is not edited by hand.
"""

import os
import re
import sys

ROW = re.compile(r'^\s*(F0|F1|FN)\("([^"]+)",\s*([A-Z0-9_|]+)\),\s*$')
COUNTS = {"F0": "0", "F1": "1", "FN": "n"}


def version(root):
    text = open(os.path.join(root, "main/php_version.h")).read()
    m = re.search(r'#define PHP_VERSION "([^"]+)"', text)
    if not m:
        sys.exit("php_version.h: no PHP_VERSION")
    return m.group(1)


def rows(root):
    path = os.path.join(root, "Zend/Optimizer/zend_func_infos.h")
    when = []
    out = []
    for n, line in enumerate(open(path), 1):
        stripped = line.strip()
        if stripped.startswith("#if"):
            when.append(stripped[3:].strip())
            continue
        if stripped.startswith("#endif"):
            if not when:
                sys.exit(f"zend_func_infos.h:{n}: #endif without #if")
            when.pop()
            continue
        if stripped.startswith("#"):
            sys.exit(f"zend_func_infos.h:{n}: unread directive {stripped}")
        if not stripped.startswith(("F0(", "F1(", "FN(", "FC(")):
            continue
        m = ROW.match(line)
        if not m:
            sys.exit(f"zend_func_infos.h:{n}: unread row {stripped}")
        macro, name, mask = m.groups()
        words = []
        for word in mask.split("|"):
            if not word.startswith("MAY_BE_"):
                sys.exit(f"zend_func_infos.h:{n}: {word} is no MAY_BE_ word")
            words.append(word[len("MAY_BE_"):])
        out.append((name, COUNTS[macro], words, " && ".join(when)))
    if when:
        sys.exit("zend_func_infos.h: an #if left open")
    return out


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = sys.argv[1]
    table = rows(root)
    print("# What php's optimizer knows an internal function returns beyond its")
    print("# declared type: Zend/Optimizer/zend_func_infos.h's func_infos[], one")
    print("# row per F1/FN, by scripts/extract_func_infos.py. Not edited by hand.")
    print(f'version = "php-{version(root)}"')
    print(f"rows = {len(table)}")
    for name, count, words, when in table:
        print()
        print("[[function]]")
        print(f'name = "{name}"')
        print(f'count = "{count}"')
        print("types = [" + ", ".join(f'"{w}"' for w in words) + "]")
        if when:
            print('when = "' + when.replace("\\", "\\\\").replace('"', '\\"') + '"')


if __name__ == "__main__":
    main()
