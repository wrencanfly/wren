# Wren and the Scion principles

Anthropic's [Redesigning source control at Anthropic](https://www.anthropic.com/engineering/redesigning-source-control)
describes Scion, their in-house source control system. The article does not
propose replacing git. It says Scion "is by no means a suitable replacement
for git for general use", and it hopes others will "apply some of the most
successful parts of Scion to git without departing from the overall model".
It also suggests that "Git platforms can provide comprehensive server-side
RPCs like Scion, perhaps even in a standardized way across forges that the
git client can learn to talk to."

Wren is an independent, open-source implementation of the Scion
architecture for git. It follows the migration order the article describes:
first the server answers metadata queries in front of an unmodified git host,
then it takes over writes, then storage, caching and the source of truth.
Wren has Scion's read path, its write path (M2), its storage and tiers (M3,
M4), and can be the source of truth with the git host as a downstream mirror
(M5). The [migration plan](design-scion-migration.md) records each
milestone and its measurements. Wren is not affiliated with Anthropic, contains no
Anthropic code, and Anthropic has not reviewed or endorsed it.

This page checks Wren against each principle in the article. **Follows**
means Wren implements it. **Partial** means it is implemented with a stated
limitation. **Not adopted** means Wren deliberately does not do it, and the
reason is given.

## Who this is for

The article says Scion's design fits "truly enormous monorepos, with either
tens of thousands of engineers or swarms of agents". It also expects that
"this class of problems will become more commonplace over the next six to 12
months".

For most repositories, plain git is the right tool. Wren is for teams that
are starting to hit these walls:

- CI jobs and agent sandboxes that clone thousands of times a day;
- agents that need history, blame or ancestry, and would otherwise need a
  full clone to get it;
- question-shaped workloads, such as "did X land on main?", that are
  expensive to answer from a log.

## Principle by principle

| Scion principle (article) | Wren | Status |
|---|---|---|
| **Clients are always online.** "we designed Scion's client and protocols around a server that is always reachable" | The server answers history questions over HTTP, a CLI and MCP. Clients keep no history. | Follows |
| **Comprehensive server API.** "jobs don't need a local repository at all" | The API covers log (also by path, and following renames), show, diff (with renames and copies), blame (across renames), merge-base, is-ancestor, landed, bisect, grep, path search, file and tree, at any revision. It has no `log -S`/`log -L` and no indexed content search yet. | Partial |
| **Server does the writes.** "every commit, rebase, or merge happens on the server" | Commits are created on the server from parents plus changed files, with no checkout and no tree from the client; stacks are rebased onto a moving branch (git's own three-way merge for files changed on both sides), landed and reverted on the server. In write-proxy mode the git host is updated first with `--force-with-lease` and Wren's ref follows; with `--source-of-truth wren` (M5) Wren moves its ref first and the host follows as a downstream mirror. Merge commits are not created on the server yet; only linear stacks land. | Partial |
| **Changesets.** "files that changed relative to its first parent", in a write-ahead log | Every commit is ingested as a changeset against its first parent: its author, committer, headers and message verbatim, and the files it changed. It does not point to a tree. The path index is written in the same transaction. | Follows |
| **Refs are compare-and-swap.** | Every ref move is a compare-and-swap, whichever way it comes (the write API, `land`, `git push` through Wren's git front end): a CAS on Wren's ref, preceded in write-proxy mode by a CAS on the git host (`--force-with-lease`, checked inside receive-pack). A lost race costs one more rebase. Within one server, lands to a repository take turns (a process-local lock that only prevents retry storms); correctness rests on the CAS, which also catches writers outside Wren. The turmoil simulation checks that ref updates are linearizable. | Follows |
| **Derived data is defined recursively.** For each type, "we can choose whether to compute it eagerly or defer the work" | Every index is a derived type behind one mechanism. Trees and commit objects are derived from changesets and checked against git's ids at ingest. The path index (eager, materialized into its own table in the changeset's transaction) and the commit graph (eager) derive from parents; blame (lazy, persisted at checkpoints) and renames (lazy, per changeset) are derived on first query. | Follows |
| **No fully materialized copy.** "there is no fully materialized copy of the repo anywhere, even on the server" | Since M3d, contents live in object storage (S3, GCS, Azure) as immutable segments, and metadata in Postgres. A server that only answers queries and serves objects reads segments by range and caches blocks; it starts with the indexes alone (11.5 MiB for git/git). But a server that syncs from or pushes to the git host still downloads every segment first, because its git mirror reads objects through alternates. With Wren as the source of truth (M5), nothing is fetched from the host and pushes through Wren's git front end become segments directly; only the server exporting to the host still pushes through a mirror. | Partial |
| **Content-addressed caches; identical RPCs at every tier; strict tiering.** | `wren cache` (region) and `wren daemon` (node) speak exactly the server's RPCs. Tiers are strict: a node's upstream must be a region and a region's the server, checked at startup (an explicit override exists for small deployments). Objects and pinned queries (all revisions are full commit ids) are immutable. Cached answers are keyed by a query-semantics version, so an algorithm change never mixes old and new answers. Concurrent misses share one upstream request. | Follows |
| **Want-only negotiation.** It "operates exclusively on trees and blobs the client wants", and is resumable | Clients ask only for missing objects, every object is verified, and retries resume. A client may name, per object, a base it already holds (the same path in the tree it last checked out); a tier then answers with a zstd delta against it, and every tier still stores whole objects. Updating a node from `HEAD~15` sends 13 KB (a git thin pack: 14 KB). Git protocol v2 can also express want-only fetches (no `have`, noop negotiation, `filter`). Wren's difference is per-object addressability and HTTP caching. | Follows |
| **Sparse checkouts.** "the client simply doesn't ask for trees it doesn't want" | Wren-native checkouts fetch only the cone. A `--git` checkout writes git's sparse index, which also needs the trees on the cone's boundary. Stock git reads those trees, but not deeper ones. | Partial |
| **Virtualized checkouts.** Files "materialize on demand" | `wren mount` (Linux, FUSE) shows a commit's whole tree and fetches a directory's tree when it is listed and a file's blob when it is read; writes go to an overlay. With `--git`, git's fsmonitor (fed from the mount's journal) and a cache-tree index keep `git status` and `git commit` from touching unread files. No macOS support (FSKit) yet. | Partial |
| **Post-merge testing instead of a global merge queue.** | `land` joins no queue and takes no global lock: it rebases onto the current tip and retries the CAS if it lost. `landed`, first-parent `bisect` (steps are fresh sparse checkouts) and server-side `revert` are the primitives for finding and backing out a bad change after it merged. Wren is not a testing pipeline. | Partial |
| **"the various sparse and shallow features in git can be made to compose well"** | Wren hit two rough edges. First, `sparse-checkout reapply` lazily fetches out-of-cone blobs it never writes. Second, a sparse index needs the boundary tree objects to be present. A report for the git list is drafted in [upstream/](upstream/git-sparse-promisor.md) and has not been sent yet. | Partial |
| **RPCs "standardized … across forges that the git client can learn to talk to"** | There is an [API reference](spec/wren-protocol-v1.md) (an unstable draft) and a conformance method that uses git itself as the oracle. A [draft proposal](upstream/git-protocol-v2-query.md) maps the queries to git protocol v2 extensions; it has not been sent. Stock git talks to Wren for objects and refs (clone, fetch, partial clone, push, through protocol v2 and receive-pack), but not yet for queries. | Partial |
| **Hermetic simulation and fuzzing** | A seeded differential fuzz suite generates random histories (branches, merges, large edits, force-pushes) and checks every answer against git. A turmoil simulation runs the real server, region cache and node daemons on a simulated network with partitions, latency and crashes drawn from a seed, and checks object verification, agreement of cached answers with the server, linearizable ref updates and upstream limits (10,000 seeds). Storage runs on real threads, so thread interleavings are not bit-for-bit reproducible. | Follows |
| **Migration starts with metadata queries.** "Early in the migration, before we had our own client, repo format, or protocols, Scion only answered these" metadata queries | Wren followed the same order: metadata queries in front of git first, then server-side writes (M2), its own storage and tiers (M3, M4), then the source of truth (M5), always with git's ids and objects. | Follows |
| **No packfiles, no repository maintenance** | The server stores objects in immutable segments in git's pack format: a fetched pack moves in whole, with git's deltas, and no segment is ever repacked or garbage-collected. git/git takes 331 MiB, about its pack size. Wren's own clients get objects, not packs. Stock git gets a pack generated per request (it can take nothing else), reusing the deltas stored in the segments; it is never stored. | Follows |
| **A bounded client cache; one daemon per node shares objects across jobs.** "The client maintains a bounded local object cache" | `wren daemon` owns the node's store and serves every checkout on the machine over a unix socket. Over its byte budget it drops the oldest generations whole, after copying forward trees, recently read blobs and blobs of leased `--git` checkouts (git reads those through alternates). A new daemon seeds itself from a snapshot. | Follows |
| **Directory-level permissions and per-client limits** | Read tokens can be restricted to directories: query answers are filtered, and file, blame and log refuse paths outside the grant. Because an object's hash does not say which directory it belongs to, a restricted token's object requests name a commit, and the server serves only that commit and the trees and blobs inside the token's directories at it, never the trees above them; `wren checkout --sparse` with such a token starts at its directories. Restricted tokens cannot use the git front end, `--git` checkouts or caches. Each client (token, or address) has a rate limit. | Partial |
| **Copy and rename detection in derived indexes** | Renames and copies are a derived type per changeset: exact matches, then git's own similarity estimate (50%). `log --follow`, blame across renames and `diff -M` agree with git in the differential tests (one candidate per rename; several equally good candidates may be paired differently). | Follows |
| **Agents "violate core system constraints to satisfy a pattern"**, e.g. "linearly reading the commit log" | [Invariants](design.md#8-不变量给人和-agent-的不清单) are written down for people and agents. `landed` and `bisect` use skip lists instead of scanning the log. | Follows |

## Where Wren is weaker than git today

The article praises git, and Wren uses git as the baseline it must beat
fairly.

- **Storage.** Fixed in M3a: segments keep git's deltas, so git/git takes
  331 MiB, about its git pack (it was 4.7 GB of loose objects).
- **Incremental fetches.** Fixed in M3b with delta frames: updating a node
  from `HEAD~15` sends 13 KB, against 14 KB for a git thin pack. A full
  checkout still sends about 25% more than a git pack, because each object
  is compressed on its own.
- **Trust model.** The object plane is one trust domain: the store is shared
  across repositories, and only directory-restricted read tokens (M6) are
  kept from it. Content removed by a force-push stays readable until the
  data directory is rebuilt.
- **Freshness.** In write-proxy mode, answers reflect the host as of the
  last sync. Wren reports that time on every unpinned answer, and a query
  naming an unknown commit triggers one sync. A branch that moved without
  Wren seeing it is still stale until the next sync. With Wren as the source
  of truth (M5) there is nothing to sync; the host lags instead, and its lag
  is reported.

The [benchmarks](benchmarks.md) give the numbers. The
[design](design.md#10-已知局限) lists the remaining limitations.
