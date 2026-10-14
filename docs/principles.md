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
Wren has Scion's read path and, since M2, its write path (with the git host
still the source of truth). The [migration plan](design-scion-migration.md)
covers the rest. Wren is not affiliated with Anthropic, contains no
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
| **Comprehensive server API.** "jobs don't need a local repository at all" | The API covers log (also by path), show, diff, blame, merge-base, is-ancestor, landed, bisect, grep, file and tree, at any revision. It has no `log -S`/`log -L` yet and does not follow renames. | Partial |
| **Server does the writes.** "every commit, rebase, or merge happens on the server" | Commits are created on the server from parents plus changed files, with no checkout and no tree from the client; stacks are rebased onto a moving branch (git's own three-way merge for files changed on both sides), landed and reverted on the server. While a git host is the source of truth (write-proxy mode), Wren updates it first with `--force-with-lease` and its own ref follows. Merge commits are not created on the server yet; only linear stacks land. | Partial |
| **Changesets.** "files that changed relative to its first parent", in a write-ahead log | Every commit is ingested as a changeset against its first parent: its author, committer, headers and message verbatim, and the files it changed. It does not point to a tree. The path index is written in the same transaction. | Follows |
| **Refs are compare-and-swap.** | Every ref move is a compare-and-swap: mirrored refs, and lands, which are a CAS on the git host (`--force-with-lease`, checked inside receive-pack) followed by a CAS on Wren's ref. A lost race costs one more rebase. Within one server, lands to a repository take turns (a process-local lock that only prevents retry storms); correctness rests on the CAS, which also catches writers outside Wren. | Follows |
| **Derived data is defined recursively.** For each type, "we can choose whether to compute it eagerly or defer the work" | Trees are derived from changesets (`tree(cs) = apply(tree(p1), changes)`, path copying, git's own serialization), and so is the commit object; ingest checks that both reproduce git's ids. The commit graph (generations and first-parent skip lists) is derived eagerly from parents. Blame is derived lazily and persisted, and is a pure function of history. The path index is maintained at ingest, not through the generic mechanism. | Partial |
| **No fully materialized copy.** "there is no fully materialized copy of the repo anywhere, even on the server" | The server's git mirror keeps refs only, but the authoritative server's object store holds every object (as loose objects), and the metadata is single-host SQLite. Only object reads and pinned queries scale out, through caches. | Not adopted (v0.1) |
| **Content-addressed caches; identical RPCs at every tier; strict tiering.** | `wren cache` speaks exactly the server's RPCs and points at one upstream. Objects and pinned queries (all revisions are full commit ids) are immutable. Cached answers are keyed by a query-semantics version, so an algorithm change never mixes old and new answers. | Follows |
| **Want-only negotiation.** It "operates exclusively on trees and blobs the client wants", and is resumable | Clients ask only for missing objects, every object is verified, and retries resume. The cost is that there are no deltas: an incremental update sends whole objects (see [benchmarks](benchmarks.md#costs)). Git protocol v2 can also express want-only fetches (no `have`, noop negotiation, `filter`). Wren's difference is per-object addressability and HTTP caching. | Partial |
| **Sparse checkouts.** "the client simply doesn't ask for trees it doesn't want" | Wren-native checkouts fetch only the cone. A `--git` checkout writes git's sparse index, which also needs the trees on the cone's boundary. Stock git reads those trees, but not deeper ones. | Partial |
| **Virtualized checkouts.** Files "materialize on demand" | Wren does not plan this. It needs a filesystem driver, which is a separate product. | Not adopted |
| **Post-merge testing instead of a global merge queue.** | `land` joins no queue and takes no global lock: it rebases onto the current tip and retries the CAS if it lost. `landed`, first-parent `bisect` (steps are fresh sparse checkouts) and server-side `revert` are the primitives for finding and backing out a bad change after it merged. Wren is not a testing pipeline. | Partial |
| **"the various sparse and shallow features in git can be made to compose well"** | Wren hit two rough edges. First, `sparse-checkout reapply` lazily fetches out-of-cone blobs it never writes. Second, a sparse index needs the boundary tree objects to be present. A report for the git list is drafted in [upstream/](upstream/git-sparse-promisor.md) and has not been sent yet. | Partial |
| **RPCs "standardized … across forges that the git client can learn to talk to"** | There is an [API reference](spec/wren-protocol-v1.md) (an unstable draft) and a conformance method that uses git itself as the oracle. A [draft proposal](upstream/git-protocol-v2-query.md) maps the queries to git protocol v2 extensions; it has not been sent. Stock git cannot speak Wren's query API today. | Partial |
| **Hermetic simulation and fuzzing** | A seeded differential fuzz suite generates random histories (branches, merges, large edits, force-pushes) and checks every answer against git. An end-to-end test runs the real server → cache → client topology. There is no deterministic simulation of the tiers with fault injection yet. | Partial |
| **Migration starts with metadata queries.** "Early in the migration, before we had our own client, repo format, or protocols, Scion only answered these" metadata queries | This is exactly Wren's stage: it answers metadata queries in front of git, with git's own format and client. | Follows |
| **No packfiles, no repository maintenance** | Objects are stored one per file, so there is no repack or gc. The cost is disk space (see above). | Follows |
| **A bounded client cache; one daemon per node shares objects across jobs.** "The client maintains a bounded local object cache" | Checkouts on a node share one object store through git alternates, and `wren cache` can run per node. Neither store is bounded yet: nothing is ever evicted. | Partial |
| **Directory-level permissions and per-client limits** | Wren has one trust domain per server and only an upstream concurrency limit. A directory ACL would not be a security boundary while the git host can be cloned directly. | Not adopted |
| **Copy and rename detection in derived indexes** | Wren does not follow renames or copies; blame and log match `git --no-renames`. | Not adopted |
| **Agents "violate core system constraints to satisfy a pattern"**, e.g. "linearly reading the commit log" | [Invariants](design.md#8-不变量给人和-agent-的不清单) are written down for people and agents. `landed` and `bisect` use skip lists instead of scanning the log. | Follows |

## Where Wren is weaker than git today

The article praises git, and Wren uses git as the baseline it must beat
fairly.

- **Storage.** Objects are stored one file each, in git's loose zlib
  encoding. For git/git that is 4.7 GB on disk, against about 313 MiB as a
  git pack.
- **Incremental fetches.** Wren has no deltas, so updating a warm node can
  send many times more bytes than a git thin pack.
- **Trust model.** Wren serves one trust domain. A token can read every
  repository on a server, and the object store is shared across
  repositories. Content removed by a force-push stays readable until the
  data directory is rebuilt.
- **Freshness.** Answers reflect the host as of the last sync. Wren reports
  that time on every unpinned answer, and a query naming an unknown commit
  triggers one sync. A branch that moved without Wren seeing it is still
  stale until the next sync.

The [benchmarks](benchmarks.md) give the numbers. The
[design](design.md#10-已知局限) lists the remaining limitations.
