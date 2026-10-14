# Draft: report to the git mailing list (NOT SENT)

> Status: draft for the maintainers to review. It has not been sent. Before
> sending, re-test it on the latest git `master`.
> Context: the Anthropic Scion post hopes that "the various sparse and
> shallow features in git can be made to compose well". Building Wren turned up two concrete
> rough edges.

**Subject:** sparse-checkout + partial clone: `sparse-checkout reapply` fetches out-of-cone blobs; sparse index still needs boundary trees

Hi,

While building a tool that materializes sparse, blobless working trees
(objects come from a separate store via `objects/info/alternates`, and the
origin is configured as a promisor remote), I hit two behaviours that make
sparse checkout and partial clone compose less well than they could. Both
reproduce with git 2.54.

## 1. `git read-tree HEAD && git sparse-checkout reapply` fetches out-of-cone blobs

Setup: a repository with `extensions.partialclone=origin`, cone-mode sparse
checkout enabled, and an index freshly built with `git read-tree HEAD`
(no `-u`). In-cone files are already present in the worktree. Out-of-cone
blobs are absent locally.

Running `git sparse-checkout reapply` sets the skip-worktree bits correctly,
but first lazily fetches the out-of-cone blobs from the promisor remote.
Those blobs are never written to the worktree, so the fetch seems avoidable:
for a path that is about to get skip-worktree, the blob should not be needed.

Repro script: `<to attach>`. Workaround: write the index directly with the
skip-worktree bits already set.

## 2. With a sparse index, `git status` reads each sparse directory's tree object

With `index.sparse=true`, a sparse-directory entry (`dir/`, mode 040000,
skip-worktree) whose tree object is absent locally makes `git status` fail:

    fatal: unable to read tree (<oid of dir>)

The failure happens in `unpack_trees` → `traverse_trees` during the HEAD vs
index comparison. When `GIT_NO_LAZY_FETCH` is unset, the tree is
lazy-fetched instead. Only the boundary tree is read, never its subtrees.
Since the index entry and the HEAD entry carry the same tree OID, the
comparison could arguably skip reading the tree altogether. That would let
`--filter=tree:0`-style clones use sparse indexes without fetching boundary
trees.

The same behaviour occurs with an index produced by
`git sparse-checkout init --cone --sparse-index` after deleting the boundary
tree object. This suggests it is general, not specific to how our index was
written.

Repro script: `<to attach>`.

Thanks for all the work on sparse index and partial clone. Both made the
approach above possible.
