#!/usr/bin/env python3
"""Wren vs git benchmark, against git's best equivalent setup.

Both sides are served from the same machine (git via file://, Wren via
localhost HTTP), so the numbers compare protocols and architecture, not
network bandwidth. The git baseline is configured the way a well-run forge
or CI system would configure it:

* the serving repo allows partial-clone filters (checked: a silently ignored
  filter aborts the run),
* the repo used for history queries has a commit-graph with changed-path
  Bloom filters,
* a second job on the same node reuses the node's existing partial clone via
  `git worktree add` instead of cloning again.

usage: bench/run.py --upstream BARE_REPO --wren-server URL --repo NAME
                    --sparse DIR --blame FILE --workdir DIR [--wren BIN]
"""
import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import time


def sh(cmd, cwd=None, env=None):
    r = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"command failed: {cmd}\n{r.stderr}")
    if "filtering not recognized" in r.stderr:
        raise SystemExit(f"git ignored the partial-clone filter: {cmd}")
    return r.stdout, r.stderr


def out(cmd, **kw):
    return sh(cmd, **kw)[0]


def median_time(fn, runs, setup=None):
    times, result = [], None
    for _ in range(runs):
        if setup:
            setup()
        t = time.perf_counter()
        result = fn()
        times.append(time.perf_counter() - t)
    return statistics.median(times), result


def du(path):
    total = 0
    for root, _, files in os.walk(path):
        for f in files:
            p = os.path.join(root, f)
            if not os.path.islink(p):
                total += os.path.getsize(p)
    return total


def rm(*paths):
    def go():
        for p in paths:
            shutil.rmtree(p, ignore_errors=True)
    return go


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--upstream", required=True)
    ap.add_argument("--wren-server", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--sparse", required=True)
    ap.add_argument("--blame", required=True)
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--wren", default="target/release/wren")
    ap.add_argument("--runs", type=int, default=3)
    a = ap.parse_args()

    wren = os.path.abspath(a.wren)
    upstream = os.path.abspath(a.upstream)
    up = "file://" + upstream
    w = os.path.abspath(a.workdir)
    os.makedirs(w, exist_ok=True)
    env = dict(os.environ, WREN_SERVER=a.wren_server, WREN_REPO=a.repo, WREN_LOG="warn")
    G = ["git", "--git-dir", upstream]

    # ---- fair git setup ---------------------------------------------------------
    out(G + ["config", "uploadpack.allowFilter", "true"])
    out(G + ["config", "uploadpack.allowAnySHA1InWant", "true"])
    out(G + ["commit-graph", "write", "--reachable", "--changed-paths"])
    head = out(G + ["rev-parse", "HEAD"]).strip()
    rows = []

    def row(name, git_t, wren_t, git_note="", wren_note=""):
        rows.append((name, git_t, wren_t, git_note, wren_note))
        print(f"{name:<50} git {git_t*1000:8.0f} ms   wren {wren_t*1000:8.0f} ms   {git_note} | {wren_note}", flush=True)

    def wren_checkout(dest, store, *extra):
        _, err = sh([wren, "checkout", dest, "--store", store, *extra], env=env)
        m = re.search(r"fetched (\d+) objects \(([\d.]+) MiB\)", err)
        return f"{m.group(1)} objects, {m.group(2)} MiB fetched" if m else ""

    # ---- 1. full checkout, cold node ----------------------------------------------
    g, s, f = f"{w}/git-full", f"{w}/store-full", f"{w}/wren-full"
    gt, _ = median_time(lambda: out(["git", "clone", "-q", "--depth=1", up, g]), a.runs, rm(g))
    ft, note = median_time(lambda: wren_checkout(f, s), a.runs, rm(f, s))
    row("full checkout at HEAD, cold node", gt, ft, "clone --depth=1", note)

    # ---- 2. sparse checkout as a git repo, cold node --------------------------------
    g, s, f = f"{w}/git-sparse", f"{w}/store-sparse", f"{w}/wren-sparse"

    def git_sparse():
        out(["git", "clone", "-q", "--depth=1", "--filter=blob:none", "--sparse", up, g])
        out(["git", "-C", g, "sparse-checkout", "set", "--cone", a.sparse])

    gt, _ = median_time(git_sparse, a.runs, rm(g))
    git_objs = out(["git", "-C", g, "count-objects", "-v"])
    git_n = sum(int(l.split()[1]) for l in git_objs.splitlines() if l.startswith(("count:", "in-pack:")))
    ft, note = median_time(lambda: wren_checkout(f, s, "--git", "--sparse", a.sparse), a.runs, rm(f, s))
    assert out(["git", "-C", f, "status", "--porcelain"]) == "", "wren checkout not clean"
    row(f"sparse git checkout of {a.sparse}, cold node", gt, ft,
        f"blobless+sparse clone, {git_n} objects", f"sparse index, {note}")

    # ---- 3. same, second job on the same node ---------------------------------------
    f2, g2 = f"{w}/wren-sparse2", f"{w}/git-sparse2"
    ft, _ = median_time(lambda: wren_checkout(f2, s, "--git", "--sparse", a.sparse), a.runs, rm(f2))

    def git_worktree():
        shutil.rmtree(g2, ignore_errors=True)
        out(["git", "-C", g, "worktree", "prune"])
        out(["git", "-C", g, "worktree", "add", "-q", "--detach", g2, "HEAD"])

    gt, _ = median_time(git_worktree, a.runs)
    assert out(["git", "-C", g2, "status", "--porcelain"]) == ""
    row(f"sparse git checkout of {a.sparse}, warm node", gt, ft,
        "worktree add from node's clone", "shared node store")

    # ---- 4. queries --------------------------------------------------------------
    # Setup cost git pays before it can answer history queries at all.
    b = f"{w}/git-blobless"
    gt, _ = median_time(lambda: out(["git", "clone", "-q", "--bare", "--filter=blob:none", up, b]), 1, rm(b))
    row("prerequisite: local history to query", gt, 0.0,
        f"blobless bare clone, {du(b)/2**20:.0f} MiB", "none")

    gt, gout = median_time(lambda: out(G + ["blame", "--first-parent", "--porcelain", head, "--", a.blame]), a.runs)
    # First call derives and persists blame; later calls reuse it.
    cold_t, _ = median_time(lambda: out([wren, "blame", a.blame, "--rev", head, "--json"], env=env), 1)
    ft, fout = median_time(lambda: out([wren, "blame", a.blame, "--rev", head, "--json"], env=env), a.runs)
    row(f"blame {a.blame}", gt, ft, "full repo + commit-graph", f"warm; cold first call {cold_t*1000:.0f} ms")
    hexline = re.compile(r"^[0-9a-f]{40} \d+ \d+")
    git_lines, files, cur = [], {}, None
    for l in gout.splitlines():
        if hexline.match(l):
            cur = l[:40]
            git_lines.append(cur)
        elif l.startswith("filename "):
            files.setdefault(cur, l[9:])
    wren_lines = [x["commit"] for x in json.loads(fout)["lines"]]
    assert len(git_lines) == len(wren_lines)
    same = sum(x == y for x, y in zip(git_lines, wren_lines))
    nonrename = [(x, y) for x, y in zip(git_lines, wren_lines) if files.get(x) == a.blame]
    print(f"    blame agreement: {same}/{len(git_lines)} overall; "
          f"{sum(x == y for x, y in nonrename)}/{len(nonrename)} on lines git did not trace through a rename")

    gt, gout = median_time(lambda: out(G + ["log", "--first-parent", "--format=%H", head, "--", a.blame]), a.runs)
    ft, fout = median_time(lambda: out([wren, "log", head, "-n", "100000", "--json", "--", a.blame], env=env), a.runs)
    row(f"log --first-parent -- {a.blame}", gt, ft, "commit-graph + Bloom filters")
    assert gout.split() == [c["oid"] for c in json.loads(fout)["commits"]], "path log mismatch"

    old = out(G + ["rev-list", "--first-parent", "--skip=5000", "-n1", head]).strip()
    gt, gout = median_time(lambda: out(G + ["merge-base", "--all", old, head]), a.runs)
    ft, fout = median_time(lambda: out([wren, "merge-base", old, head], env=env), a.runs)
    row("merge-base --all HEAD HEAD~5000", gt, ft, "commit-graph")
    assert sorted(gout.split()) == sorted(fout.split())

    side = out(G + ["rev-list", "--no-merges", "-n1", "--skip=20000", head]).strip()

    def git_landed():
        # Commits on the first-parent chain that descend from `side`, walked
        # in topological order (merges often share a timestamp).
        anc = set(out(G + ["rev-list", "--ancestry-path", f"{side}..{head}"]).split())
        for c in reversed(out(G + ["rev-list", "--first-parent", head]).split()):
            if c in anc:
                return c

    gt, expect = median_time(git_landed, a.runs)
    ft, fout = median_time(lambda: out([wren, "landed", side, "--into", head], env=env), a.runs)
    row("landed: which merge brought commit X into HEAD", gt, ft, "ancestry-path ∩ first-parent", "skip-list binary lifting")
    assert fout.strip() == expect, (fout, expect)

    print("\n| scenario | git | wren | git setup | wren notes |\n|---|---:|---:|---|---|")
    for name, g_, f_, gn, fn in rows:
        print(f"| {name} | {g_*1000:.0f} ms | {f_*1000:.0f} ms | {gn or '-'} | {fn or '-'} |")


if __name__ == "__main__":
    main()
