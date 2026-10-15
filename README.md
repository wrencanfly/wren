<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/wren-lockup-dark.svg">
    <img alt="Wren" src="assets/logo/wren-lockup.svg" width="300">
  </picture>
</p>

<p align="center">
  <b>An independent, open-source implementation of the Scion architecture for git.</b><br>
  Commit, land, blame, grep and bisect on the server, without a clone. Built for agents and CI.
</p>

<p align="center">
  <a href="https://github.com/wrencanfly/wren/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/wrencanfly/wren/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-2F4BFF">
  <img alt="Written in Rust" src="https://img.shields.io/badge/rust-stable-B7410E">
  <img alt="Status: experimental" src="https://img.shields.io/badge/status-experimental-F2A516">
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#for-agents-mcp">MCP</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="docs/principles.md">Scion principles</a> ·
  <a href="docs/benchmarks.md">Benchmarks</a> ·
  <a href="docs/spec/wren-protocol-v1.md">API reference</a>
</p>

---

Wren implements, in the open, the source control architecture Anthropic
describes for **Scion**, its in-house system, in
[Redesigning source control at Anthropic](https://www.anthropic.com/engineering/redesigning-source-control).
Like Scion, Wren keeps clients thin and puts the work on the server:

- **History on the server.** Log, blame, diff, merge-base, "which merge
  brought this into main?" and bisect are answered from server-side indexes.
  Nothing is cloned.
- **Writes on the server.** Commits are created from "parent + changed
  files", then rebased, landed and reverted on the server, with no checkout
  and no merge queue.
- **Changesets, with derived trees.** A changeset records what changed
  relative to its first parent; trees and commit objects are derived from it.
- **Want-only checkouts through cache tiers that all speak the same RPC.**
  A job fetches only the objects it reads.

Unlike Scion, Wren stays git all the way through. Commit ids are git ids,
objects are git objects, and checkouts are normal git working trees. By
default Wren runs in front of your existing git host, which stays the source
of truth: Wren lands commits on it with `--force-with-lease`. With
`--source-of-truth wren`, Wren holds the refs and keeps the host up to date
as a downstream mirror, and stock git clones, fetches and pushes through
Wren itself. The [migration plan](docs/design-scion-migration.md) records
each milestone toward Scion's design.

> Wren is an independent project. It is not affiliated with or endorsed by
> Anthropic and contains no Anthropic code. "Scion" refers to the system
> described in Anthropic's article.

## Why

Anthropic's [Redesigning source control at Anthropic](https://www.anthropic.com/engineering/redesigning-source-control)
describes how agents running all day broke the assumption that every git
client is offline and holds a full copy of history. The article does not
call for replacing git. It suggests that "Git platforms can provide
comprehensive server-side RPCs like Scion, perhaps even in a standardized way
across forges that the git client can learn to talk to."

Wren is a working, open implementation of those server-side RPCs that you can
run today, and a concrete starting point for standardizing them.
[docs/principles.md](docs/principles.md) checks Wren against each Scion
principle and says where it still differs.

### Progress toward Scion's design

| Scion | Wren | |
|---|---|---|
| Metadata queries answered by the server (history, blame, ancestry) | Log, blame, diff, merge-base, `landed`, bisect, grep | ✅ |
| Changesets relative to the first parent; trees are derived data | Changesets keep no tree; trees and commits are derived and checked against git's ids | ✅ M1 |
| Every commit, rebase and revert happens on the server; refs move by compare-and-swap | Server-side commit, rebase, land and revert; the git host is updated first (write proxy) | ✅ M2 |
| Want-only object protocol; identical RPCs at every cache tier | `wren cache` tiers, per-object verification, resumable | ✅ |
| No packfiles and no maintenance; contents in object storage; distributed metadata | Immutable segments (git's pack format, never repacked): git/git in 331 MiB. Delta frames on the wire. Segments in S3, GCS or Azure, read by range; Postgres metadata shared by stateless servers | ✅ M3 |
| A daemon per node with a bounded cache, seeded from snapshots; strict tiering | `wren daemon`: generational eviction under a byte budget, leases for git checkouts, snapshot seeding; node → region → server enforced at startup; turmoil simulation of the tiers with fault injection | ✅ M4 |
| The server is the source of truth; older git clients still work | Stock git clones, fetches (partial clones too) and pushes through Wren's git protocol front end; with `--source-of-truth wren` the git host becomes a downstream mirror. Protected refs, a policy hook and an audit log replace the host's protections | ✅ M5 |
| Path history, copy detection and search indexes; directory permissions | The path index is a derived type; renames and copies are detected as git does (`log --follow`, blame across renames, `diff -M`); path search by trigrams; read tokens restricted to directories; per-client rate limits | ✅ M6 |
| Virtualized checkouts | `wren mount` (Linux, FUSE): the whole tree visible at once, fetched when read, writes in an overlay; with `--git`, fsmonitor and cache-tree keep `git status` and `git commit` from reading untouched files | ✅ M7 |

|  |  |
|---|---|
| **Commit without a checkout** | Send "parent + changed files"; Wren derives the tree, rebases onto the branch tip with git's own three-way merge, and lands with a compare-and-swap. No merge queue: a lost race costs one more rebase. |
| **Ask history without a clone** | Log by path, blame, diff, merge-base, ancestry, `landed` and bisect are answered from server-side indexes: changesets, a commit graph with skip lists, a path index, and persisted blame. |
| **Search and read at any revision** | `grep`, `cat` and `ls` at any commit, with no checkout. |
| **Bounded, cacheable answers for agents** | MCP tools pin revisions to full commit ids, so answers are immutable and served by caches. Line ranges and output caps keep token use predictable. |
| **Check out only what you read** | A want-only object protocol through same-RPC cache tiers. `--git` checkouts are stock git working trees with a sparse index, so `git status`, `commit` and `push` work. |
| **Verified against git** | A seeded differential fuzz suite checks every answer against git itself. Server-made commits have the ids `git commit` would give them, rebases agree with `git merge-tree`, and all 85,545 commits of git/git round-trip through changesets. |

> [!IMPORTANT]
> **v0.1, experimental. Read this before you deploy.**
> - **Writes are off by default.** They need `--write-token` on the server.
>   In write-proxy mode the git host's branch protections still apply, because
>   Wren pushes to it like any client. With `--source-of-truth wren` they no
>   longer do: use `--protect` (no deletion, fast-forward only),
>   `--policy-hook` (a webhook that allows or refuses each ref move) and the
>   audit log (`/api/v1/repos/{repo}/audit`), and let only Wren push to the host.
> - **One trust domain.** A token can read every repository on a server, and
>   the object store is shared across repositories. Run one Wren per trust
>   domain.
> - **Force-pushed content stays readable.** Objects removed from a branch
>   remain in the store.
> - **Several servers need Postgres, and a bucket or a shared data
>   directory.** With the default SQLite metadata, one server serves each
>   repository. With `--meta postgres://…`, any number of servers serve the
>   same repositories and any of them can answer any request, given either
>   the same `--data` directory on a shared filesystem or
>   `--segments s3://…` (also `gs://`, `az://`, `file://`), in which case each
>   keeps its own `--data` directory as a local cache.
> - **No TLS of its own.** Put Wren behind a TLS-terminating proxy.
> - **A full checkout sends about 25% more bytes than a git pack.** Storage
>   and incremental updates are on par with git. See
>   [Costs](docs/benchmarks.md#costs).

## Quick start

```sh
cargo build --release
export PATH=$PWD/target/release:$PATH

# 1. Serve a repository mirrored from any git URL (or local path).
export WREN_TOKEN=$(openssl rand -hex 16)
export WREN_WRITE_TOKEN=$(openssl rand -hex 16)   # optional: enables writes
wren server --data ./wren-data --repo git=https://github.com/git/git.git &

# 2. Optional: a region cache. Same RPCs, so clients can't tell the difference.
wren cache --upstream http://127.0.0.1:7070 --listen 127.0.0.1:7071 \
           --upstream-token "$WREN_TOKEN" &
export WREN_SERVER=http://127.0.0.1:7071 WREN_REPO=git

# 3. Optional: a node daemon per machine, with a bounded store shared by
#    every checkout on the machine; objects go through its socket.
wren daemon --store ~/.wren/store --upstream http://127.0.0.1:7071 \
            --upstream-token "$WREN_TOKEN" --budget 20GiB --snapshot git &
export WREN_STORE=~/.wren/store WREN_OBJECTS=unix:$HOME/.wren/store/daemon.sock
```

**Ask without a checkout**

```sh
wren log -n 5 -- builtin/blame.c
wren blame Documentation/git-blame.adoc
wren grep -i 'first.parent' builtin
wren landed <commit> --into master        # which merge brought it into master
wren merge-base master next
wren diff master --name-status          # what master changed vs its first parent
wren cat README.md --rev v2.40.0
```

**Writes.** Commits are made on the server, from whole files, with no
checkout. Landing rebases onto the branch tip on the server; with a git
host upstream, the host is updated first with `--force-with-lease`. Writes go
to the server, not a cache, and need its write token.

```sh
export WREN_WRITE_SERVER=http://127.0.0.1:7070   # the server, not a cache
export WREN_AUTHOR="Ada Lovelace <ada@example.com>"
wren commit -m "Fix typo" --land master --set README.md=./README.md
wren commit -m "Drop script" --parent master --delete ci/old.sh   # prints the commit id
wren land --onto master <commit>...       # oldest first; exits 1 on conflicts
wren revert <commit> --onto master
```

**Check out only what you need, as a real git repo**

```sh
wren checkout ws --sparse t/helper --git
cd ws && git status && git log -1     # stock git works
```

**Mount instead of checking out** (Linux, `cargo build --release --features
vfs`, fuse3): every file is visible at once and fetched when read.

```sh
wren mount ws --git          # ready in ~50 ms for git/git; Ctrl-C or `wren umount ws`
cd ws && git status          # looks only at what you wrote
```

**Use stock git against Wren** (protocol v2, git ≥ 2.26; the token is the
password):

```sh
git clone --filter=blob:none http://x:$WREN_TOKEN@127.0.0.1:7070/git/git
git push http://x:$WREN_WRITE_TOKEN@127.0.0.1:7070/git/git HEAD:refs/heads/topic
```

**Find the commit that broke main.** Bisection runs over first-parent
history. Each step is a fresh sparse checkout that reuses the node-wide
object store.

```sh
wren bisect --good v2.40.0 --bad master --sparse t -- make -C t test-lint
```

**Freshness.** Answers reflect the git host as of the last sync. Every
unpinned answer carries a `wren-synced-at` header. A query that names a
commit Wren has not seen, for example one you just pushed, triggers one sync
before Wren answers. You can also point your host's push webhook at
`POST /api/v1/repos/{repo}/sync`.

## For agents (MCP)

```json
{ "mcpServers": { "wren": { "command": "wren", "args": ["mcp"],
  "env": { "WREN_SERVER": "http://127.0.0.1:7071", "WREN_REPO": "git",
           "WREN_TOKEN": "…" } } } }
```

| Tool | Answers |
|---|---|
| `wren_log` | First-parent history, optionally for a path |
| `wren_show`, `wren_diff` | What a commit changed; diffs between revisions |
| `wren_blame` | Who last changed each line, for a line range |
| `wren_read_file`, `wren_list_dir`, `wren_grep` | Files, directories and search at any revision |
| `wren_merge_base`, `wren_is_ancestor`, `wren_landed` | Ancestry questions, such as "did this land on main, and through which merge?" |
| `wren_bisect_step` | The next commit to test, or the first bad one |
| `wren_refs` | Branches and tags, filtered by prefix |
| `wren_commit` | Commit whole-file contents on the server, and optionally land the commit on a branch |
| `wren_land`, `wren_revert` | Land commits on a branch; revert a commit |

Every query tool resolves its revisions to full commit ids first, and every
answer names the commit it describes. Outputs are capped (about 10k tokens by
default) and say when they were cut, so an agent can narrow the request
instead of guessing.

The write tools send commits to `WREN_WRITE_SERVER` (default `WREN_SERVER`)
with `WREN_WRITE_TOKEN` (default `WREN_TOKEN`), authored as `WREN_AUTHOR`.
When a land conflicts, nothing moves. The tool lists the conflicting paths and
tells the agent to read them at the branch tip and commit again.

## How it works

```mermaid
flowchart LR
  host[(git host)] -- fetch --> server["wren server<br/><small>changesets · derived trees<br/>commit graph · path index · blame</small>"]
  server -- "land (--force-with-lease)" --> host
  mcp -. "commit / land / revert<br/>(control plane)" .-> server
  server -- same RPC --> region["wren cache<br/><small>region</small>"]
  region -- same RPC --> node["wren cache<br/><small>node</small>"]
  node --> cli[wren CLI]
  node --> mcp[MCP / agents]
  node --> co["git checkout<br/><small>sparse index</small>"]
```

- **Changesets.** Each commit is ingested as the list of paths it changed
  relative to its first parent, plus its author, committer and message. The
  path index is written in the same transaction. A changeset does not point
  to a tree: trees and commit objects are derived from it, and ingest checks
  that they reproduce git's ids.
- **Server-side writes.** A new changeset is "parent + changed files"; its
  tree is derived by copying only the paths that changed. `land` replays a
  stack onto the branch tip, looking only at the paths it touched, uses
  `git merge-file` for files changed on both sides, and moves the branch with
  a compare-and-swap. With a git host upstream, the host is updated first
  (`--force-with-lease` is checked inside receive-pack) and Wren's ref
  follows. A lost race costs one more rebase. Caches refuse
  writes, so a cache's credentials can never be used to write.
- **Derived data.** Trees are derived from changesets. The commit graph (generation numbers and first-parent
  skip lists) is derived recursively from parents. Blame is derived lazily:
  the file's change chain is read in one pass over the path index, each
  change costs one line diff, and the result plus a checkpoint every 32
  changes are persisted.
- **Immutable answers.** Objects are content-addressed. A query whose
  revisions are all full commit ids can never change its answer, so it is
  marked `immutable`. Caches keep it, keyed by a query-semantics version
  (`wren-semantics`), so an algorithm change never mixes old and new
  answers.
- **Want-only objects.** `POST /v1/objects {"oids": [...]}`. The client walks
  down from the root tree and asks only for objects it is missing. It
  verifies every hash, and retries resume. For each object it may name a
  base it already holds (the same path in the tree it last checked out);
  a tier then answers with a zstd delta against that base. Every tier still
  stores whole objects.
- **Git compatibility.** Objects use git's loose format. For `--git`
  checkouts Wren writes git's sparse index directly, configures cone-mode
  sparse checkout, and sets the git host as a promisor remote.
- **Segments, not loose files.** A fetched pack moves into the object store
  whole, as an immutable segment with git's deltas, and is never repacked.
  The git mirror keeps refs only and reads the segments through alternates.
  git/git takes 331 MiB, about the size of its git pack.

## Benchmarks

On [git/git](https://github.com/git/git) (85k commits, local machine),
against git's *best* equivalent setup: a partial clone that really applies
its filter, a commit-graph with Bloom filters, and `worktree add` on a warm
node.

| Scenario | git | Wren | |
|---|---:|---:|---:|
| History to download before the first query | 5.5 s, 117 MiB | none | |
| Sparse checkout as a git repo, cold node | 709 ms | 269 ms | 2.6× |
| Same, second job on the node | 248 ms | 100 ms | 2.5× |
| "Which merge brought commit X into HEAD?" | 90 ms | 7 ms | 13× |
| `log --first-parent -- <path>` | 50 ms | 15 ms | 3.3× |
| `blame`, warm | 86 ms | 14 ms | 6× |
| `blame`, first time for a file | 86 ms | 62 ms | 1.4× |

**Costs.** Wren's object store for git/git is 331 MiB, about the size of a git
pack (it was 4.7 GB of loose objects before M3a). With delta frames (M3b), an
incremental update from `HEAD~15` sends 13 KB, against 14 KB for a git thin
pack (640 KB without deltas). A full checkout still sends about 25% more
than a git pack. Full results, method, corrections and
caveats are in [docs/benchmarks.md](docs/benchmarks.md).

## Roadmap

The [migration plan](docs/design-scion-migration.md) has the details and the
gates for each milestone.

- **M3, storage.** Done: immutable segments (M3a), delta frames on the wire
  (M3b), Postgres metadata shared by stateless servers (M3c), segments in
  object storage (M3d).
- **M4, node daemon.** Done: a bounded per-node cache seeded from snapshots,
  strict tiering, and simulation tests of the tiers with fault injection.
- **M5, source of truth.** Done: a git protocol front end so stock git can clone,
  fetch and push through Wren, with the git host as a downstream mirror.
- **M6, indexes and permissions.** Done: path history through the
  derived-data mechanism, rename and copy detection, path search, directory
  ACLs for queries, per-client rate limits. Content search indexes remain.
- **Upstream.** Send the [sparse/promisor report](docs/upstream/git-sparse-promisor.md)
  to the git list, and refine the [draft git protocol v2 extensions](docs/upstream/git-protocol-v2-query.md).

- **M7, virtual checkouts.** Done on Linux: `wren mount`. macOS (FSKit)
  remains.

Not planned: merge queues and code review.

## Documentation

| | |
|---|---|
| [Scion principles](docs/principles.md) | How Wren maps to each Scion principle, and where it still differs |
| [Migration plan](docs/design-scion-migration.md) | Milestones M1–M7 toward Scion's design, decisions and measured results (Chinese) |
| [API reference](docs/spec/wren-protocol-v1.md) | Endpoints, wire format and caching rules (draft, unstable) |
| [Benchmarks](docs/benchmarks.md) | Method, results, costs and corrections |
| [Design](docs/design.md) | Data model, invariants, limitations (Chinese) |
| [Reviews](docs/reviews) and [research](docs/research) | Design reviews and research notes written with AI agents during development (Chinese) |

## Development

```sh
cargo test --release                         # unit + differential fuzz (vs git) + end-to-end
WREN_FUZZ_SEEDS=1,2,3,0xbeef cargo test --release -p wren-vcs-core --test differential
```

Before changing core logic, read the invariants in
[docs/design.md §8](docs/design.md#8-不变量给人和-agent-的不清单) and
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache-2.0](LICENSE)
