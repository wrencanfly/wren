# Wren and the Scion principles

Anthropic's [Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control)
describes Scion, their in-house source control system. The article does not
propose replacing git. It says Scion "is by no means a suitable replacement
for git for general use", and it hopes others will "apply some of the most
successful parts of Scion to git without departing from the overall model".
It also suggests that "Git platforms can provide comprehensive server-side
RPCs like Scion, perhaps even in a standardized way across forges that the
git client can learn to talk to."

Wren is an attempt to do that in the open. It adds Scion's read path in front
of an unmodified git host. Wren is an independent project, and Anthropic has
not reviewed or endorsed it.

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
| **Server does the writes.** "every commit, rebase, or merge happens on the server" | Writes go through `git push` to your existing host, which stays the source of truth. The article's own advice is to keep git's overall model, so Wren does not become a second source of truth. | Not adopted |
| **Changesets.** "files that changed relative to its first parent", in a write-ahead log | Every commit is ingested as a changeset against its first parent. The path index is written in the same transaction. | Follows |
| **Refs are compare-and-swap.** | Refs are mirrored from the host with CAS updates. | Follows |
| **Derived data is defined recursively.** It is computed "eagerly or deferred" and written back | The commit graph (generations and first-parent skip lists) is derived eagerly from parents. Blame is derived lazily and persisted, and is a pure function of history. The path index is maintained at ingest, not through the generic mechanism. | Partial |
| **No fully materialized copy.** "there is no fully materialized copy of the repo anywhere, even on the server" | The server's git mirror keeps refs only, and objects live in one content-addressed store. The metadata is single-host SQLite, so one server is authoritative per repository. | Partial |
| **Content-addressed caches; identical RPCs at every tier; strict tiering.** | `wren cache` speaks exactly the server's RPCs and points at one upstream. Objects and pinned queries (all revisions are full commit ids) are immutable. Cached answers are keyed by a query-semantics version, so an algorithm change never mixes old and new answers. | Follows |
| **Want-only negotiation.** It "operates exclusively on trees and blobs the client wants", and is resumable | Clients ask only for missing objects, every object is verified, and retries resume. The cost is that there are no deltas: an incremental update sends whole objects (see [benchmarks](benchmarks.md#costs)). Git protocol v2 can also express want-only fetches (no `have`, noop negotiation, `filter`). Wren's difference is per-object addressability and HTTP caching. | Partial |
| **Sparse checkouts.** "the client simply doesn't ask for trees it doesn't want" | Wren-native checkouts fetch only the cone. A `--git` checkout writes git's sparse index, which also needs the trees on the cone's boundary. Stock git reads those trees, but not deeper ones. | Partial |
| **Virtualized checkouts.** Files "materialize on demand" | Wren does not plan this. It needs a filesystem driver, which is a separate product. | Not adopted |
| **Post-merge testing instead of a global merge queue.** | Wren provides the primitives: `landed`, first-parent `bisect` whose steps are fresh sparse checkouts, and read-your-writes queries. It is not a testing pipeline. | Partial |
| **"the various sparse and shallow features in git can be made to compose well"** | Wren hit two rough edges. First, `sparse-checkout reapply` lazily fetches out-of-cone blobs it never writes. Second, a sparse index needs the boundary tree objects to be present. A report for the git list is drafted in [upstream/](upstream/git-sparse-promisor.md) and has not been sent yet. | Partial |
| **RPCs "standardized … across forges that the git client can learn to talk to"** | There is an [API reference](spec/wren-protocol-v1.md) (an unstable draft) and a conformance method that uses git itself as the oracle. A [draft proposal](upstream/git-protocol-v2-query.md) maps the queries to git protocol v2 extensions; it has not been sent. Stock git cannot speak Wren's query API today. | Partial |
| **Hermetic simulation and fuzzing** | A seeded differential fuzz suite generates random histories (branches, merges, large edits, force-pushes) and checks every answer against git. | Follows |
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
