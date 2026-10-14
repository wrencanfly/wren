#!/usr/bin/env python3
"""Bytes on the wire: Wren object frames vs. git packs, for the same objects.

Models what each protocol sends for a checkout, from a git repository's own
objects. Wren frames are a 25-byte header plus the object in git's loose
encoding (zlib level 1, as Wren stores it). Git's numbers come from
`git pack-objects` itself.

    python3 -I bench/wire_bytes.py <git-repo> <new-commit> <old-commit> [cone]

Reports a full checkout of <new-commit>, a sparse checkout of <cone>
(default: Documentation), and an incremental update from <old-commit> on a
node that already has it (git: thin pack with deltas; Wren: every new object
whole, since the protocol has no deltas). For the incremental case it writes
two throwaway commit objects into <git-repo> (via `git commit-tree`) so that
git packs exactly the tree change, without intermediate history.
"""

import gzip
import subprocess
import sys
import zlib

repo, C1, C0 = sys.argv[1], sys.argv[2], sys.argv[3]
cone = sys.argv[4] if len(sys.argv) > 4 else "Documentation"


def git(*a, inp=None):
    return subprocess.run(
        ["git", "-C", repo, *a], input=inp, capture_output=True, check=True
    ).stdout


def closure(commit):
    tree = git("rev-parse", commit + "^{tree}").decode().strip()
    objs = {tree: ""}
    for line in git("ls-tree", "-r", "-t", commit).decode().splitlines():
        meta, path = line.split("\t", 1)
        _mode, typ, oid = meta.split()
        if typ in ("tree", "blob"):
            objs[oid] = path
    return tree, objs


def contents(oids):
    out = git("cat-file", "--batch", inp=("\n".join(oids) + "\n").encode())
    res, i = {}, 0
    while i < len(out):
        nl = out.index(b"\n", i)
        oid, typ, size = out[i:nl].decode().split()
        size = int(size)
        res[oid] = (typ, out[nl + 1 : nl + 1 + size])
        i = nl + 2 + size
    return res


def wren_bytes(objs, data):
    total, stream = 0, bytearray()
    for o in objs:
        typ, c = data[o]
        z = zlib.compress(f"{typ} {len(c)}\0".encode() + c, 1)
        total += 25 + len(z)
        stream += bytes(25) + z
    return total, len(gzip.compress(bytes(stream), 6))


def pack(lines, revs=False, thin=False):
    a = ["pack-objects", "--stdout", "-q"]
    a += ["--revs"] if revs else []
    a += ["--thin"] if thin else []
    return len(git(*a, inp=("\n".join(lines) + "\n").encode()))


C1 = git("rev-parse", C1).decode().strip()
C0 = git("rev-parse", C0).decode().strip()
T1, o1 = closure(C1)
T0, o0 = closure(C0)
data = contents(sorted(set(o1) | set(o0) | {C1}))
MB = 1e6

full = [C1] + list(o1)
wb, wgz = wren_bytes(full, data)
print(f"full checkout ({len(full)} objects)")
print(f"  wren {wb / MB:.2f} MB (gzip {wgz / MB:.2f})  git pack {pack([T1], revs=True) / MB:.2f} MB")

sparse = [C1, T1]
for o, p in o1.items():
    if p and (p == cone or p.startswith(cone + "/") or "/" not in p):
        sparse.append(o)  # the cone, plus root files and boundary trees
sparse = list(dict.fromkeys(sparse))
wb, wgz = wren_bytes(sparse, data)
print(f"sparse checkout of {cone} ({len(sparse)} objects)")
print(f"  wren {wb / MB:.2f} MB (gzip {wgz / MB:.2f})  git pack {pack(sparse) / MB:.2f} MB")

new = [C1] + [o for o in o1 if o not in o0]
wb, wgz = wren_bytes(new, data)
# Throwaway commits whose only difference is the tree, so the thin pack holds
# exactly the new tree state, deltified against the old one.
X0 = git("commit-tree", "-m", "x0", T0).decode().strip()
X1 = git("commit-tree", "-m", "x1", "-p", X0, T1).decode().strip()
thin = pack([X1, "^" + X0], revs=True, thin=True)
print(f"incremental {C0[:10]}..{C1[:10]} ({len(new)} new objects)")
print(f"  wren {wb / MB:.3f} MB (gzip {wgz / MB:.3f})  git thin pack {thin / MB:.3f} MB")
