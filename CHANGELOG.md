# Changelog

## v0.1.0 — 2026-10-18

The first release. Wren is an independent, open-source implementation of
the Scion architecture for git. It is not affiliated with Anthropic and
contains no Anthropic code. It is experimental: read the notes in the
[README](https://github.com/wrencanfly/wren#quick-start) before you deploy it.

### What it does

- **History on the server (M0).** Log by path, blame, diff, merge-base,
  `landed` ("which merge brought this into main?"), bisect, grep, `cat` and
  `ls` at any revision, answered from server-side indexes, with no clone.
  MCP tools and a CLI on top of an HTTP API.
- **Changesets with derived trees (M1).** A changeset records what changed
  relative to its first parent. Trees and commit objects are derived from
  it and checked against git's ids; all 85,545 commits of git/git round-trip.
- **Writes on the server (M2).** Commit from "parent + changed files", then
  rebase, land, merge and revert on the server. Refs move by
  compare-and-swap, with no merge queue. Rebases and merges agree with
  `git merge-tree`; where git would follow a rename, Wren reports a conflict
  instead of giving a different answer.
- **Storage without maintenance (M3).** Immutable segments in git's pack
  format, never repacked, in a local directory or in S3, GCS or Azure, read
  by range. Delta frames on the wire. Postgres metadata lets any number of
  stateless servers share the work.
- **Cache tiers and a node daemon (M4).** `wren cache` tiers speak the same
  want-only object RPC as the server. `wren daemon` keeps a node's store
  under a byte budget and seeds it from snapshots. The tiers are tested by
  simulation under injected faults.
- **Stock git still works (M5).** Clone, fetch (partial clones too) and push
  through a git protocol v2 front end. With `--source-of-truth wren`, the git
  host becomes a downstream mirror. Protected refs, a policy hook and an
  audit log stand in for the host's protections.
- **Search and permissions (M6).** Renames and copies are detected as git
  does. Paths are searched by trigrams. A trigram index over blobs lets grep
  read only the files that can match. Read tokens can be restricted to
  directories, on the query API and on the object plane. Each client gets a
  rate limit.
- **Virtual checkouts (M7).** `wren mount` shows the whole tree at once and
  fetches files when they are read. Writes go to an overlay. It uses FUSE on
  Linux and a localhost NFS server on macOS, with nothing to install. With
  `--git` it is a git working tree whose `git status` reads only what you
  wrote.

### Known limitations

- The HTTP API and wire protocol are an unstable draft and may change in any
  0.x release.
- Restricted tokens cannot use caches, the git front end or `--git`
  checkouts.
- No TLS of its own: put Wren behind a TLS-terminating proxy.
- Windows is not supported.
- `git` (2.26 or newer) must be on `PATH` for ingestion, merges and `--git`
  checkouts.

### Binaries

Built with `--features vfs`, so `wren mount` is included:

- `wren-v0.1.0-x86_64-unknown-linux-gnu.tar.gz`
- `wren-v0.1.0-aarch64-unknown-linux-gnu.tar.gz`
- `wren-v0.1.0-aarch64-apple-darwin.tar.gz`

`wren mount` on Linux needs `fusermount3` (the `fuse3` package). Each
archive has a `.sha256` file next to it.
