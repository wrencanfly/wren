# Benchmarks

Repository: **[git/git](https://github.com/git/git)**, with 85,545 commits on
branches and tags, 421k objects and 1,019 refs. It was mirrored locally on
2026-10-13.

Machine: Apple Silicon Mac, release build. Each figure is the median of 3 runs.
Git is served over `file://` and Wren over localhost HTTP, so both sides avoid
the network. The numbers compare protocols and architecture, not bandwidth.
Reproduce with `bench/run.py`.

**The git baseline is git's best equivalent setup, not a strawman:**

- Partial-clone filters are enabled on the serving repo. If git ever
  silently ignores a filter, the script aborts.
- The repo that answers history queries has a commit-graph with
  changed-path Bloom filters, as forges maintain it.
- On a warm node, git reuses the node's existing clone via
  `git worktree add` instead of cloning again.

> An earlier version of this page did not do this and overstated Wren's lead
> (claims such as 4.8× and 12×). See [Corrections](#corrections).

## Results

| Scenario | git (best setup) | Wren | Ratio |
|---|---:|---:|---:|
| Full checkout at HEAD, cold node (`clone --depth=1`) | 1466 ms | 650 ms | 2.3× |
| Sparse checkout of `t/helper` as a git repo, cold node (blobless + sparse clone) | 709 ms | 269 ms | 2.6× |
| Same, second job on the same node (`git worktree add` from the node's clone) | 248 ms | 100 ms | 2.5× |
| **Prerequisite for history queries**: local history to query (blobless bare clone) | 5544 ms, 117 MiB | none | n/a |
| `blame --first-parent builtin/blame.c`, Wren warm | 86 ms | 14 ms | 6× |
| `blame --first-parent builtin/blame.c`, Wren cold (first blame of this file) | 86 ms | 622 ms | **0.14× (slower)** |
| `log --first-parent -- builtin/blame.c` | 50 ms | 15 ms | 3.3× |
| `merge-base --all HEAD HEAD~5000` | 35 ms | 35 ms | 1× |
| "Which merge brought commit X into HEAD?" | 90 ms | 7 ms | 13× |

The benchmark script asserts that the results agree. Path log, merge-base and
landed are identical to git's.

## How to read this

**Wren's real advantage is what it lets you skip.**

- To answer a history query, git first needs local history. Even a blobless
  bare clone of git/git takes 5.5 s and 117 MiB over a local `file://`
  transport, and much longer over a real network.
- A full clone from GitHub took about 73 s and 619 MB when we mirrored the
  repository.
- Wren answers with no local repository at all. For CI jobs and agent
  sandboxes that are created and thrown away all day, that is the cost that
  matters.

**Checkouts are 2–3× faster, not an order of magnitude.**

- Both sides fetch about the same number of objects: 1,850 for Wren and
  1,966 for git in the sparse case.
- Wren wins on protocol mechanics. There is no have/want negotiation and no
  pack generation, and requests run in parallel batches.
- On a warm node, git's `worktree add` is already good. Wren's shared node
  store is 2.5× faster and needs no pre-existing clone per repository.

**Query speed against a well-maintained local repo is mixed.**

- Wren is clearly faster on `landed` and path log, comparable on
  merge-base, and much faster on warm blame.
- **Cold blame is slower than git.** The first blame of a file derives and
  persists every step of that file's first-parent change history (622 ms for
  `builtin/blame.c`). After that, each further change to the file costs one
  line diff.

## Ingest and footprint

| | |
|---|---|
| Initial sync (all objects + 85,545 changesets + commit graph + path index) | 173 s |
| Objects imported | 421,115 |
| Server-side git mirror after sync | **5.3 MiB** (refs only; fetched packs are absorbed into the object store and deleted) |

## Blame agreement with git

- **Renames.** Git blame follows whole-file renames
  (`builtin-blame.c` → `builtin/blame.c`) and Wren does not.
- **Agreement.** On the 884 lines git did not trace through a rename, Wren
  agrees on **873 (98.8%)**.
- **Remaining differences.** These come from alignment heuristics in git's
  xdiff (e.g. the indent heuristic) that the Rust diff library does not
  reproduce.

## Corrections

Independent reviews ([reviews/03](reviews/03-strategy-and-claims.md)) found
that the first published version of this page compared Wren against
misconfigured git baselines.

1. **Sparse checkout (claimed 4.8×).** The partial-clone filter was passed
   through an environment variable that git ignores for `file://`. Git printed
   `filtering not recognized by server, ignoring` and fetched every blob at
   HEAD. With the filter actually applied, the ratio is 2.6×.
2. **Warm node (claimed 12×).** The git side cloned again instead of using
   `git worktree add` from the node's existing clone. The ratio is 2.5×.
3. **Queries.** The git repo had no commit-graph. With one, git's path log
   drops from 278 ms to 50 ms and merge-base from 220 ms to 35 ms.
4. **Disk note.** The earlier claim that Wren's store is smaller than git's
   `.git` was an artifact of mistake 1.

The script now enforces the fair setup: a silently ignored filter aborts
the run.

## Caveats

- This is a single machine, and git/git is small next to the repositories
  that motivated Scion (100 GB of history). Wren's checkout cost tracks what
  you read, and its query cost tracks the change or index size; git's local
  history cost tracks the repository. That gap should widen with repository
  size, but **these benchmarks do not demonstrate that.**
- Servers and clients ran on the same host, so cache tiers were not measured
  under real network latency.
