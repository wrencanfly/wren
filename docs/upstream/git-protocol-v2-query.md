# Draft: server-side queries as git protocol v2 extensions (NOT SENT)

> Status: a working draft for discussion. It has not been sent anywhere.
> Before it goes to the git list, every claim about current git behaviour
> must be re-checked against git `master`, and each extension needs a
> prototype in a real `upload-pack`.

## Why

Anthropic's [Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control)
suggests that "Git platforms can provide comprehensive server-side RPCs like
Scion, perhaps even in a standardized way across forges that the git client
can learn to talk to."

Today a client that wants to know whether commit X is in `main`, what the
merge base of two branches is, or which commits touched a path, must first
download the history. Forges answer such questions through their own REST
APIs, which differ from forge to forge and which `git` cannot call.

Wren implements these queries as an HTTP API in front of an unmodified git
host. This note sketches how the same queries could become **git protocol v2
capabilities**, so that a stock git client could ask any server that
advertises them.

## What v2 already does well

These points should be stated up front, because the list will raise them.

- **Exact object fetches.** A client can send only `want` lines, no `have`
  lines, and `done`, with `filter tree:0` or `blob:none`. The server then
  returns essentially the named objects. Lazy fetches for partial clones work
  this way, with the noop negotiator. Wren's "want-only" object protocol adds
  no new capability here; what it adds is per-object HTTP caching.
- **`object-info`** already exists for size and type. It is the natural home
  for small per-object facts.
- **Capability advertisement** with parameters, and `object-format`, are
  more mature than anything in Wren's HTTP API.

## Proposed extensions, smallest first

### 1. More `object-info` attributes

Add commit attributes that commit-graph already computes:

- `generation`: the corrected commit date (generation number v2);
- `first-parent-depth`: the number of first-parent steps to a root.

These let a client do cheap ancestry pruning and bisection bookkeeping
without fetching history. This is the smallest change and the easiest to
review.

### 2. A `query` command

A new v2 command that answers pure functions of commit ids:

```
command=query
object-format=sha1
0001
op=is-ancestor <oid-a> <oid-b>
op=merge-base <oid-a> <oid-b>
op=landed <oid> <tip-oid>              # first-parent commit of tip that contains oid
op=bisect-step <good-oid> <bad-oid>    # first-parent midpoint, or the first bad commit
op=log <tip-oid> <limit> [<path>]      # first-parent commits touching path
0000
```

The capability advertisement would be
`query=is-ancestor merge-base landed bisect-step log semantics=1`. The rules:

- Arguments are full object ids only. Ref names are resolved separately
  with `ls-refs`, so every answer is a pure function of its inputs.
- `semantics=<n>` versions the answers. An implementation that changes what
  an op returns, for example by following renames, bumps it. Any cache can
  then key answers by `(op, args, semantics)` and keep them forever.
- The response is one pkt-line section per op, in request order.
- An unknown oid gets a per-op error line and does not fail the whole
  request.

`blame` is deliberately left out of the first version. Its output is large,
and agreeing on line-attribution semantics across implementations is a
separate discussion. Git's own blame and Wren's already differ in alignment
heuristics.

### 3. `fetch` with `missing-ok`

Today, an unknown `want` makes `upload-pack` die. With `missing-ok`
advertised and requested, the server would skip unknown or unreachable
objects and list them in a `missing-objects` section. That lets a client
batch speculative wants, and it lets caches fill gaps without one bad id
failing everything.

### 4. Pointing clients at nearer caches

`promisor-remote` advertisement can already tell a client about other
remotes. The proposal is to document the pattern "an authoritative server
advertises a cache tier that speaks the same protocol", including how
tokens are scoped, so that CI fleets can put caches in front of forges
without client-side configuration.

## Reference implementation

| Extension | Wren today |
|---|---|
| `object-info` attributes | `GET /api/v1/repos/{repo}/commits/{oid}` returns `depth` (first-parent) and parents; the generation is computed internally. |
| `query` | HTTP endpoints `is-ancestor`, `merge-base`, `landed`, `bisect`, `log`. Each answer is versioned by `wren-semantics` and is immutable when every argument is a full id. Each is checked against git by a differential fuzz suite. |
| `missing-ok` | The object frame format already has a per-object "missing" status. |
| cache tiers | `wren cache` speaks exactly the server's RPCs. |

Wren does not yet speak `git-upload-pack` v2. Adding a v2 front end that
implements `ls-refs`, a filtered `fetch`, and the extensions above is the
next step, so that stock git can be tested against it.

## Open questions

- Should `query` live in `upload-pack`, or be a separate service that does
  not need the object database at all? Wren's answers come from derived
  indexes, not from walking objects.
- How should a server cap the cost of a `query` request, and should it
  advertise those limits?
- Permissions. Answers about refs a client cannot read must not leak.
  Forges that share object pools across forks need per-repository
  reachability checks.
