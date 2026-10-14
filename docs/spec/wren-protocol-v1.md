# Wren HTTP API Reference (protocol v1)

| | |
|---|---|
| Document status | **Unstable draft.** This is not a standard. No forge, git project, or standards body has reviewed it. Routes, fields, and answers may still change. |
| Protocol major version | **1** (the `wren-protocol` header, [§1.2](#12-protocol-version-and-the-wren-protocol-header)) |
| Query semantics version | **`1`** (the `wren-semantics` header, [§7.3](#73-the-immutability-contract)) |
| Reference implementation | `crates/wren` (server, cache, client) and `crates/wren-core` |
| Reference test suite | `crates/wren-core/tests/differential.rs` (git as oracle), `crates/wren/tests/e2e.rs`; for writes, `crates/wren-core/tests/writes.rs` and `crates/wren/tests/writes_http.rs` |
| Object format | `sha1` only |
| Revision | Fourth draft. The third added `grep`, error codes, the semantics version, on-demand sync, and the `wren-synced-at` header; the fourth adds the write API ([§14](#14-writes)). See [§13](#13-open-issues). |

This document describes the HTTP API served by Wren, a git-compatible
source-control acceleration layer that answers history queries on the server.
It is the reference for Wren's own server, cache, and clients.

It is also meant as a concrete starting point for standardizing server-side
source-control RPCs across forges (GitHub, GitLab, Gitea, ...). It is written
precisely enough that an independent implementation could serve the same API,
and that any client could then talk to any server or cache that follows it.
[Wren and the Scion principles](../principles.md) explains the motivation, and
[a draft proposal](../upstream/git-protocol-v2-query.md) sketches the same
queries as git protocol v2 extensions. Neither has been sent anywhere.

Wren is an independent project. It is not affiliated with, reviewed by, or
endorsed by Anthropic.

**Source of truth.** Where this document describes behaviour, it describes what
the reference implementation does. Where the implementation is ambiguous,
inconsistent, or diverges from git, the behaviour is still documented as-is and
the problem is listed in [Open issues](#13-open-issues). Sections and
paragraphs marked *(informative)* or *(non-normative)* do not impose
requirements. MUST and SHOULD state what an implementation of this draft has
to do; they do not mean the draft is final.

---

## Contents

0. [Conventions and terminology](#0-conventions-and-terminology)
1. [Scope and versioning](#1-scope-and-versioning)
2. [Authentication and tiers](#2-authentication-and-tiers)
3. [Object protocol](#3-object-protocol)
4. [Query API](#4-query-api)
5. [Revision syntax](#5-revision-syntax)
6. [Errors](#6-errors)
7. [Caching](#7-caching)
8. [Conformance](#8-conformance)
9. [Relationship to git (non-normative)](#9-relationship-to-git-non-normative)
10. [Known gaps in v1](#10-known-gaps-in-v1)
11. [Security considerations](#11-security-considerations)
12. [Worked example (informative)](#12-worked-example-informative)
13. [Open issues](#13-open-issues)
14. [Writes](#14-writes)

---

## 0. Conventions and terminology

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

| Term | Meaning |
|---|---|
| **oid** | A git object id. In v1, a SHA-1 digest: 20 raw bytes on the wire, or 40 lowercase hexadecimal characters in JSON and URLs. |
| **Loose object** | git's loose-object encoding of an object: `zlib("<type> <size>\0<content>")` ([§3.1](#31-object-encoding)). |
| **Tier** | Any HTTP endpoint that speaks this protocol. There are two kinds: the **server** (authoritative) and a **cache** (sits in front of exactly one upstream tier, which is a server or another cache). |
| **Client** | Anything that sends requests to a tier, for example the `wren` CLI or a cache talking to its upstream. |
| **Repository** (repo) | A named history served by a server, mirrored from an upstream git URL. Named by the `{repo}` path segment. |
| **Changeset** | Wren's record of one commit: the commit's tree, its parents, and the list of paths it changed **relative to its first parent** ([§4.0.5](#405-the-change-object)). Every commit the server has ingested is a changeset. Changesets are never deleted, so a commit that later becomes unreachable (for example after a force-push upstream) stays queryable by id. |
| **First-parent chain** of commit `C` | The sequence `C, C^1, C^1^1, ...` down to a root commit. |
| **generation** | 1 for a root commit, otherwise `1 + max(generation of each parent)`. Any proper ancestor has a strictly smaller generation. This equals git's commit-graph "topological level" (generation number v1). |
| **depth** | The number of first-parent steps from a commit to a root commit. A root commit has depth 0. Two commits on the same first-parent chain are exactly `|depth(a) - depth(b)|` steps apart. |
| **Pinned query** | A query request whose answer can never change, as defined precisely in [§7.1](#71-pinned-queries). |
| **Semantics version** | An opaque string naming the version of the query answers' semantics, sent as `wren-semantics`. A pinned answer is fixed for a given semantics version ([§7.3](#73-the-immutability-contract)). |

Byte orders: all multi-byte integers in the object protocol are **big-endian**.

---

## 1. Scope and versioning

### 1.1 Scope

This specification covers:

* the **object protocol**, which fetches git objects by id ([§3](#3-object-protocol));
* the **query API**, which answers history questions such as log, blame, ancestry, merge-base, "landed", bisect, and grep ([§4](#4-query-api));
* the **capabilities** document and the version headers ([§1.2](#12-protocol-version-and-the-wren-protocol-header), [§1.3](#13-capabilities-get-v1capabilities));
* **authentication** and the **tier model** ([§2](#2-authentication-and-tiers));
* **caching** rules ([§7](#7-caching));
* the **write API**, which creates changesets, lands them, moves branches, and reverts, on a server started with a write token ([§14](#14-writes)).

The following are out of scope: how a server ingests history from its upstream,
the on-disk storage format, the derived-data indexes, the CLI, and the MCP
interface (`crates/wren/src/mcp.rs`). The MCP interface is a separate client
of this API. Its tools first resolve every revision to a full commit id with
[`resolve`](#43-resolve), and then send only pinned queries
([§7.1](#71-pinned-queries)), so any cache tier can answer them. For example,
`wren_bisect_step` is a thin wrapper over [`bisect`](#49-bisect). Tool output
is capped (40 000 characters by default, adjustable per call), and a cut
output says so. The client checkout algorithm is described in
[§3.7](#37-client-checkout-algorithm-informative) for illustration only.

### 1.2 Protocol version and the `wren-protocol` header

The protocol major version defined here is **1**.

* A tier MUST include the response header `wren-protocol: 1` on **every** HTTP
  response it sends, including `/healthz`, error responses (`401`, `404`,
  `405`, ...), and responses relayed from an upstream tier.
* A client SHOULD check this header, or the `protocol` field of
  `/v1/capabilities`, before relying on any v1 semantics. If the value is not
  `1`, the client SHOULD stop.

Every response also carries a `wren-semantics` header: the version of the
query answers' semantics, currently `1` (`QUERY_SEMANTICS` in
`crates/wren-core/src/api.rs`). It is separate from the protocol version. It
changes when the server changes what it answers, not how it is asked
([§7.3](#73-the-immutability-contract)). The value is an opaque string.
Clients and caches MUST compare it only for equality.

* The server MUST send `wren-semantics` on every response, including
  `/healthz` and errors.
* A cache MUST relay the upstream's `wren-semantics` value on every forwarded
  query response, and on a stored response it MUST send the value the
  response was stored under ([§7.2](#72-marking-and-storing)). On responses it
  produces itself (objects, capabilities, `/v1/stats`, `/healthz`, its own
  errors), and on a forwarded response that lacks the header, the reference
  cache sends its own built-in value ([Open issues](#13-open-issues), item 27).

### 1.3 Capabilities: `GET /v1/capabilities`

Returns a description of the tier. Every tier MUST serve it, and the response
shape is identical on every tier.

**Response** `200 OK`, `Content-Type: application/json`:

```json
{
  "protocol": 1,
  "object_format": "sha1",
  "tier": "server",
  "features": [
    "objects-batch",
    "objects-single",
    "query-api",
    "bisect",
    "landed",
    "grep",
    "immutable-pinned-queries",
    "on-demand-sync",
    "error-codes"
  ],
  "semantics": "1"
}
```

A server started with a write token adds `"write.v1"` to `features`
([§14](#14-writes)).

| Field | Type | Meaning |
|---|---|---|
| `protocol` | integer | Protocol major version. MUST be `1` for this specification. |
| `object_format` | string | The hash algorithm of every oid on this tier. MUST be `"sha1"` in v1 ([§1.5](#15-object-format)). |
| `tier` | string | `"server"` or `"cache"`: which kind of tier answered. Clients MUST NOT change their behaviour based on this value ([§2.2](#22-tiers)). |
| `features` | array of strings | Feature flags. The reference implementation advertises exactly the nine values above on both tiers (`FEATURES` in `crates/wren/src/server.rs`), plus `write.v1` on a server with a write token. |
| `semantics` | string | The query semantics version ([§1.2](#12-protocol-version-and-the-wren-protocol-header)). A server reports its own. A cache reports the value it last learned from its upstream, or its own built-in value if it has learned none ([§7.2](#72-marking-and-storing)). Clients MUST treat an absent field (older servers) as unknown. |

Feature flags defined in v1:

| Flag | Meaning |
|---|---|
| `objects-batch` | `POST /v1/objects` ([§3.2](#32-batch-fetch-post-v1objects)) |
| `objects-single` | `GET /v1/objects/{oid}` ([§3.3](#33-single-fetch-get-v1objectsoid)) |
| `query-api` | The `/api/v1/...` endpoints of [§4](#4-query-api) |
| `bisect` | `GET /api/v1/repos/{repo}/bisect` ([§4.9](#49-bisect)) |
| `landed` | `GET /api/v1/repos/{repo}/landed` ([§4.8](#48-landed)) |
| `grep` | `GET /api/v1/repos/{repo}/grep` ([§4.15](#415-grep)) |
| `immutable-pinned-queries` | Pinned query responses carry `Cache-Control: ... immutable` ([§7](#7-caching)) |
| `on-demand-sync` | An unknown revision triggers one sync with upstream before the server answers `404` ([§4.0.7](#407-unknown-revisions-and-on-demand-sync)) |
| `error-codes` | Errors produced by handlers carry a machine-readable `code` ([§6.1](#61-errorbody)) |
| `write.v1` | The write API of [§14](#14-writes). Only a server with a write token advertises it; a cache never does. |

Clients MUST ignore feature flags they do not recognise.

### 1.4 Compatibility rules

* All v1 routes live under `/v1/...` (object protocol and capabilities) and
  `/api/v1/...` (query API). `/healthz` is unversioned.
* A **breaking change** MUST bump the protocol major version and move the
  affected routes to new prefixes: `/v2/...` and `/api/v2/...`. Breaking
  changes include removing or renaming a field or endpoint, changing a field's
  type or meaning, changing the frame layout, and changing the object format.
* Changing the **answer** to an already-answerable pinned query is not a
  protocol break. It MUST instead come with a new query semantics version
  ([§7.3](#73-the-immutability-contract)).
* Non-breaking changes MAY be made within v1: new endpoints, new optional query
  parameters, new JSON fields, new feature flags. A new field in the answer to
  a pinned query changes that answer, so it also needs a new semantics
  version.
* While this document is a draft, these rules describe intent. The draft
  itself may still change v1 in place.
* Clients MUST ignore unknown JSON fields in responses. (The reference client
  does this, because its serde types do not deny unknown fields.)
* Servers ignore unknown query parameters ([§4.0.2](#402-query-parameter-encoding)).
* A tier MAY serve several major versions side by side under their respective prefixes.

### 1.5 Object format

v1 supports SHA-1 object ids only. `object_format` MUST be `"sha1"`, oids are 20
bytes in frames and 40 hex characters in text.

A future SHA-256 deployment would be signalled by `object_format: "sha256"`. A
client that does not support the advertised `object_format` MUST NOT use the
object protocol or interpret oids from that tier. The v1 frame layout has a
fixed 20-byte oid field ([§3.2](#32-batch-fetch-post-v1objects)), so this
document does not define how SHA-256 objects would be carried (see
[Open issues](#13-open-issues), item 17).

---

## 2. Authentication and tiers

### 2.1 Bearer tokens

A tier MAY be configured with a single shared secret token. When a tier has a token:

* Every request except `GET /healthz` MUST carry
  `Authorization: Bearer <token>`. The scheme is matched case-sensitively as the
  literal prefix `Bearer ` (one space), and the token is compared byte-for-byte.
  The reference implementation compares in constant time.
* A request without a valid token MUST be rejected with `401 Unauthorized` and
  the [ErrorBody](#61-errorbody)
  `{"error":"missing or invalid bearer token","code":"unauthorized"}`.
  Authentication happens before routing, so an unknown path also gets `401`,
  not `404`.
* A tier with a token marks immutable responses `Cache-Control: private, ...`
  instead of `public, ...`, so that shared HTTP caches and CDNs do not keep
  them ([§7.2](#72-marking-and-storing)).
* `GET /healthz` is exempt. It returns `200 OK`, `Content-Type: text/plain;
  charset=utf-8`, body `ok`.

When a tier has no token, all requests are accepted. Tokens are per tier. A
cache has its own client-facing token, and it presents a separately configured
upstream token to its upstream tier. A cache never forwards the client's
`Authorization` header.

The token is not repository-scoped. Any valid token grants access to every
repository and to every object in the server's object store ([§11](#11-security-considerations)).

A server MAY also have a write token, which enables the write API and is
accepted for reads as well. The read token never grants writes
([§14.1](#141-authentication)).

### 2.2 Tiers

Tiers form a tree: clients talk to a cache or to the server, and each cache has
exactly one upstream, which is a server or another cache.

* A cache MUST answer **the same RPCs** as the server: every route in [§3](#3-object-protocol) and
  [§4](#4-query-api), plus `/v1/capabilities` and `/healthz`, with the same
  request syntax and the same response semantics. A client cannot tell, and
  need not know, which tier it is talking to.
* A cache MUST NOT alter responses. Object bytes MUST be relayed exactly as
  received from upstream. Forwarded query responses MUST be relayed with the
  same status code, body, `Content-Type`, `Cache-Control`, `wren-semantics`,
  and `wren-synced-at` ([§4.0.8](#408-freshness-the-wren-synced-at-header)).
  The reference cache relays exactly these headers and no others. The only
  permitted differences are: the `tier` field of `/v1/capabilities`; transport
  headers such as `Date` and `Content-Length`; and, on a response served from
  the cache's own store, a `Cache-Control` value that the cache sets itself
  (`private` if the cache has a client token, otherwise `public`,
  [§7.2](#72-marking-and-storing)).
* Errors that originate in the cache itself, rather than being relayed, are
  reported as in [§6.3](#63-errors-at-caches). An upstream failure is `502 Bad Gateway`.
* A cache MUST NOT serve a stored query response unless upstream marked that
  response immutable, and the response was stored under the semantics version
  the cache currently believes its upstream serves
  ([§7.2](#72-marking-and-storing)). Every other query MUST be forwarded
  upstream.
* A cache MAY expose additional, cache-specific routes. The reference cache
  serves `GET /v1/stats` (OPTIONAL, not advertised in `features`), which returns:

  ```json
  {"hits":12,"misses":3,"hit_rate":0.8,"api_hits":4,"api_misses":9,"upstream_requests":10}
  ```

  `hits` and `misses` count objects served from the local store versus fetched
  from upstream. `hit_rate` is `hits/(hits+misses)`, or `0.0` when both are
  zero. `api_hits` and `api_misses` count GET query requests served from, or
  missing from, the immutable response store. `upstream_requests` counts every
  request sent upstream. The server does not serve this route.

*(informative)* The reference cache bounds concurrent upstream requests with a
semaphore (default 32) and applies an upstream timeout (default 120 s), so a
cold or flooded cache cannot overwhelm the tier above it.

---

## 3. Object protocol

### 3.1 Object encoding

Objects travel in git's **loose-object encoding**: a zlib stream (RFC 1950)
whose inflated content is

```
<type> SP <size> NUL <content>
```

`<type>` is one of `commit`, `tree`, `blob`, `tag`. `<size>` is the decimal byte
length of `<content>`. The object's oid is the SHA-1 of the entire inflated byte
string, header included, exactly as in git. These bytes are what the reference
server stores, ships over the wire, and what a client can drop into
`.git/objects/xx/yyyy...` unchanged.

The zlib compression level is not specified. Two servers MAY therefore send
different bytes for the same oid. The **inflated** bytes are always identical,
because they hash to the oid. A cache relays the bytes it received verbatim.

### 3.2 Batch fetch: `POST /v1/objects`

**Request**

```
POST /v1/objects
Content-Type: application/json

{"oids": ["136c59eb0d199f2f9181ea4db8f448b4df4d3f32", "..."]}
```

* `Content-Type: application/json` is REQUIRED. Without it, the reference
  implementation answers `415`.
* `oids` is an array of oids, each a 40-character hex string. Clients SHOULD send
  lowercase. The reference server also accepts uppercase. A malformed oid causes
  `422` in the reference implementation ([§6](#6-errors)).
* **Batch limit:** at most **10 000** oids per request (`MAX_BATCH`). Server and
  cache both answer a larger request with `413 Payload Too Large` and
  `{"error":"at most 10000 oids per request","code":"too_large"}`. The
  reference client sends batches of 1 000.
* An empty array is valid and yields `200` with an empty body.
* Duplicate oids are allowed. Each occurrence gets its own frame.

**Response** `200 OK`, `Content-Type: application/x-wren-objects`. The body is
the concatenation of exactly *N* frames, where *N* is the length of `oids`. There
is no header, trailer, or count. The end of the body is the end of the stream. No
`Cache-Control` header is set, because a batch response as a whole is not
immutable: a frame that is missing now may be found later.

**Frame layout**

| Offset | Size (bytes) | Field | Encoding |
|---:|---:|---|---|
| 0 | 20 | `oid` | Raw SHA-1 bytes of the requested oid |
| 20 | 1 | `status` | `0x00` = found, `0x01` = missing. Any other value is invalid and the receiver MUST treat the stream as corrupt. |
| 21 | 4 | `len` | Unsigned 32-bit **big-endian** length of `data`. MUST be `0` when `status = 0x01`. |
| 25 | `len` | `data` | The object's loose encoding ([§3.1](#31-object-encoding)). Absent when missing. |

A frame is therefore exactly `25 + len` bytes. Because `len` is 32 bits, an
object whose loose encoding is 2³² bytes (4 GiB) or larger cannot be framed in
v1. A tier MUST NOT truncate the length. The reference implementation fails the
whole request with `500` (`object <oid> is too large for a v1 frame (<n> bytes)`)
instead ([Open issues](#13-open-issues), item 24). The receiver MUST treat a body
that ends in the middle of a frame as an error (the reference decoder reports
"truncated object stream"). Frames MAY be split arbitrarily across
transport chunks, and the reference decoder is incremental.

Example, from the reference server, for
`{"oids":["0000000000000000000000000000000000000000","136c59eb0d199f2f9181ea4db8f448b4df4d3f32"]}`:

```
00000000  00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00   frame 1: oid (20 bytes, all zero)
00000010  00 00 00 00 01 00 00 00 00 13 6c 59 eb 0d 19 9f   status=01 (missing), len=0 | frame 2: oid 136c59eb...
00000020  2f 91 81 ea 4d b8 f4 48 b4 df 4d 3f 32 00 00 00   ... status=00 (found), len=
00000030  00 9b 78 01 95 8e 4b 0a 02 31 10 44 5d e7 14 bd   0x0000009b (155) | data: zlib stream 78 01 ...
```

**Ordering.** Frames MUST appear in request order: frame *i* answers `oids[i]`.
The `oid` field of each frame MUST equal the requested oid.

**Missing-object semantics.** `status = 0x01` means "this tier cannot supply
this object". It is not an error. The HTTP status stays `200`, and other frames in
the same response are unaffected. For the server, missing means the object is
not in its object store. For a cache, missing means it is neither stored locally
nor returned by its upstream. A client that receives a missing frame MAY retry
later or against another tier. Missing results are never cached.

**Object scope.** The object namespace is **per server, not per repository**:
any object in the server's store can be fetched regardless of which repository
it belongs to (see [Open issues](#13-open-issues), item 10).

### 3.3 Single fetch: `GET /v1/objects/{oid}`

`{oid}` is a 40-character hex oid.

| Status | Body | Headers |
|---|---|---|
| `200` | The object's loose encoding (raw bytes, not framed) | `Content-Type: application/x-git-loose-object`, `Cache-Control: public, max-age=31536000, immutable` (`private` instead of `public` when the tier has a token) |
| `404` | ErrorBody `{"error":"object <oid> not found","code":"not_found"}` (server and cache alike) | `Content-Type: application/json` |
| `400` | ErrorBody, e.g. `{"error":"oid must be 40 hex chars: \"xyz\"","code":"bad_request"}` | when `{oid}` is not 40 hex characters |
| `502` | ErrorBody `{"error":"upstream: ...","code":"upstream_unavailable"}` | cache only: the object is not stored locally and the upstream fetch failed ([§6.3](#63-errors-at-caches)) |

This route is suitable for plain HTTP caches and CDNs, when the tier has no
token.

### 3.4 Verification

* A client MUST verify every object it receives before trusting or storing it.
  It MUST inflate `data`, parse the `<type> <size>\0` header, check that the
  content length equals `<size>`, compute SHA-1 over the full inflated bytes,
  and compare the result with the frame's oid (or the URL oid). On mismatch, the
  client MUST discard the object. The reference client does this
  (`put_raw_verified`) and treats a mismatch as a failed batch, which it retries.
* Because clients verify, caches need not be trusted for integrity. A cache
  SHOULD still verify before storing, so that a corrupted upstream response does
  not become permanent, because stored objects are never revalidated. The reference
  cache verifies every object it receives from upstream (the miss path) with
  `put_raw_verified` before storing it. A verification failure fails the request
  with `502` and nothing from that upstream response is relayed as found. Objects
  served from the cache's own store (the hit path) are not re-verified.

### 3.5 Immutability

The answer for a given oid can never change: it is either "missing" or the one
object that hashes to that oid. Consequently:

* `GET /v1/objects/{oid}` responses with status `200` are marked
  `Cache-Control: public, max-age=31536000, immutable` (`private, ...` when the
  tier has a token), and any cache that the marking admits MAY store them
  forever.
* A tier MAY keep any found object forever and MUST NOT revalidate it.
* A *missing* answer is not immutable: the object may arrive later, for example after a sync.

### 3.6 The want-only principle

Clients **never** describe what they already have. There is no have/want
negotiation, no common-ancestor computation, and no server-side delta
compression. A client names exactly the objects it wants, by id.

Consequences, which motivate the design:

1. **Any tier can answer.** The response to "send me object X" depends on
   nothing but X. A cache needs no knowledge of the client's state, the
   repository's refs, or the history graph to answer from its store, and the
   answer it gives is the same as the server's.
2. **Responses are immutable and cacheable,** so caches never coordinate or
   revalidate ([§3.5](#35-immutability)).
3. **Resumption is trivial.** After a failed or partial transfer the client
   recomputes what it still lacks locally and asks again for exactly that. The
   reference client retries up to 4 rounds, each time requesting only the oids
   still absent from its store.
4. **Cost scales with what is read.** A client that walks only part of a tree
   (for example a sparse checkout) fetches only that part.

The trade-off is that the client must discover object ids itself: from a
commit to its tree, from a tree to its entries, and so on, or from the query
API. This takes one round trip per tree level ([§3.7](#37-client-checkout-algorithm-informative)).

### 3.7 Client checkout algorithm (informative)

This section describes what the reference client (`crates/wren/src/checkout.rs`,
`crates/wren/src/client.rs`) does. It is not normative. Other clients MAY
fetch in any order.

1. **Resolve** the requested revision with `GET /api/v1/repos/{repo}/resolve?rev=...` ([§4.3](#43-resolve)).
2. **Ensure** the commit object. `ensure(oids)` deduplicates the list, drops oids
   already in the local store, sends the rest in batches of 1 000 with up to 16
   batches in flight, verifies and stores every found object, then recomputes
   what is still missing and retries, for at most 4 rounds. If objects are still
   missing after that, it fails.
3. **Walk trees breadth-first, level by level.** Start with the commit's root
   tree. For each level, `ensure` all tree oids of the level, parse them, and
   collect the next level. With a sparse "cone" (a set of directories), descend
   only into directories inside the cone or on the path to a cone directory.
   Files directly in the root and in every ancestor of a cone directory are
   included, which matches git's cone-mode semantics.
4. In `--git` mode, a directory outside the cone becomes a single sparse-index
   entry pointing at its tree oid. Only that boundary tree object is fetched,
   never anything below it, unless `--full-index` is used, in which case trees
   (never blobs) outside the cone are walked as well.
5. **Fetch blobs** for every file inside the cone, skipping gitlinks (submodules,
   mode `160000`), which are never fetched and are materialised as empty
   directories.
6. **Materialise** files in parallel: regular files, executables (`100755` gets
   permission `0755`), and symlinks (`120000`, whose target is the blob content).
7. Optionally (`--git`) write a `.git` directory whose `objects/info/alternates`
   points at the client's shared object store. This works because the store uses
   git's loose layout. It records the commit as `shallow`, configures the
   upstream git URL as a promisor remote (`partialclonefilter = blob:none`), and
   writes the index directly.

The walk costs one request round per tree level (×⌈n/1000⌉ batches), plus one
round for blobs.

---

## 4. Query API

### 4.0 Common rules

#### 4.0.1 Routes

All query endpoints are under `/api/v1/`. Repository-scoped endpoints are under
`/api/v1/repos/{repo}/`, where `{repo}` is a single path segment. An unknown
`{repo}` yields `404` and `{"error":"unknown repo \"<name>\"","code":"not_found"}`.

| Method | Path | § |
|---|---|---|
| GET | `/api/v1/repos` | [4.1](#41-list-repositories) |
| GET | `/api/v1/repos/{repo}/refs` | [4.2](#42-refs) |
| GET | `/api/v1/repos/{repo}/resolve` | [4.3](#43-resolve) |
| GET | `/api/v1/repos/{repo}/commits/{rev}` | [4.4](#44-commit) |
| GET | `/api/v1/repos/{repo}/log` | [4.5](#45-log) |
| GET | `/api/v1/repos/{repo}/merge-base` | [4.6](#46-merge-base) |
| GET | `/api/v1/repos/{repo}/is-ancestor` | [4.7](#47-is-ancestor) |
| GET | `/api/v1/repos/{repo}/landed` | [4.8](#48-landed) |
| GET | `/api/v1/repos/{repo}/bisect` | [4.9](#49-bisect) |
| GET | `/api/v1/repos/{repo}/diff` | [4.10](#410-diff) |
| GET | `/api/v1/repos/{repo}/blame` | [4.11](#411-blame) |
| GET | `/api/v1/repos/{repo}/tree` | [4.12](#412-tree) |
| GET | `/api/v1/repos/{repo}/file` | [4.13](#413-file) |
| POST | `/api/v1/repos/{repo}/sync` | [4.14](#414-sync) |
| GET | `/api/v1/repos/{repo}/grep` | [4.15](#415-grep) |

The write endpoints (`POST` on `blobs`, `changesets`, `land`, `refs`, and
`revert`) are described in [§14](#14-writes).

A request with the wrong method on a known path gets `405 Method Not Allowed`
with an `Allow` header (for example `Allow: GET,HEAD`) and an empty body.

#### 4.0.2 Query-parameter encoding

* Parameters use standard `application/x-www-form-urlencoded` query syntax and
  are percent-decoded by the server.
* **Booleans** (`changes`, `patch`, `ignore_case`) accept exactly `true` or
  `false`. Any other value (for example `1`) is a `400`. An absent boolean is
  `false`.
* **Integers** (`limit`) are unsigned decimal. A non-numeric value is a `400`.
* Unknown parameters are ignored. Repeating a known parameter is a `400`.
* A missing **required** parameter is a `400`.
* Parameter-parsing errors produce a `text/plain` body, not an ErrorBody
  ([§6](#6-errors), [Open issues](#13-open-issues), item 5).

#### 4.0.3 Revision parameters

The parameters `rev`, `a`, `b`, `base`, `head`, `commit`, `ref`, `good`, `bad`, and the
`{rev}` path segment of `/commits/{rev}`, are **revisions**, resolved by the
syntax of [§5](#5-revision-syntax). Every revision resolves to a commit oid.
Where a revision parameter is optional and absent, it defaults to `HEAD`. A
revision the server does not know triggers one on-demand sync before the
server answers `404` ([§4.0.7](#407-unknown-revisions-and-on-demand-sync)).

#### 4.0.4 Path parameters

The `path` parameter names a path inside a commit's tree. It is **normalised**
before use: split on `/`, drop empty segments and `.` segments, and rejoin with
`/`. So `/src//./a.txt/` becomes `src/a.txt`. `..` is not special: it is looked
up as a literal name. Matching is byte-exact and case-sensitive. A path that
normalises to the empty string means the root tree.

#### 4.0.5 The Change object

A change describes one path that differs between two trees. Only non-tree
entries (files, symlinks, gitlinks) appear. Directories never do.

```json
{"path":"src/a.txt","status":"M",
 "old":{"mode":"100644","oid":"4cb29ea38f70d7c61b2a3a25b02e3bdf44905402"},
 "new":{"mode":"100644","oid":"d7ed9e6a2d578accc8bcf915f28aff2a60475f9a"}}
```

| Field | Type | Meaning |
|---|---|---|
| `path` | string | Full path from the tree root, `/`-separated. Non-UTF-8 bytes are replaced with U+FFFD. |
| `status` | string | One letter. See below. |
| `old` | FileVersion, **omitted** if absent | The entry before the change. Present for `M`, `D`, `T`. |
| `new` | FileVersion, **omitted** if absent | The entry after the change. Present for `A`, `M`, `T`. |
| `*.mode` | string | The git file mode as a six-digit **octal string**, exactly as git writes it: `"100644"`, `"100755"`, `"120000"` (symlink), `"160000"` (gitlink). This is the same representation as tree listings ([§4.12](#412-tree)). |
| `*.oid` | string | Blob oid (or commit oid for a gitlink). |

Status letters:

| Letter | Meaning |
|---|---|
| `A` | Added: the path is absent in the old tree, or is a directory there. |
| `M` | Modified: content and/or mode changed, and the file type (`mode & 0o170000`) is unchanged. A pure `644`↔`755` mode change is `M`. |
| `D` | Deleted: the path is absent in the new tree, or is a directory there. |
| `T` | Type changed: both sides are non-trees and the file type differs (regular ↔ symlink ↔ gitlink). |

When a file is replaced by a directory of the same name, or the reverse, the
result is a `D` for the file plus `A`s for the directory's files, or the
reverse. Renames are never detected: a rename is a `D` plus an `A`. Arrays of
changes are sorted by `path` in ascending byte order. Legacy tree modes are
normalised the way git does (for example `100664` becomes `100644`).

These semantics equal `git diff --name-status --no-renames` (letters and paths).

A **changeset's changes** are the changes between the commit's first parent's
tree and its own tree. For a root commit, they are the changes from the empty
tree, so every file is `A`. For a merge commit, they are the changes relative to
the **first** parent only. This equals
`git diff-tree -r --no-renames --name-status --root -m --first-parent <commit>`.

#### 4.0.6 The CommitInfo object

```json
{
  "oid": "136c59eb0d199f2f9181ea4db8f448b4df4d3f32",
  "tree": "ace4f9e27aee65aa15ce51848403e29cbd9a8e17",
  "parents": ["f5a007458f7ee8f53f440e457a55a2735f070a15"],
  "author":    {"name":"A U Thor","email":"a@example.com","time":1700000000,"tz":"+0000"},
  "committer": {"name":"A U Thor","email":"a@example.com","time":1700000000,"tz":"+0000"},
  "message": "add zero and link\n",
  "generation": 4,
  "depth": 3,
  "changes": [
    {"path":"src/a.txt","status":"M",
     "old":{"mode":"100644","oid":"4cb29ea38f70d7c61b2a3a25b02e3bdf44905402"},
     "new":{"mode":"100644","oid":"d7ed9e6a2d578accc8bcf915f28aff2a60475f9a"}},
    {"path":"src/link","status":"A",
     "new":{"mode":"120000","oid":"8d14cbf983b3fad683171c9418998d9f68340823"}}
  ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `oid` | string | Commit id. |
| `tree` | string | Root tree id. |
| `parents` | array of strings | Parent ids in commit order. The first entry is the first parent. Empty for a root commit. |
| `author`, `committer` | Signature | `name`: the text before the first `<`, with trailing whitespace trimmed. `email`: the text between the first `<` and the last `>`. `time`: seconds since the Unix epoch, as written in the commit (`0` if unparsable). `tz`: the offset string as written, for example `"+0200"` (`"+0000"` if absent). |
| `message` | string | The full commit message: everything after the first blank line of the raw commit, including the trailing newline. Converted to UTF-8 lossily. The `encoding` header is not honoured. |
| `generation` | integer | See [§0](#0-conventions-and-terminology). Root = 1. |
| `depth` | integer | First-parent steps to a root. Root = 0. |
| `changes` | array of Change, **omitted** unless requested | The changeset's changes (relative to the first parent), when the endpoint was called with `changes=true`. |

#### 4.0.7 Unknown revisions and on-demand sync

A server that advertises `on-demand-sync` gives clients read-your-writes for
new commits and new refs. The reference server does this in
`ServerState::resolve` (`crates/wren/src/server.rs`):

1. When resolving a revision parameter fails with `404` (any not-found cause
   of [§5](#5-revision-syntax): unknown name or id, `~N` past the root, a
   nonexistent `^N` parent, a missing `HEAD`), and the repository has an
   upstream, the server syncs the repository once
   ([§4.14](#414-sync)) and resolves the revision again. The second result is
   the answer.
2. At most one on-demand sync of a repository runs at a time. Requests that
   arrive while one runs wait for it and then reuse its result instead of
   starting another.
3. On-demand syncs of one repository are at least 1 s apart. If the previous
   on-demand sync finished less than 1 s ago, the server resolves again
   without syncing. Periodic syncs and `POST .../sync` do not count.
4. A failed on-demand sync is logged and recorded in `last_sync_error`
   ([§4.1](#41-list-repositories)). The request then gets the result of the
   second resolution, usually the same `404`.

Only revision resolution triggers this. A missing `path` at a known revision
does not, and neither do `refs` and `GET /api/v1/repos`. A revision that
resolves without error is never refreshed: a branch name that upstream has
moved still names the commit it named at the last sync. So a client that has
just pushed can ask about the new commit id, or a new branch, immediately. To
see an existing branch's new position, it has to call
[`POST .../sync`](#414-sync) or wait for the next periodic sync.

#### 4.0.8 Freshness: the `wren-synced-at` header

Branch-relative answers reflect upstream as of the repository's last
successful sync. The server makes this visible on every response to a request
under `/api/v1/repos/{repo}/...` where `{repo}` is a known repository and the
request is not pinned ([§7.1](#71-pinned-queries)):

```
wren-synced-at: 1791927298
```

The value is `last_sync` ([§4.1](#41-list-repositories)): the Unix time
(seconds) of the last successful sync, read after the request was handled. It
therefore includes an on-demand sync or `POST .../sync` made by that request.
The header is sent on errors too, including framework rejections. It is
omitted when the repository has never synced. It is never sent on pinned
requests, whose answers do not depend on refs, nor on `GET /api/v1/repos`,
which reports `last_sync` per repository in its body. A cache relays the
header on forwarded responses ([§2.2](#22-tiers)). Responses served from a
cache's store are pinned and never carry it.

### 4.1 List repositories

`GET /api/v1/repos`. No parameters. Never pinned.

**Response** `200`: an array sorted by `name`.

```json
[
  {
    "name": "demo",
    "upstream": "https://git.example.com/demo.git",
    "head": "refs/heads/main",
    "changesets": 5,
    "last_sync": 1791927298,
    "last_sync_error": null,
    "tree_overrides": 0,
    "commit_overrides": 0
  }
]
```

| Field | Type | Meaning |
|---|---|---|
| `name` | string | The `{repo}` segment for this repository. |
| `upstream` | string or null | The git URL the repository is mirrored from. |
| `head` | string or null | The full ref name that `HEAD` resolves to: the upstream's default branch as of the last sync. `null` if the upstream advertised none. |
| `changesets` | integer | Number of changesets the server holds. This counts every commit ever ingested, which can exceed the number of commits currently reachable from refs (for example after a force-push). |
| `last_sync` | integer or null | Unix time (seconds) of the last **successful** sync with upstream. `null` if there has never been one. **Every branch-relative answer (refs, `HEAD`, branch names in revisions) reflects upstream as of this moment.** A failed sync leaves it unchanged. |
| `last_sync_error` | string or null | The error message of the most recent sync attempt if that attempt failed. `null` if it succeeded or none has run. |
| `tree_overrides` | integer | Ingested commits whose tree the server could not derive from the changeset, because git accepted a non-canonical tree (for example a legacy `100664` mode). The server stores git's tree for them. Commit ids are never affected. Older servers omit it; read a missing value as 0. |
| `commit_overrides` | integer | Ingested commits whose object the server could not rebuild from the changeset fields (for example headers in a non-standard order). The server stores the original bytes. Older servers omit it; read a missing value as 0. |

New commits and new refs are picked up on demand when a request names them
([§4.0.7](#407-unknown-revisions-and-on-demand-sync)). A client that needs
existing refs to be fresher than `last_sync` MAY call
[`POST .../sync`](#414-sync).

### 4.2 Refs

`GET /api/v1/repos/{repo}/refs`. No parameters. Never pinned.

**Response** `200`: an array sorted by `name` in ascending byte order.

```json
[
  {"name":"refs/heads/feature","target":"3850c6e89333d70a9485afed4574a00736f46dd3","commit":"3850c6e89333d70a9485afed4574a00736f46dd3"},
  {"name":"refs/heads/main","target":"136c59eb0d199f2f9181ea4db8f448b4df4d3f32","commit":"136c59eb0d199f2f9181ea4db8f448b4df4d3f32"},
  {"name":"refs/tags/v1","target":"dd2f1ab550486b41240318325fc39d770208da10","commit":"136c59eb0d199f2f9181ea4db8f448b4df4d3f32"}
]
```

| Field | Meaning |
|---|---|
| `name` | Full ref name. Only `refs/heads/*` and `refs/tags/*` are mirrored. Forge-specific refs such as `refs/pull/*` are not. |
| `target` | What the ref points at: a commit, or an annotated tag object. This equals the `objectname` in `git for-each-ref`. |
| `commit` | The commit the ref ultimately names, after peeling tags. |

Refs whose peeled target is not a commit (for example a tag of a tree or a blob)
are omitted. Annotated tag objects are fetchable through the object protocol.
Refs are updated by sync, only after every commit they reach has been
ingested, and by the write API ([§14](#14-writes)), only after the git host
accepted the update.

### 4.3 Resolve

`GET /api/v1/repos/{repo}/resolve`

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `rev` | no | `HEAD` | A revision ([§5](#5-revision-syntax)). |

**Response** `200`: `{"oid":"136c59eb0d199f2f9181ea4db8f448b4df4d3f32"}`, the commit oid.

Errors ([§5.3](#53-applying-suffixes)): `404` for an unknown revision, an
unknown full commit id, a `~N` past the root, a nonexistent `^N` parent, or a
missing `HEAD`, each after at most one on-demand sync
([§4.0.7](#407-unknown-revisions-and-on-demand-sync)). `400` for invalid
suffix syntax or an ambiguous abbreviated id.

Pinned only when `rev` is itself a full lowercase oid. This is useful mainly to
check that the server knows a commit.

### 4.4 Commit

`GET /api/v1/repos/{repo}/commits/{rev}`

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `{rev}` (path) | yes | – | A revision. A ref name containing `/` MUST be percent-encoded as `%2F`, for example `refs%2Fheads%2Fmain`. |
| `changes` | no | `false` | Include the changeset's `changes`. |

**Response** `200`: one [CommitInfo](#406-the-commitinfo-object).

### 4.5 Log

`GET /api/v1/repos/{repo}/log`

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `rev` | no | `HEAD` | Tip revision. |
| `path` | no | none | Restrict to commits whose changeset touched this path ([§4.0.4](#404-path-parameters)). |
| `limit` | no | `50` | Maximum number of commits. Values above `10000` are silently clamped to `10000`. `0` returns an empty list. |
| `changes` | no | `false` | Include `changes` in each CommitInfo. |

**Response** `200`:

```json
{
  "commits": [
    {
      "oid": "136c59eb0d199f2f9181ea4db8f448b4df4d3f32",
      "tree": "ace4f9e27aee65aa15ce51848403e29cbd9a8e17",
      "parents": ["f5a007458f7ee8f53f440e457a55a2735f070a15"],
      "author": {"name":"A U Thor","email":"a@example.com","time":1700000000,"tz":"+0000"},
      "committer": {"name":"A U Thor","email":"a@example.com","time":1700000000,"tz":"+0000"},
      "message": "add zero and link\n",
      "generation": 4,
      "depth": 3
    },
    {
      "oid": "f5a007458f7ee8f53f440e457a55a2735f070a15",
      "tree": "c80c944fa611c4f4fd9a2ee2e62de8fbc0edc205",
      "parents": ["87f3f060a649bd0477cb755ac00616223cbc9f2c","3850c6e89333d70a9485afed4574a00736f46dd3"],
      "author": {"name":"A U Thor","email":"a@example.com","time":1700000000,"tz":"+0000"},
      "committer": {"name":"A U Thor","email":"a@example.com","time":1700000000,"tz":"+0000"},
      "message": "Merge feature\n",
      "generation": 3,
      "depth": 2
    }
  ]
}
```

**Semantics: first-parent history.**

* Without `path`, the result is the first-parent chain of `rev`, starting with
  `rev` itself, newest first, truncated to `limit`. Second and later parents of
  merges are never followed. This equals `git log --first-parent --format=%H <rev>`.
* With `path` (after normalisation, non-empty), the result is the commits on the
  first-parent chain of `rev` whose changeset ([§4.0.5](#405-the-change-object))
  changed `path` itself or any path below it (`path/...`), newest first,
  truncated to `limit`. A merge is included if it changed `path` **relative to its
  first parent**, which typically means a merge that brought the path's changes
  onto the chain. A root commit is included if it contains the path. Renames are
  not followed. This equals
  `git log --first-parent --format=%H <rev> -- <path>`.
* A path that never existed yields an empty list (not `404`).
* There is no pagination cursor. To continue, a client issues a new request with
  `rev=<oldest-returned-oid>~1`, or with `rev=<oldest-returned-oid>` and drops the
  first result.

### 4.6 Merge-base

`GET /api/v1/repos/{repo}/merge-base`

| Parameter | Required | Meaning |
|---|---|---|
| `a` | yes | Revision. |
| `b` | yes | Revision. |

**Response** `200`: `{"bases":["3850c6e89333d70a9485afed4574a00736f46dd3"]}`

**Semantics.** `bases` is the set of **all best common ancestors** of `a` and `b`
over the full commit graph (all parents, not first-parent only). It is sorted
in ascending oid order. It is `[a]` when `a == b`, and `[]` when the commits
share no history. Equals `git merge-base --all a b` as a set.

### 4.7 Is-ancestor

`GET /api/v1/repos/{repo}/is-ancestor`

| Parameter | Required | Meaning |
|---|---|---|
| `a` | yes | Candidate ancestor revision. |
| `b` | yes | Descendant revision. |

**Response** `200`: `{"result":true}`

**Semantics.** `true` iff `a` is reachable from `b` through any parents. A commit
is its own ancestor. Equals the exit status of `git merge-base --is-ancestor a b`.

### 4.8 Landed

`GET /api/v1/repos/{repo}/landed`

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `commit` | yes | – | Revision of the commit being asked about. |
| `ref` | no | `HEAD` | The integration branch. Despite its name, this accepts any revision. |

**Response** `200`: `{"landed":"f5a007458f7ee8f53f440e457a55a2735f070a15"}`, or `{"landed":null}`.

**Semantics.** Let *F* be the first-parent chain of `ref`. `landed` is the
**oldest** commit *X* in *F* such that `commit` is an ancestor of, or equal to,
*X*. In other words, it is the commit that brought `commit` onto the branch:
typically the merge commit for a feature-branch commit, or `commit` itself if it
was committed directly on the branch. It is `null` iff `commit` is not an
ancestor of `ref`. Oracle: the first *X* in
`git rev-list --first-parent --reverse <ref>` for which
`git merge-base --is-ancestor <commit> X` succeeds.

### 4.9 Bisect

`GET /api/v1/repos/{repo}/bisect`

| Parameter | Required | Meaning |
|---|---|---|
| `good` | yes | A revision known to be good. |
| `bad` | yes | A revision known to be bad. |

**Response** `200`, while the search continues:

```json
{"next":"87f3f060a649bd0477cb755ac00616223cbc9f2c","first_bad":null,"candidates":2,"steps_left":2}
```

and when it is finished:

```json
{"next":null,"first_bad":"136c59eb0d199f2f9181ea4db8f448b4df4d3f32","candidates":0,"steps_left":0}
```

| Field | Type | Meaning |
|---|---|---|
| `next` | string or null | The commit to test next. `null` when the search is finished. |
| `first_bad` | string or null | When finished, the first bad commit on `bad`'s first-parent chain. Otherwise `null`. |
| `candidates` | integer | Number of untested commits strictly between the lower bound and `bad`. |
| `steps_left` | integer | Upper bound on remaining test runs: the bit length of `candidates`, which is ⌈log₂(candidates+1)⌉. |

**Semantics: one step of first-parent bisection.** The server is stateless. Each
call computes one step from the given bounds.

1. `base :=` the **newest** commit on `bad`'s first-parent chain that is an
   ancestor of, or equal to, `good`. (In the reference implementation this is
   `newest_chain_ancestor(bad, good)` in `graph.rs`. Along the chain, "is an
   ancestor of `good`" holds for a contiguous run of the oldest commits, so the
   boundary can be found with O(log n) ancestry checks.)
   * If no such commit exists, the request fails with `400`
     (`good commit <g> shares no history with the first-parent history of <b>`).
   * If `base == bad` (`bad` is an ancestor of, or equal to, `good`), the
     request fails with `400` (`bad commit <b> is an ancestor of good commit <g>`).
2. `lo := depth(base)`, `hi := depth(bad)`, `candidates := hi - lo - 1`.
3. If `candidates == 0`, the search is finished: `first_bad := bad`.
4. Otherwise, `next` is the commit on `bad`'s first-parent chain at depth
   `lo + ⌊(hi - lo) / 2⌋`, and `steps_left := bitlen(candidates)`.

The search space is `bad`'s first-parent chain between `base` (exclusive) and
`bad` (inclusive). On an integration branch, it therefore finds the first bad
*landing*, usually a merge, rather than a commit inside a feature branch.

`good` need **not** be an ancestor of `bad`, and need not lie on `bad`'s
first-parent chain. As in `git bisect`, every ancestor of a good commit is
assumed good, so a good commit on a side branch vouches only for the history it
contains. In particular, `base` is an ancestor of `good`, never a merge that
brought `good` in together with other, untested changes. When `good` lies on
`bad`'s first-parent chain, `base == good`.

**Driving loop (informative, as in the reference CLI and the MCP tool
`wren_bisect_step`):** resolve `good` and `bad` to oids. Then repeat: call
`bisect`. If `next` is null, report `first_bad`. Otherwise test `next` (for
example in a checkout), and replace `good` (on pass) or `bad` (on fail) with
`next`. Because both drivers resolve `good` and `bad` first, every `bisect`
call they make uses full oids, so every call is a pinned query
([§7](#7-caching)) and is cacheable.

### 4.10 Diff

`GET /api/v1/repos/{repo}/diff`

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `head` | yes | – | Revision of the new side. |
| `base` | no | first parent of `head` | Revision of the old side. If omitted and `head` is a root commit, the request fails with `400` (`root commit has no parent; pass base`). |
| `path` | no | none | Keep only changes whose path equals the normalised `path` or lies below it. |
| `patch` | no | `false` | Also render a unified diff. |

**Response** `200`:

```json
{
  "base": "f5a007458f7ee8f53f440e457a55a2735f070a15",
  "head": "136c59eb0d199f2f9181ea4db8f448b4df4d3f32",
  "changes": [
    {"path":"src/a.txt","status":"M",
     "old":{"mode":"100644","oid":"4cb29ea38f70d7c61b2a3a25b02e3bdf44905402"},
     "new":{"mode":"100644","oid":"d7ed9e6a2d578accc8bcf915f28aff2a60475f9a"}},
    {"path":"src/link","status":"A","new":{"mode":"120000","oid":"8d14cbf983b3fad683171c9418998d9f68340823"}}
  ],
  "patch": "diff --git a/src/a.txt b/src/a.txt\n--- a/src/a.txt\n+++ b/src/a.txt\n@@ -1,3 +1,4 @@\n+zero\n one\n two\n three\ndiff --git a/src/link b/src/link\nnew file mode 120000\n--- /dev/null\n+++ b/src/link\n@@ -0,0 +1 @@\n+a.txt\n\\ No newline at end of file\n"
}
```

| Field | Meaning |
|---|---|
| `base`, `head` | The resolved commit oids actually compared. |
| `changes` | The tree-to-tree changes ([§4.0.5](#405-the-change-object)) from `base` to `head`, sorted by path, after the `path` filter. |
| `patch` | **Omitted** unless `patch=true`. See below. |

**Semantics.** A direct two-tree comparison, which is not a merge-base ("three
dot") diff. Equals `git diff --name-status --no-renames <base> <head>`.

**Patch text (informative format).** For each change, in order: a
`diff --git a/<p> b/<p>` line; then `new file mode <m>`, `deleted file mode <m>`,
or `old mode`/`new mode` lines when the mode changed. If either side contains a
NUL byte, the change gets the line `Binary files a/<p> and b/<p> differ`.
Otherwise it gets a unified diff with 3 lines of context and `--- a/<p>`/`+++ b/<p>`
headers (`/dev/null` for added or deleted files). Gitlinks are rendered as
empty content. Content is decoded as UTF-8 lossily. There are no `index` lines.
The patch is meant for humans and agents, and is **not** guaranteed to be
byte-identical to `git diff` (see [Open issues](#13-open-issues), item 16).

### 4.11 Blame

`GET /api/v1/repos/{repo}/blame`

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `rev` | no | `HEAD` | Revision. |
| `path` | yes in practice | `""` | File to blame. An empty or missing path is the root directory, so it yields `404`. |

**Response** `200`:

```json
{
  "rev": "136c59eb0d199f2f9181ea4db8f448b4df4d3f32",
  "path": "src/a.txt",
  "lines": [
    {"line":1,"commit":"136c59eb0d199f2f9181ea4db8f448b4df4d3f32","orig_line":1,"content":"zero"},
    {"line":2,"commit":"c79f1816ddba11b21306c0e7f2781b513bd4acda","orig_line":1,"content":"one"},
    {"line":3,"commit":"c79f1816ddba11b21306c0e7f2781b513bd4acda","orig_line":2,"content":"two"},
    {"line":4,"commit":"f5a007458f7ee8f53f440e457a55a2735f070a15","orig_line":3,"content":"three"}
  ],
  "commits": {
    "136c59eb0d199f2f9181ea4db8f448b4df4d3f32": {"author":"A U Thor","time":1700000000,"summary":"add zero and link"},
    "c79f1816ddba11b21306c0e7f2781b513bd4acda": {"author":"A U Thor","time":1700000000,"summary":"root commit"},
    "f5a007458f7ee8f53f440e457a55a2735f070a15": {"author":"A U Thor","time":1700000000,"summary":"Merge feature"}
  },
  "exact": true
}
```

| Field | Meaning |
|---|---|
| `rev` | The resolved commit. |
| `path` | The normalised path. |
| `lines[].line` | 1-based line number in the file at `rev`. |
| `lines[].commit` | The commit to which the line is attributed. |
| `lines[].orig_line` | 1-based line number of this line in the file as of `commit`. |
| `lines[].content` | The line's text, with all trailing `\n` and `\r` characters removed, decoded as UTF-8 lossily. |
| `commits` | One entry per distinct attributed commit, keyed by oid (keys sorted ascending): `author` is the author name, `time` is the author time (Unix seconds), and `summary` is the first line of the message. |
| `exact` | boolean. Always `true`. Earlier drafts let a server bound each line diff by time and answer `false` (with `Cache-Control: no-store`) when a diff was cut short. The field is kept so that existing clients keep working; clients SHOULD ignore it. |

Lines are split after each `\n`. A final line without a newline counts as a
line. An empty file has `"lines": []`.

**Semantics: first-parent blame.** History is the first-parent chain of `rev`.
The commits that changed `path` are exactly those that `log?rev=<rev>&path=<path>`
returns. Starting from the commit that created the file (or the oldest change
reachable without the file disappearing), each successive change is compared
with the previous version using git's default line diff: Myers with xdiff's
cost heuristic, followed by git's hunk sliding with the indent heuristic (the
reference server uses imara-diff's `Myers` with `postprocess_lines`). Where
several alignments are equally short, this choice decides which commit a line
is blamed on. Unchanged lines keep their attribution. Inserted or replaced
lines are attributed to the changing commit. Consequently:

* A change that arrived through a merge is attributed to the **merge commit**
  (in the example above, line 4 `three` was written on a feature branch, but is
  attributed to the merge `f5a00745`).
* Renames and copies are **not** followed. A file that was deleted and later
  re-created starts afresh, and so does a file that replaces a directory or a
  gitlink. A change of mode or of type (regular file and symlink) is compared
  like any other change.
* No whitespace or move detection is performed.

The result is meant to equal
`git blame --first-parent --porcelain <rev> -- <path>` in `(commit, orig_line)`
for every line, provided git does not detect renames (see [§8](#8-conformance)).
The diff is deterministic and has no time budget, so blame is a pure function
of the history and a pinned blame is as immutable as any other pinned query.

Errors: `404` (`no file "<path>" at <oid>`) if `path` does not exist at `rev`,
or is a directory or a gitlink. Symlinks are blamed as their target string.

### 4.12 Tree

`GET /api/v1/repos/{repo}/tree`

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `rev` | no | `HEAD` | Revision. |
| `path` | no | `""` (root) | Directory to list. |

**Response** `200`:

```json
{
  "oid": "ace4f9e27aee65aa15ce51848403e29cbd9a8e17",
  "entries": [
    {"name":"README.md","mode":"100644","kind":"blob","oid":"bceaad7eb74d81add1c32af46f82014e8659c5ea"},
    {"name":"src","mode":"040000","kind":"tree","oid":"c6f2031bac22d4f5863fd0bd543138de7b14d085"}
  ]
}
```

| Field | Meaning |
|---|---|
| `oid` | The tree id of the listed directory. |
| `entries` | Direct children in git tree order (as stored in the tree object). |
| `entries[].name` | Entry name. Non-UTF-8 bytes are replaced with U+FFFD. |
| `entries[].mode` | Six-digit **octal string**: `100644`, `100755`, `120000`, `040000`, `160000`. Legacy modes are normalised. |
| `entries[].kind` | `"blob"` (files and symlinks), `"tree"`, or `"commit"` (gitlink). |
| `entries[].oid` | Entry id. |

The listing is not recursive. Errors: `404` (`no directory "<path>" at <oid>`) if
`path` does not exist or is not a directory.

### 4.13 File

`GET /api/v1/repos/{repo}/file`

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `rev` | no | `HEAD` | Revision. |
| `path` | yes in practice | `""` | File path. |

**Response** `200`, `Content-Type: application/octet-stream`. The body is the raw
blob content: inflated, without the loose header, and not JSON. For a symlink,
the body is the link target.

Errors: `404` (JSON ErrorBody `no file "<path>" at <oid>`) if `path` is missing,
a directory, or a gitlink.

### 4.14 Sync

`POST /api/v1/repos/{repo}/sync`. No body, no parameters.

Asks the server to fetch from its upstream git URL now, ingest every new commit,
and update refs and `HEAD`. Blocks until done. Concurrent syncs of one
repository are serialised. Servers also sync periodically on their own (the
reference default is every 30 s, and `--poll 0` syncs only at startup and on
demand), and on demand when a request names an unknown revision
([§4.0.7](#407-unknown-revisions-and-on-demand-sync)). A client needs this
endpoint only to refresh refs that already exist. This endpoint only
refreshes from upstream. It accepts no data from the client and cannot create
commits or move refs to arbitrary values.

**Response** `200`:

```json
{"new_changesets":0,"refs_updated":0,"objects_imported":0,"millis":65}
```

| Field | Meaning |
|---|---|
| `new_changesets` | Commits ingested by this sync. |
| `refs_updated` | Refs created, moved, or deleted. |
| `objects_imported` | Objects added to the store. |
| `millis` | Wall-clock duration of the sync in milliseconds. |

The response carries `wren-synced-at` with the time of this sync, if it
succeeded ([§4.0.8](#408-freshness-the-wren-synced-at-header)).

Errors: `500` with code `internal` if the repository has no upstream
(`repo has no upstream`), or if the upstream git fetch fails, because neither
is classified as a client error ([§6.2](#62-status-codes),
[Open issues](#13-open-issues), item 23). A failed attempt is recorded in
`last_sync_error` ([§4.1](#41-list-repositories)). A cache forwards this
request upstream and never caches it.

### 4.15 Grep

`GET /api/v1/repos/{repo}/grep`

Searches file contents at a revision on the server, so a job or an agent
needs no checkout. Implemented in `crates/wren-core/src/grep.rs`.

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `pattern` | yes | – | A regular expression in the syntax of the Rust `regex` crate. |
| `rev` | no | `HEAD` | Revision. |
| `path` | no | `""` (root) | A file or directory to search ([§4.0.4](#404-path-parameters)). |
| `ignore_case` | no | `false` | Match case-insensitively. |
| `limit` | no | `100` | Maximum number of matches. Clamped to the range 1 to 10 000, so `0` means 1. |

**Response** `200`, here for `grep?pattern=^t&path=src/a.txt&rev=136c…`:

```json
{
  "rev": "136c59eb0d199f2f9181ea4db8f448b4df4d3f32",
  "matches": [
    {"path":"src/a.txt","line":3,"text":"two"},
    {"path":"src/a.txt","line":4,"text":"three"}
  ],
  "truncated": false
}
```

| Field | Meaning |
|---|---|
| `rev` | The resolved commit. |
| `matches[].path` | Full path of the file from the tree root. Non-UTF-8 bytes are replaced with U+FFFD. |
| `matches[].line` | 1-based line number. |
| `matches[].text` | The matching line, without its `\n` and without one trailing `\r`, decoded as UTF-8 lossily. A line longer than 400 bytes is cut at the last character boundary at or before 400 bytes and followed by `…` (U+2026). |
| `truncated` | `true` iff more matching lines exist than `limit`. |

**Semantics.**

* **Files searched.** If `path` names a directory (or is empty, the root),
  every file below it, recursively. If it names a file or symlink, that file
  alone (a symlink is searched as its target string). If it names a gitlink,
  nothing. Gitlinks below a directory are skipped.
* **Binary files** are skipped entirely: a file is binary if its first 8000
  bytes contain a NUL byte, the same test git uses.
* **Lines** are split on `\n`. A final `\n` ends the last line and does not
  start another, so a file that is empty or is exactly `\n` has no lines. The
  pattern is matched against each line's bytes without the `\n`, but with any
  trailing `\r`.
* **Order.** Files in git's path order (the order of `git ls-files`), lines in
  ascending order within a file. The first `limit` matches in this order are
  returned.
* The result is a pure function of the history, `pattern`, `path`,
  `ignore_case`, and `limit`. A request with a full commit id as `rev` is
  pinned ([§7.1](#71-pinned-queries)).

For patterns that mean the same as a POSIX extended regular expression and in
Rust `regex` syntax, the `(path, line)` pairs equal those of
`git grep -n -I -E [-i] <pattern> <rev> -- <path>`, cut to `limit`. Other
patterns may match differently: git uses basic regular expressions unless
given `-E`, and the Rust crate has its own syntax, with no backreferences or
look-around.

Errors: `400` with code `bad_request` (`invalid pattern: ...`) if `pattern`
does not compile. `400` (`text/plain`, [§4.0.2](#402-query-parameter-encoding))
if `pattern` is missing. `404` (`no path "<path>" at <oid>`) if `path` does not
exist at `rev`.

---

## 5. Revision syntax

Every revision parameter is resolved by the following algorithm (`resolve` in
`crates/wren-core/src/repo.rs`). The result is always a **commit** oid known to
the server. **A Wren revision always names a commit**: tags are peeled, and
there is no syntax for naming a tree, blob, or tag object.

Every `404` in this section is the answer after at most one on-demand sync
and a second resolution ([§4.0.7](#407-unknown-revisions-and-on-demand-sync)).
The algorithm itself only reads the server's current state.

### 5.1 Grammar

```
revision = base *suffix
base     = *( any character except "~" and "^" )      ; may be empty
suffix   = "~" *DIGIT
         / "^" *DIGIT
         / "^{commit}"
         / "^{}"
```

The base is everything before the first `~` or `^`. The remainder MUST consist
entirely of suffixes, which are applied left to right. A digit run is the
longest run of ASCII digits after the operator and is parsed as an unsigned
32-bit integer. At each position, `^{commit}` and `^{}` are recognised before
`^` followed by digits. Anything else in the suffix part is invalid. Examples:
`main~1x`, `main^{tree}`, `main^!`, `main~99999999999` (overflow).

### 5.2 Resolving the base

The first rule that matches wins:

1. **`HEAD` or empty.** The commit named by the repository's `HEAD` ref (the
   upstream default branch recorded at the last sync). If the repository has no
   `HEAD`, or the ref it names is gone, the request fails with `404`
   (`repo has no HEAD`, `HEAD ref <name> missing`). An empty base allows `~2` to
   mean `HEAD~2`.
2. **Full object id.** If the base is exactly 40 hexadecimal characters (either
   case), it names that commit and nothing else. If the repository has a
   changeset with that id, that commit is the result. Otherwise the request fails
   with `404` (`unknown revision <base>`). A full id is **never** looked up as a
   ref name, so a request pinned to full ids ([§7.1](#71-pinned-queries)) can
   never be redirected by a ref.
3. **Ref names**, in git's order (gitrevisions(7)):
   1. `<base>` exactly (for example `refs/heads/main`, `refs/tags/v1`);
   2. `refs/<base>` (for example `heads/main`, `tags/v1`);
   3. `refs/tags/<base>`;
   4. `refs/heads/<base>`.

   Only `refs/heads/*` and `refs/tags/*` exist on a Wren server ([§4.2](#42-refs)),
   so git's further rules (`refs/remotes/<base>`, `refs/remotes/<base>/HEAD`) can
   never match. A match resolves to the ref's **peeled commit**: an annotated tag
   yields the commit it points to, not the tag object. This is intended, because
   every query operates on commits.
4. **Abbreviated id.** If the base is 4 to 39 hexadecimal characters (either
   case), it is matched as a prefix against the ids of all changesets in the
   repository, including commits no longer reachable from any ref. This rule is
   tried only after the ref rules. Exactly one match resolves to that commit.
   Two or more matches fail with `400` (`ambiguous revision <base>`). Zero
   matches fall through. Only commits match: trees, blobs, and tag objects never
   do.
5. Otherwise the request fails with `404` (`unknown revision "<base>"`).

### 5.3 Applying suffixes

| Suffix | Meaning | Error |
|---|---|---|
| `~` | First parent (same as `~1`). | `404` if the commit is a root |
| `~N` | The N-th first-parent ancestor. `~0` is the commit itself. | `404` `<rev>: not enough history` if N exceeds the commit's `depth` |
| `^` | First parent (same as `^1`). | `404` if the commit is a root |
| `^0` | The commit itself. | |
| `^N` (N ≥ 1) | The N-th parent (1-based, any parent). | `404` `<rev>: no such parent` |
| `^{commit}`, `^{}` | No-op: the value is already a commit, so peeling leaves it unchanged. | |
| anything else | – | `400` `invalid revision syntax "<rev>"` |
| digit run that overflows u32 | – | `400` `invalid revision syntax "<rev>"` |

Suffixes chain, for example `main^2~3`, `main^1~2`, or `main~2^{}`.

**Evaluation order.** The base is resolved first, then suffixes are applied left
to right, and the first error wins. The status of a revision that is wrong in
several ways therefore depends on position. For example, `nope~x` is `404`
(unknown base), and `main~9x` is `404` if `main` has depth below 9, even though
`x` is invalid syntax ([Open issues](#13-open-issues), item 22).

### 5.4 What is not supported

The following git forms are **not** supported:

| Form | Result |
|---|---|
| `A..B`, `A...B` | `404` (the whole string is looked up as a ref name) |
| `^A` (negation), `A^!`, `A^@`, `A^-` | `400` invalid syntax (or `404` if an earlier step fails) |
| `^{tree}`, `^{blob}`, `^{tag}`, `^{/text}` | `400` invalid syntax |
| `@`, `@{N}`, `@{upstream}`, `@{-1}`, `branch@{date}` | `404` (looked up as a ref name); `400` if placed after a suffix, for example `main~1@{1}` |
| `rev:path`, `:path`, `:/message` | `404` (looked up as a ref name); `400` if placed after a suffix, for example `main~1:path` |
| `refs/remotes/*`, `FETCH_HEAD`, `ORIG_HEAD`, refs outside `refs/heads` and `refs/tags` | `404` |
| abbreviated ids shorter than 4 characters | `404` (unless a ref has that name) |
| ids of non-commit objects | `404` |

### 5.5 Differences from `git rev-parse`

| Case | git | Wren v1 |
|---|---|---|
| Annotated tag `v1` | tag object id | peeled commit id, i.e. `git rev-parse v1^{commit}` (intended) |
| `X^{}` on an annotated tag | peels the tag | no-op, because the base is already peeled |
| Abbreviated id universe | all objects | ingested commits only |
| Unknown full 40-hex id | git may print the id unchanged | `404` |
| Lookup order for refs and ids | full id, then refs, then abbreviated id | same |

Otherwise the supported subset agrees with `git rev-parse`. The differential
test suite verifies `main`, `main~1`, `main~3`, `main^`, `main^1~2`, `HEAD~2`,
`<abbrev>^2`, `main^{commit}`, `main~2^{}`, `HEAD~0`, and `main^0` against
`git rev-parse`. It also checks that `main~1x`, `main^{tree}`, `main~99999`, and an
unknown full id fail ([§8](#8-conformance)).

---

## 6. Errors

### 6.1 ErrorBody

Errors produced by Wren handlers and middleware have
`Content-Type: application/json` and the body

```json
{"error": "<human-readable message>", "code": "<code>"}
```

`code` is derived from the HTTP status alone (`ApiError::into_response` in
`crates/wren/src/server.rs`):

| Status | `code` |
|---|---|
| `404` | `not_found` |
| `400` | `bad_request` |
| `401` | `unauthorized` |
| `403` | `forbidden` |
| `409` | `conflict` |
| `413` | `too_large` |
| `502`, `504` | `upstream_unavailable` (no tier sends `504` today) |
| any other, in practice `500` | `internal` |

The message is diagnostic text. Clients MUST NOT parse it. They SHOULD branch
on `code` or on the status code. Clients MUST accept unknown codes, and MUST
treat a missing `code` (servers that predate the `error-codes` feature) like
the status code alone. Messages may contain oids, revision strings, and paths.

Not every error is an ErrorBody. Rejections produced by the HTTP framework
before a handler runs have other bodies and no `code`
([Open issues](#13-open-issues), item 5):

| Status | When | Body |
|---|---|---|
| `400` | Query-string parse failure ([§4.0.2](#402-query-parameter-encoding)) | `text/plain; charset=utf-8`, for example ``Failed to deserialize query string: missing field `b` `` |
| `400`, `415`, `422` | `POST /v1/objects` with malformed JSON (400), without `Content-Type: application/json` (415), or with an invalid oid or wrong shape (422) | `text/plain; charset=utf-8`, for example ``Expected request with `Content-Type: application/json` `` |
| `404` | Unknown route (after authentication) | empty, no `Content-Type` |
| `405` | Known path, wrong method | empty, with an `Allow` header |

These responses still carry `wren-protocol` and `wren-semantics`, and, under
`/api/v1/repos/{repo}/`, `wren-synced-at`
([§4.0.8](#408-freshness-the-wren-synced-at-header)). A client that fails to
parse an ErrorBody SHOULD fall back to the raw body as the message, as the
reference client does.

### 6.2 Status codes

The reference server classifies every handler error by its cause chain:

* it contains `wren_core::NotFound`: **404**;
* else it contains `wren_core::BadRequest`: **400**;
* else: **500**. Anything not attributable to the request is a server fault.

| Status | When | Body |
|---|---|---|
| `200` | Success. | Endpoint-specific. |
| `201 Created` | A write created a changeset ([§14.4](#144-create-a-changeset), [§14.7](#147-revert)). | Endpoint-specific. |
| `400 Bad Request` | The request is malformed or self-contradictory (`BadRequest`): invalid revision suffix syntax or digit overflow ([§5.3](#53-applying-suffixes)); ambiguous abbreviated id; malformed oid in `/v1/objects/{oid}`; `diff` on a root commit without `base`; `bisect` when `good` shares no history with `bad`'s first-parent chain, or when `bad` is an ancestor of `good`; a `grep` pattern that does not compile. | ErrorBody, `bad_request` |
| `400 Bad Request` | Query-string parse failure: missing required parameter, non-numeric `limit`, boolean other than `true`/`false`, duplicate parameter. | `text/plain` ([§6.1](#61-errorbody)) |
| `401 Unauthorized` | Missing or invalid bearer token ([§2.1](#21-bearer-tokens), [§14.1](#141-authentication)). | ErrorBody, `unauthorized` |
| `403 Forbidden` | A write to a server without a write token, or with the read token ([§14.1](#141-authentication)); any write sent to a cache ([§14.9](#149-caches)). | ErrorBody, `forbidden` |
| `404 Not Found` | The thing named does not exist (`NotFound`): unknown repository; unknown revision or full id, after at most one on-demand sync ([§4.0.7](#407-unknown-revisions-and-on-demand-sync)); `~N` past the root; nonexistent `^N` parent; repository without `HEAD`, or `HEAD` naming a missing ref; unknown changeset; an object the query needs is not in the store; `path` absent or of the wrong kind for `blame`/`tree`/`file`/`grep`; `GET /v1/objects/{oid}` for an absent object. | ErrorBody, `not_found` |
| `404 Not Found` | Unknown route (after authentication). | empty |
| `405 Method Not Allowed` | Known path, wrong method. Carries an `Allow` header. | empty |
| `409 Conflict` | A land with conflicts or refused by the git host, a stale ref update ([§14.5](#145-land), [§14.6](#146-update-a-ref)). | The Landed or RefUpdate object |
| `409 Conflict` | A land that gave up because the branch kept moving ([§14.5](#145-land)). | ErrorBody, `conflict` |
| `413 Payload Too Large` | `POST /v1/objects` with more than 10 000 oids (server and cache); a write body over 64 MiB ([§14.2](#142-common-rules)). | ErrorBody, `too_large` |
| `415`, `422`, `400` | `POST /v1/objects` with the wrong `Content-Type` (415), a syntactically valid body with an invalid oid or shape (422), or malformed JSON (400). | `text/plain` |
| `500 Internal Server Error` | Every other handler error: I/O and database failures; corrupt stored data; an object too large to frame ([§3.2](#32-batch-fetch-post-v1objects)); `sync` without an upstream, or a failed upstream fetch ([§4.14](#414-sync)); a panicked worker task (`task failed: ...`). | ErrorBody, `internal` |
| `502 Bad Gateway` | Caches only ([§6.3](#63-errors-at-caches)). | ErrorBody, `upstream_unavailable` |

A `404` from a revision-taking endpoint means the server does not know the
revision even after syncing on demand
([§4.0.7](#407-unknown-revisions-and-on-demand-sync)). Retrying at once does
not help: on-demand syncs of one repository are at least 1 s apart, and a
commit that upstream does not have will not appear. A branch that exists but
has moved upstream does not cause a `404`; it answers with its position as of
`last_sync`, which `wren-synced-at` reports
([§4.0.8](#408-freshness-the-wren-synced-at-header)). A client that needs the
new position calls `sync` first. A `500` is a server fault, and a client MAY
retry it. A `400` is not retryable unchanged.

### 6.3 Errors at caches

A cache relays upstream query responses (status, `Content-Type`, body, and the
headers of [§2.2](#22-tiers)) unchanged, including upstream `4xx` and `5xx`
answers. The reference cache forwards every `GET` and `HEAD` request under
`/api/`, whatever its path, and `POST .../sync`, so framework rejections on query routes (`text/plain`
`400`, empty `404` and `405`) come from upstream and are relayed. Errors that
originate in the cache itself use the same ErrorBody shape:

| Status | `code` | When (reference cache) |
|---|---|---|
| `502 Bad Gateway` | `upstream_unavailable` | Object path (`POST /v1/objects` with at least one local miss, or `GET /v1/objects/{oid}` on a local miss): the upstream is unreachable or times out, answers non-2xx (`upstream: upstream returned <status>`), returns a malformed frame stream, or returns an object that fails hash verification ([§3.4](#34-verification)). Body: `{"error":"upstream: ...","code":"upstream_unavailable"}`. |
| `502 Bad Gateway` | `upstream_unavailable` | Query path (`/api/...`): the upstream is unreachable or times out (`upstream unreachable: ...`). |
| `403` | `forbidden` | Any other request under `/api/`, i.e. a write: writes go directly to the server ([§14.9](#149-caches)). |
| `413` | `too_large` | Batch over 10 000 oids, checked before contacting upstream. |
| `404` | `not_found` | `GET /v1/objects/{oid}` that is neither stored nor returned by upstream. |
| `400` | `bad_request` | Malformed oid in `GET /v1/objects/{oid}`. |
| `500` | `internal` | Local faults, for example a store I/O error, or a failure reading a request or upstream response body. |

---

## 7. Caching

### 7.1 Pinned queries

A query is **pinned** when every revision in it is a full commit id. Such an
answer depends only on immutable inputs (commits and everything they reach) and
can never change. Precisely, a request is pinned iff **all** of the following hold
(this is `is_pinned` in `crates/wren/src/server.rs`):

1. The method is `GET`.
2. The path has the form `/api/v1/repos/{repo}/{endpoint}[/...]`, where
   `{endpoint}` is one of: `commits`, `log`, `merge-base`, `is-ancestor`,
   `landed`, `diff`, `blame`, `tree`, `file`, `bisect`, `resolve`, `grep`. (`repos`,
   `refs`, `sync`, and anything outside `/api/v1/repos/` are never pinned.)
3. Build a list *R* of revision strings:
   * for `commits`, the raw path segment after `commits/`;
   * plus the value of **every** query parameter whose name is one of `rev`, `a`,
     `b`, `base`, `head`, `commit`, `ref`, `good`, `bad`, regardless of whether
     the endpoint uses it, and including repeated occurrences. Values are taken
     from the raw query string, **without percent-decoding**. A parameter with no
     `=` contributes an empty string.
4. *R* contains at least *n* entries, where *n* = 2 for `landed`, `merge-base`,
   `is-ancestor`, and `bisect`, and *n* = 1 for all other endpoints. This is
   because endpoints whose revision defaults to `HEAD` must name it explicitly.
5. Every entry of *R* is exactly 40 characters from `[0-9a-f]`. Lowercase only.

Examples:

| Request | Pinned? | Why |
|---|---|---|
| `GET .../blame?rev=136c…3f32&path=src/a.txt` | yes | |
| `GET .../diff?head=136c…3f32` | yes | `base` defaults to the first parent of a fixed commit, which is itself fixed |
| `GET .../log?rev=136c…3f32&path=src&limit=10` | yes | |
| `GET .../landed?commit=3850…6dd3&ref=136c…3f32` | yes | |
| `GET .../grep?pattern=TODO&rev=136c…3f32` | yes | `pattern` is not a revision parameter |
| `GET .../landed?commit=3850…6dd3` | no | `ref` defaults to `HEAD` (only 1 revision) |
| `GET .../blame?path=src/a.txt` | no | `rev` defaults to `HEAD` |
| `GET .../commits/main` | no | branch name |
| `GET .../resolve?rev=136C…3F32` | no | uppercase (it still names that commit, [§5.2](#52-resolving-the-base), but is conservatively not pinned) |
| `GET .../blame?rev=136c…3f32&a=main&path=x` | no | `a` is a revision parameter, even though blame ignores it |
| `GET .../refs`, `GET /api/v1/repos` | no | mutable by nature |

### 7.2 Marking and storing

* **Server.** When a pinned request succeeds (2xx) and the handler has not set a
  `Cache-Control` header itself, the server MUST add an immutable
  `Cache-Control` header. The reference server (`fn immutable` in
  `crates/wren/src/server.rs`) sends
  `Cache-Control: private, max-age=31536000, immutable` when it has a token
  ([§2.1](#21-bearer-tokens)), and
  `Cache-Control: public, max-age=31536000, immutable` otherwise. It MUST NOT
  add the header to non-pinned or unsuccessful responses. It MUST NOT override
  a `Cache-Control` header the handler set, so a handler whose answer is not a
  pure function of the history can opt out with `no-store` (no query handler of
  the reference server needs to). `GET /v1/objects/{oid}` with `200` always
  carries the same immutable header ([§3.5](#35-immutability)).
* **Any cache** that the marking admits MAY store a response marked
  `immutable` **forever**, and MUST NOT revalidate it. Under HTTP caching rules
  (RFC 9111), a `private` response may be kept by the client's own cache but
  not by a shared HTTP cache or CDN. A Wren cache tier authenticates its own
  clients, and stores immutable responses whether they are `public` or
  `private`.
* **Wren cache tier (reference behaviour, `crates/wren/src/cache.rs`).**
  * *Semantics.* The cache keeps the semantics version
    ([§7.3](#73-the-immutability-contract)) it believes its upstream serves. At
    startup it reads `semantics` from the upstream's `/v1/capabilities` (best
    effort). After that, it adopts the `wren-semantics` header of every
    forwarded query response. Until it learns a value, it uses its own built-in
    value.
  * *Key.* A stored answer is keyed by the pair (semantics version, exact
    `path?query` string). The string is not normalised: parameter order and
    encoding matter.
  * *Hit.* For a `GET` under `/api/`, it looks up the key with the current
    semantics version. On a hit, it returns `200` with the stored body, the
    stored `Content-Type`, an immutable `Cache-Control` (`private` if the cache
    has a client token, otherwise `public`), and `wren-semantics` set to the
    key's version, without contacting upstream.
  * *Miss.* It forwards the request. If upstream answers `200` to a `GET` with
    a `Cache-Control` value containing `immutable`, it stores the body and
    `Content-Type` under the semantics version named by that response's
    `wren-semantics` header (or the current one if the header is absent), then
    relays the response. Every other request (non-GET, not marked immutable,
    non-200) is forwarded and relayed but never stored. When relaying, it
    passes `Cache-Control`, `wren-semantics`, and `wren-synced-at` through
    unchanged.
  * *Objects.* Objects found upstream are verified ([§3.4](#34-verification))
    and stored forever. "Missing" answers are never stored.
* A cache MUST NOT derive "pinned" itself from the URL. It relies on the
  upstream's `immutable` marking and `wren-semantics` header, so a cache needs
  no knowledge of query semantics.

### 7.3 The immutability contract

A pinned answer is a pure function of two things: immutable inputs (the
commits named and everything they reach), and the server's **query semantics
version** (`QUERY_SEMANTICS`, sent as `wren-semantics`,
[§1.2](#12-protocol-version-and-the-wren-protocol-header)).

* For a given semantics version, a server MUST return the same body for the
  same pinned request, forever.
* A server whose answer to any already-answerable pinned query changes MUST
  send a new semantics version. Examples are a bug fix in blame attribution, a
  different line-diff heuristic, or a new field in a pinned answer. It does not
  need a new protocol major version or a new route.
* Wren cache tiers key stored answers by semantics version
  ([§7.2](#72-marking-and-storing)). After an upgrade, they stop serving
  answers stored under the old version and fill again from upstream. Old and
  new answers never mix.

The reference implementation relies on these properties:

* changesets are append-only and never deleted;
* derived data is a pure function of immutable inputs;
* a full 40-hex revision always names that commit and is never looked up as a
  ref ([§5.2](#52-resolving-the-base));
* every computation behind a query answer is deterministic, with no time
  budget (in particular blame's line diff, [§4.11](#411-blame)).

Limits *(informative)*. Entries stored under an old version are not deleted;
they only become unreachable. HTTP caches outside Wren key by URL and do not
see the semantics version, so with `public` marking they may keep serving an
old answer until it expires (`max-age` is one year). A tier with a token marks
answers `private`, which keeps them out of shared HTTP caches. See
[Open issues](#13-open-issues), item 28.

---

## 8. Conformance

### 8.1 Git is the oracle

For every query, the correct answer is **git's answer** to the corresponding
first-parent git command on the same repository. An implementation conforms to
the query API when, for every input, it returns the same result as git under
the mappings below. The reference test suite is
**`crates/wren-core/tests/differential.rs`**. It generates random histories with
the git CLI (branches, `--no-ff` merges, deletions, mode changes, large edits,
annotated tags, then force-pushes, branch deletion, and tag deletion), ingests
them, and compares every answer against git. It runs a configurable list of
seeds (`WREN_FUZZ_SEEDS`).

| Endpoint | git oracle | Compared in `differential.rs` |
|---|---|---|
| `refs` (`name`, `target`) | `git for-each-ref --format='%(refname) %(objectname)' refs/heads refs/tags` | yes |
| `is-ancestor` | `git merge-base --is-ancestor A B` | yes (random pairs) |
| `merge-base` | `git merge-base --all A B`, sorted | yes (random pairs) |
| `resolve` | `git rev-parse REV` (for the subset of §5, with tags peeled) | partially. Equal to git: `main`, `main~N`, `main^`, `main^1~2`, `HEAD~2`, `<abbrev>^2`, `main^{commit}`, `main~2^{}`, `HEAD~0`, `main^0`. Must fail: `main~1x`, `main^{tree}`, `main~99999`, unknown full id. The test checks only that these fail, not the status code. |
| `log` | `git log --first-parent --format=%H REV` | yes |
| `log?path=` | `git log --first-parent --format=%H REV -- PATH` (files, directories, nonexistent paths, `limit`) | yes |
| `blame` | `git blame --first-parent --porcelain REV -- PATH`, comparing `(commit, orig_line)` per line | yes (every file at two revisions) |
| `diff` (`changes`) | `git diff --name-status --no-renames BASE HEAD`, sorted by path | yes (status letter and path) |
| `commits?changes=true` | `git diff-tree -r --no-renames --name-status --root -m --first-parent C` | yes (status letter and path) |
| `landed` | first `X` in `git rev-list --first-parent --reverse REF` with `git merge-base --is-ancestor C X` | yes |
| `depth` | `git rev-list --first-parent --count C` − 1 | yes (random commits) |
| `generation` | commit-graph topological level | **no** |
| `bisect` | Convergence. With "bad" defined as "contains commit X" and a random `good` that does not contain X (often off main's first-parent line), the driving loop of §4.9 must end at the oldest commit on main's first-parent chain that contains X. That expectation is computed with `git rev-list --first-parent` and `git rev-list --ancestry-path X..main`. | yes (end result only, not each step's `next`, `candidates`, or `steps_left`) |
| `tree`, `file` | `git ls-tree REV PATH/`, `git cat-file blob REV:PATH` | **no** |
| `diff?patch=true` | no byte-exact oracle ([§4.10](#410-diff)) | **no** |
| `grep` | `git grep -n -E [-i] PATTERN REV [-- PATH]`, comparing `(path, line)` per match, for patterns that mean the same in both dialects ([§4.15](#415-grep)) | yes (whole tree and one directory, at two revisions; also that `limit` keeps a prefix and sets `truncated` correctly). `text` is not compared. |

The differential suite exercises the query engine (`wren-core`) directly, not
over HTTP. HTTP-level behaviour (authentication, `wren-protocol` and
`wren-semantics` on capabilities of both tiers, the `not_found` error code,
read-your-writes through on-demand sync, cache hits for pinned blame, no
caching of branch-relative queries, grep through the CLI, end-to-end bisect,
MCP tools pinning revisions, sparse checkout through a cache) is covered by
the end-to-end test `crates/wren/tests/e2e.rs`.

### 8.2 Conformance classes

* **Conforming server.** It MUST implement §1.2 (both headers), §1.3, §2.1,
  §3.2 to §3.5, every endpoint of §4 with the semantics stated there, §4.0.8,
  §5, the status mapping of §6.2 (404 for nonexistent things, 400 for
  malformed requests, 500 for server faults, 401 for authentication failures,
  ErrorBody with `code` for handler errors), the marking rule of §7.2, and the
  semantics rule of §7.3. On-demand sync (§4.0.7) is OPTIONAL and signalled by
  the `on-demand-sync` feature. Its query answers MUST agree with the git
  oracles of §8.1.
* **Conforming cache.** It MUST implement the same routes (§2.2), MUST relay
  responses unaltered (including `Cache-Control`, `wren-semantics`, and
  `wren-synced-at`), MUST NOT serve stored query responses that upstream did
  not mark immutable, MUST key stored query responses by semantics version
  (§7.2), SHOULD verify objects before storing them, and reports upstream
  failures as `502` (§6.3).
* **Conforming client.** It MUST verify object hashes (§3.4), MUST handle
  `missing` frames, MUST NOT send more than 10 000 oids per batch, MUST ignore
  unknown JSON fields and feature flags, MUST accept unknown error codes, and
  SHOULD check
  `wren-protocol` / `protocol`.

A third-party implementation can reuse `differential.rs` as a template: replace
the in-process `Repo` calls with HTTP calls to the implementation under test.

---

## 9. Relationship to git (non-normative)

*This section is non-normative. It sketches how the protocol could be hosted by
existing infrastructure. Nothing here is implemented.*

**Identity.** Wren uses git's object ids, object encoding, and ref names
unchanged. Every oid returned by the API is valid in any git client, and the
reverse is also true. A Wren client store uses git's loose-object layout, so it
can be attached to a git repository through `objects/info/alternates`.

**git protocol v2.** The object protocol is close to a protocol-v2 `fetch` that
sends `want` lines and never sends `have` lines. The differences are that the
response is a sequence of independently verifiable loose objects rather than a
packfile, and that no object closure is computed: the client asks for exactly
the objects it names. A server could expose this as a new protocol-v2 command
(for example `object-get`, advertised in the capability advertisement) next to
the existing `object-info`. The query endpoints could similarly become v2
commands (`merge-base`, `is-ancestor`, `log-first-parent`, `blame-first-parent`,
`landed`, `bisect-step`), each taking oids and returning oids. That would let
any git server that implements them act as a Wren server without a separate HTTP
API. [A draft proposal](../upstream/git-protocol-v2-query.md), not yet sent to
the git list, works this out in more detail.

**Forge REST APIs.** A forge could serve the `/api/v1/repos/{repo}/...` endpoints
directly, with `{repo}` mapped to `owner%2Fname` or a project id. Several
endpoints overlap with what forges already expose. For example, GitHub's
"compare two commits" returns a merge-base commit, and GitLab has a repository
`merge_base` endpoint. The distinguishing requirements a forge would need to
meet are: first-parent semantics for log, blame, and changesets; the
`landed` and `bisect` operations; `Cache-Control: immutable` on pinned queries,
together with a semantics version (§7.3); and answers that match git exactly
(§8). Forge-specific refs (`refs/pull/*`)
are outside v1's ref model. A forge would likely also want per-repository
object authorization, which v1 does not provide (§13, item 10).

---

## 10. Known gaps in v1

* **No ranges.** `A..B`, `A...B`, and negation are not supported (§5.4). Clients
  combine `log`, `merge-base`, and `is-ancestor` instead.
* **SHA-1 only.** `object_format` is always `"sha1"`. The frame layout fixes
  20-byte oids (§1.5).
* **Writes only through the git host's rules.** The write API (§14) creates
  changesets and moves branches on the server, but while a git host is the
  source of truth every branch move is pushed to the host first (§14.8).
  Ordinary git pushes to the host keep working and reach Wren through sync:
  periodic, on demand when a request names an unknown revision (§4.0.7), or
  explicit (§4.14). Checkouts created with `--git` are configured for this.
  Tags cannot be written, and there is no server-side merge of two branches
  (only rebase).
* **No rename or copy following.** Rename and copy detection is absent from
  `log?path=`, `blame`, `diff`, and changesets. A rename is a delete plus an add.
* **First-parent history only** for `log`, `blame`, changesets, `landed`, and
  `bisect`. There is no full-graph log.
* **Only `refs/heads/*` and `refs/tags/*`.**
* **No pagination cursor** for `log`. `limit` is capped at 10 000 (§4.5).
* **No submodule content.** Gitlinks are reported but never followed.
* **No objects of 4 GiB or more.** The 32-bit frame length cannot carry them
  (§3.2, §13 item 24).
* **Lossy text.** Non-UTF-8 paths, messages, blame content, and grep lines are
  converted lossily in JSON (§13, item 15).
* **grep is not git grep.** Patterns use Rust `regex` syntax, not POSIX regular
  expressions. There are no `git grep` options beyond `ignore_case`, and binary
  files are skipped rather than reported (§4.15).

---

## 11. Security considerations

* **Integrity** comes from content addressing. Clients verify every object
  (§3.4), so tiers need not be trusted for integrity. Query responses are **not**
  self-verifying: a client trusts its tier for query answers.
* **Writes** need the write token (§14.1), which is as server-wide as the
  read token: it can write to every repository. Blobs uploaded through one
  repository land in the shared object store and are readable through every
  repository's object route. A cache never relays writes, so a cache's
  upstream token, even a write token, cannot be used to write through it
  (§14.9). Bodies are capped at 64 MiB; there is no separate limit on the
  number of changes or the size of one file.
* **Authorization** is a single bearer token per tier, valid for all
  repositories and all objects. The object namespace is server-wide (§3.2). A
  deployment that needs per-repository access control must not put repositories
  of different audiences behind one server in v1.
* **Transport.** Tokens are sent in clear on plain HTTP. Deployments outside
  loopback SHOULD use TLS. The reference server logs a warning when it listens
  on a non-loopback address without a token.
* **Caching and tokens.** A tier with a token marks immutable answers
  `private` (§7.2), so shared HTTP caches and CDNs do not keep them. A Wren
  cache stores them anyway. A cache without a client token, in front of a
  server with a token, exposes everything the server serves to anyone who can
  reach the cache, and marks its stored answers `public`. Such a cache needs
  its own token.
* **Denial of service.** `POST .../sync` triggers an upstream fetch and is
  available to any authenticated client. So does any query naming an unknown
  revision (§4.0.7), at most once per second per repository. Batch requests are
  bounded at 10 000 oids, `log` at 10 000 commits, and `grep` at 10 000
  matches. Blame and grep have no time bound: a blame walks the file's whole
  first-parent history, and a grep reads every file under `path` until it has
  `limit` matches.

---

## 12. Worked example (informative)

The examples in this document come from the following five-commit repository,
served by the reference server as `demo`:

```
* 136c59eb  add zero and link        (main, tag v1)   generation 4, depth 3
*   f5a00745  Merge feature                            generation 3, depth 2
|\
| * 3850c6e8  feature: add three     (feature)         generation 2, depth 1
* | 87f3f060  main: readme                             generation 2, depth 1
|/
* c79f1816  root commit                                generation 1, depth 0
```

| Request | Answer |
|---|---|
| `resolve?rev=main~1x` | `400` `invalid revision syntax "main~1x"` |
| `resolve?rev=main~2^{}` | `{"oid":"87f3f060…"}` |
| `resolve?rev=main~9` | `404` `main~9: not enough history` (main has depth 3) |
| `resolve?rev=tags/v1` | `{"oid":"136c59eb…"}`: `refs/tags/v1`, peeled from the tag object `dd2f1ab5…` |
| `merge-base?a=main&b=feature` | `{"bases":["3850c6e8…"]}` |
| `is-ancestor?a=feature&b=main` | `{"result":true}` |
| `landed?commit=feature` | `{"landed":"f5a00745…"}`: the merge landed it |
| `landed?commit=main&ref=feature` | `{"landed":null}` |
| `log?rev=136c…&path=src/a.txt` | `136c59eb`, `f5a00745`, `c79f1816`: the merge is included because it changed `src/a.txt` relative to its first parent |
| `bisect?good=c79f…&bad=136c…` | `next = 87f3f060` (depth 1 = 0 + ⌊3/2⌋), `candidates = 2`, `steps_left = 2` |
| `bisect?good=3850…&bad=136c…` | `next = 87f3f060`, `candidates = 2`. `good` (on the feature branch) only vouches for its own ancestors, so the lower bound is `c79f1816`, the newest main commit it contains, and not the merge `f5a00745` |
| `bisect?good=136c…&bad=3850…` | `400`: `bad` is an ancestor of `good` |
| `grep?pattern=^t&path=src/a.txt&rev=136c…` | lines 3 `two` and 4 `three` of `src/a.txt`, `truncated: false` ([§4.15](#415-grep)) |

---

## 13. Open issues

These are places where the reference implementation is ambiguous, inconsistent,
or diverges from git or from this document's intent. The behaviour described in
the body of this document is the **current** behaviour. Item numbers are stable
across drafts: resolved items move to §13.1 and their numbers are not reused.

### 13.1 Resolved

| # | Issue | Resolution in the current code |
|---|---|---|
| 1 | Revision suffix parsing accepted garbage (`main~1x` meant `main~1^1`, and `main^{commit}` walked nine parents). | Strict grammar: only `~N`, `^N`, `^{commit}`, `^{}` are accepted after the base. Anything else, or a digit overflow, is `400`. `~N` past the root and a nonexistent `^N` are `404` (§5.1, §5.3). |
| 2 | A ref named like a 40-hex id could shadow the commit, although `is_pinned` treated the id as immutable. | A full 40-hex revision always names that commit: `404` if it is not a known changeset, and it is never looked up as a ref (§5.2 rule 2). |
| 3 | Ref disambiguation order differed from git. | Now `<name>`, `refs/<name>`, `refs/tags/<name>`, `refs/heads/<name>`, and abbreviated ids only after refs, as in git (§5.2). |
| 4 | Annotated tags resolve to the peeled commit, unlike `git rev-parse <tag>`. | Ratified as intended: a Wren revision always names a commit (§5, §5.5). |
| 6 | Cache and server gave different status codes and bodies. | `413` for oversized batches on both tiers; a JSON ErrorBody `404` for a missing single object on both; `502` for every upstream failure at a cache (§3.2, §3.3, §6.3). |
| 7 | Every error that was neither NotFound nor I/O became `400`. | NotFound → `404`, `wren_core::BadRequest` → `400`, everything else → `500` (§6.2). See item 23 for remaining classification questions. |
| 8 | The cache stored upstream objects without verifying them. | The cache verifies every object on the miss path (`put_raw_verified`) before storing it. A failure is a `502` (§3.4, §6.3). |
| 9 | An inexact (time-budgeted) blame was still marked `immutable`. | First fixed with `BlameResponse.exact` and `Cache-Control: no-store` on inexact answers. Then superseded: blame's line diff is now deterministic with no time budget, so every blame is a pure function of the history and `exact` is always `true` (§4.11). The pinned middleware still never overrides an existing `Cache-Control` (§7.2). |
| 11 | `encode_frame` silently truncated lengths of 4 GiB or more. | `encode_frame` returns an error instead. The request fails (§3.2). See item 24. |
| 12 | `mode` was a decimal integer in changes but an octal string in tree listings. | Both are six-digit octal strings such as `"100644"` (§4.0.5). |
| 14 | Bisect assumed `landed(good, bad)` was good, which is wrong when `good` is off the first-parent chain. | The lower bound is the newest commit on `bad`'s first-parent chain that is an ancestor of `good` (`newest_chain_ancestor`). `good` need not be an ancestor of `bad`. The error cases are `400` (§4.9). |
| 13 (part) | Bisect, `depth`, and revision edge cases were untested. | `differential.rs` now checks `depth` against `git rev-list --first-parent --count`, the peeling and strictness edge cases of §5, and bisect convergence against a git-computed expectation (§8.1). The remainder is still open as item 13. |
| 21 | The cache relayed `Cache-Control` only when it contained `immutable`, and dropped other values such as `no-store`. | The cache relays the upstream `Cache-Control` value unchanged, together with `wren-semantics` and `wren-synced-at` (§2.2, §7.2). |
| 26 | Pinned answers changed between drafts (#9, #12, #14, blame's switch to git's line diff), which per §7.3 would have needed a new protocol major version, and caches had to be wiped by hand. | Answers now carry a query semantics version (`wren-semantics`, `QUERY_SEMANTICS`). A server that changes answers bumps it instead of the protocol version, and Wren caches key stored answers by it (§7.3). What remains for HTTP caches outside Wren is item 28. |

### 13.2 Still open

* **#5. Not every error uses ErrorBody.** Framework-generated errors return
  `text/plain` or empty bodies, and carry no `code`: query-parameter parse
  failures (`400`), JSON body rejections on `POST /v1/objects`
  (`400`/`415`/`422`), unknown routes (`404`, empty), and wrong methods
  (`405`, empty). See §6.1. Clients that parse `{"error":…}` must fall back to
  the raw body and the status code, which the reference client does.
* **#10. No per-repository object scoping.** `/v1/objects` serves any object in the
  shared store, and tokens are server-wide. This is fine for one trust domain,
  but blocks forge adoption, where repositories have different readers. Fixing it
  needs a repo-scoped object route (for example `/v2/repos/{repo}/objects`) and
  therefore a major version.
* **#13. Remaining conformance gaps.** These are still not compared against git:
  `generation`, `tree`, `file`, CommitInfo field values (signatures, message),
  the patch text, grep's `text` field, bisect's per-step fields (`next`,
  `candidates`, `steps_left`), and the exact status codes of failing
  revisions. `differential.rs` still calls `wren-core` in-process, not the
  HTTP layer, so the caching rules, the semantics key, the 400/404/500/502
  mapping, error codes, `wren-synced-at`, on-demand sync, and cache behaviour
  are covered only by smoke tests in `e2e.rs`, if at all.
* **#15. Lossy text conversion.** Paths, commit messages, names, blame lines,
  and grep lines are converted to UTF-8 lossily. Non-UTF-8 file names cannot be
  addressed through `path`. Unparsable signature times become `0`. The commit
  `encoding` header is ignored. Blame strips *all* trailing `\r`/`\n`
  characters from each line; grep strips one `\r`.
* **#16. Patch output is not git-compatible.** It has no `index` lines, uses its own
  binary detection, decodes lossily, and may differ for mode-only changes. Either
  specify the format exactly or document it as presentation-only (as §4.10 does
  now).
* **#17. SHA-256 has no wire definition.** `object_format` can signal it, but the
  frame layout and `Oid` type are fixed at 20 bytes. It is undecided whether a
  SHA-256 deployment would be v1 with `object_format: "sha256"` and 32-byte
  frame oids, or necessarily v2.
* **#18. `/v1/stats` exists only on caches, and caches advertise static
  capabilities.** `/v1/stats` is a deliberate exception to "identical RPCs",
  but it is not advertised in `features`. A cache now learns `semantics` from
  its upstream (§7.2), but it still advertises its own static `features` and
  `protocol` without checking the upstream's, so a v1 cache in front of a
  future v2 server would advertise features it cannot relay.
* **#19. `is_pinned` works on raw strings.** It does not percent-decode, accepts only
  lowercase (uppercase full ids now name the same commit, but are not pinned),
  and counts revision-named parameters even on endpoints that ignore them. All of
  these are conservative: they lose cacheability but never cause wrong caching.
  Separately, the cache key is the semantics version plus the raw
  `path?query`, so equivalent requests with reordered parameters are cached
  separately.
* **#20. `log` limit clamping is silent.** `limit > 10000` is silently reduced, and
  there is no indication that the result was truncated. A cursor or a `truncated`
  flag (as `grep` has) would be a non-breaking addition.

Added in the second draft:

* **#22. Revision error status depends on evaluation order.** The base is resolved,
  and suffixes are applied, before the rest of the string is syntax-checked. So
  `nope~x` and `main~9x` (depth < 9) are `404`, while `main~1x` is `400`. A
  whole-string syntax check before lookup would make malformed revisions
  consistently `400`. Because `404` now triggers an on-demand sync (§4.0.7),
  such malformed revisions can also cause an upstream fetch.
* **#23. Some errors may be misclassified as `500`.** Under the current mapping, any
  error not explicitly tagged becomes `500` (code `internal`). `POST .../sync`
  on a repository without an upstream (`repo has no upstream`) is a `500`, but
  is arguably a client error (`400`/`409`). A failed upstream `git fetch`
  during `sync` is a `500`, but is arguably `502` (`upstream_unavailable`).
  Any future client-caused error that is not wrapped in `BadRequest` will also
  surface as `500`.
* **#24. Objects of 4 GiB or more are unfetchable, and fail whole batches.**
  `encode_frame` now errors instead of truncating, but the error aborts the entire
  `POST /v1/objects` response with `500`. One oversized object therefore makes
  every other oid in the same batch fail too, and v1 has no per-frame "too large"
  status. Options: a third `status` value (breaking for v1 decoders, which reject
  unknown statuses), or reporting such objects as missing.
* **#25. Bisect has no stateful checks.** The server cannot detect a driver that
  feeds inconsistent results, for example marking a commit good after a
  descendant was marked good, because each step is computed only from
  `(good, bad)`. This is by design, since it keeps the endpoint pinned and
  cacheable, but it is worth stating.

Added in the third draft:

* **#27. A cache's own responses carry its built-in semantics version.** The
  reference cache relays `wren-semantics` on forwarded query responses and
  sends the key's version on stored ones. On everything it answers itself
  (`/v1/capabilities`, objects, `/v1/stats`, `/healthz`, its own errors), it
  sends its own `QUERY_SEMANTICS`. Its `/v1/capabilities` body, by contrast,
  reports the version learned from upstream. The two differ when the cache and
  the server are different builds.
* **#28. Semantics changes are invisible to caches outside Wren.** HTTP caches
  and CDNs key by URL, not by `wren-semantics`, so after a semantics bump they
  may serve an old `public` answer until it expires (one year). Wren caches
  never delete entries stored under an old version; those entries only become
  unreachable and use disk until the cache directory is cleared. Options:
  put the semantics version in the URL, send `Vary` on a request header, or
  use a shorter `max-age`.

---

## 14. Writes

The write API (feature `write.v1`) creates commits, lands them on branches,
moves branches, and reverts commits, all on the server. A client sends file
contents and parents, never a tree and never a checkout. The server derives
the tree, so the cost of a write is proportional to the change, not to the
repository. The implementation is `crates/wren/src/server/writes.rs` over
`wren_core::write`. The tests are `crates/wren/tests/writes_http.rs` and
`crates/wren-core/tests/writes.rs`.

Commits created here are ordinary git commits with git ids. While a git host
is the source of truth for a repository (it has an upstream, as every
repository of the reference server has), a branch moves on the host first and
Wren's ref follows ([§14.8](#148-write-proxy-mode)).

### 14.1 Authentication

Writes need a separate token. The server is started with `--write-token`
(environment `WREN_WRITE_TOKEN`), which MUST differ from `--token`; the
reference server refuses to start otherwise. Without a write token the server
is read-only and does not advertise `write.v1`.

* The write token is also accepted everywhere the read token is
  ([§2.1](#21-bearer-tokens)), so a writer needs only one token. If the server
  has no read token, reads stay open to everyone, and writes still need the
  write token.
* The read token never grants writes.

A request to a write endpoint is answered as follows, before its body is read
and before the repository is looked up:

| Server configuration and request | Status | `code` |
|---|---|---|
| No write token configured | `403` | `forbidden`. The message says that writes are disabled and to start the server with `--write-token`. |
| No `Authorization: Bearer ...` header | `401` | `unauthorized` |
| The read token | `403` | `forbidden` |
| Any other wrong token | `401` | `unauthorized` |
| The write token | handled | |

The write token, like the read token, is server-wide: it can write to every
repository the server serves ([§11](#11-security-considerations)).

### 14.2 Common rules

| Method | Path | Success | § |
|---|---|---|---|
| POST | `/api/v1/repos/{repo}/blobs` | `200` | [14.3](#143-upload-blobs) |
| POST | `/api/v1/repos/{repo}/changesets` | `201` (`200`/`409` with `land`) | [14.4](#144-create-a-changeset) |
| POST | `/api/v1/repos/{repo}/land` | `200` (`409` on conflicts) | [14.5](#145-land) |
| POST | `/api/v1/repos/{repo}/refs` | `200` (`409` when stale) | [14.6](#146-update-a-ref) |
| POST | `/api/v1/repos/{repo}/revert` | `201` (`200`/`409` with `onto`) | [14.7](#147-revert) |

`GET /api/v1/repos/{repo}/refs` is still the read endpoint of [§4.2](#42-refs).
Only `POST` on that path writes.

* **Bodies** are JSON. `Content-Type` is not checked. A body that is not valid
  JSON, or has the wrong shape (a missing required field, a wrong type), is a
  `400` ErrorBody with code `bad_request`. Unknown fields are ignored.
* **Size.** A body may be up to 64 MiB. A larger one is a `413` ErrorBody
  with code `too_large`.
* **Caching.** Every response from a write endpoint, errors included,
  carries `Cache-Control: no-store`. Write responses do not carry
  `wren-synced-at`. They do carry `wren-protocol` and `wren-semantics`.
* **Revisions** (`parents`, `changesets`, `commit`) are resolved with the
  syntax of [§5](#5-revision-syntax), including one on-demand sync for an
  unknown revision ([§4.0.7](#407-unknown-revisions-and-on-demand-sync)).
  Fields documented as oids must be full 40-hex ids.
* **Errors** follow [§6.2](#62-status-codes): `400` (`bad_request`) for
  invalid requests, and `404` (`not_found`) for an unknown repository,
  revision, changeset or branch. Outcomes that are not errors (conflicts, a
  stale ref, a refusal by the git host) are `409` with a result object, not
  an ErrorBody ([§14.5](#145-land), [§14.6](#146-update-a-ref)).

**Person** (an author or committer):

| Field | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | No `<`, `>` or newline (otherwise `400`). |
| `email` | string | yes | No `<`, `>` or newline. |
| `time` | integer | no | Unix seconds. Default: the server's current time. |
| `tz` | string | no | `+hhmm` or `-hhmm`. Default `"+0000"`. Anything else is `400`. |

This is the shape of `author` and `committer` in CommitInfo
([§4.0.6](#406-the-commitinfo-object)). A changeset's id depends on both
signatures, including their times. A client that wants a retried request to
produce the same id MUST send `time`.

### 14.3 Upload blobs

`POST /api/v1/repos/{repo}/blobs`

```json
{"blobs": [{"content": "hello\n"}, {"content_base64": "AJ+Slv8="}]}
```

Each entry has exactly one of `content` (UTF-8 text, stored as its UTF-8
bytes) or `content_base64` (standard base64 with padding, for any bytes).
Anything else is `400`.

**Response** `200`: `{"oids": ["ce013625030ba8dba906f756967f9e9ca394464a", "..."]}`,
the blob ids in request order. They equal `git hash-object`. Uploading is
idempotent. Blobs go into the server-wide object store
([§3.2](#32-batch-fetch-post-v1objects)), so they can be fetched through the
object protocol at once.

Uploading separately is useful for large or binary files, and for content used
by several changesets. Small text files can be sent inline in a changeset
instead.

### 14.4 Create a changeset

`POST /api/v1/repos/{repo}/changesets`

```json
{
  "parents": ["main"],
  "author": {"name": "Agent", "email": "agent@example.com", "time": 1700000000, "tz": "+0000"},
  "message": "Add health endpoint",
  "changes": [
    {"path": "src/health.rs", "content": "pub fn ok() -> bool { true }\n"},
    {"path": "assets/logo.png", "blob": "<oid from POST .../blobs>"},
    {"path": "tools/run.sh", "content": "#!/bin/sh\necho v2\n"},
    {"path": "old.txt", "delete": true}
  ]
}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `parents` | array of revisions | yes, at least one | The new commit's parents, in order. Each must be a known commit. Two or more make a merge commit. |
| `author` | Person | yes | |
| `committer` | Person | no | Default: the author. |
| `message` | string | yes | The commit message. A trailing newline is added if missing. |
| `changes` | array of change requests | no (default empty) | What changes, **relative to the first parent**. |
| `land` | `{"onto": "<branch>"}` | no | Land the new changeset at once ([§14.5](#145-land)). |

A **change request** has a `path` and exactly one of:

| Field | Meaning |
|---|---|
| `content` | The new content as UTF-8 text. The server stores it as a blob. |
| `content_base64` | The new content, base64. |
| `blob` | The id of a blob already in the store ([§14.3](#143-upload-blobs)), or, with mode `160000`, a commit id. |
| `delete: true` | Delete the file. |

It may also have `mode`, a six-digit octal string: `"100644"`, `"100755"`,
`"120000"` (a symlink; the content is the link target) or `"160000"` (a
gitlink; needs `blob`). Without `mode`, a path that is a file or symlink at the
first parent keeps its mode, so an executable stays executable. Any other path
gets `100644`. A deletion takes no mode.

The server checks the following, and answers `400` if one fails:

* `path` is relative and already normalised (no leading or trailing `/`, no
  empty, `.` or `..` segments), has no NUL, and no segment is `.git` in any
  case;
* no path appears twice;
* a deleted path exists at the first parent and is a file (to delete a
  directory, delete the files in it);
* a `blob` is in the store;
* the resulting tree is consistent: a file cannot be added under a path that
  stays a file, and a file that replaces a directory needs every file in that
  directory deleted.

A change that leaves a path as it is at the first parent is skipped. An empty
`changes` makes a commit with the first parent's tree. An unknown parent is
`404`. No ref moves.

**Response** `201 Created`:

```json
{"oid": "<new commit>", "tree": "<its root tree>"}
```

Creating the same changeset twice (same parents, signatures, message, and
resulting tree) returns the same `oid`.

With `land`, the changeset is created and then landed as by
`POST .../land` with `{"changesets": [oid], "onto": ...}`. The response adds
the Landed object ([§14.5](#145-land)), and its status follows the land rule:
`200` when landed, `409` on conflicts or a refusal. The changeset exists
either way, so a client can retry the land later.

```json
{"oid": "...", "tree": "...",
 "land": {"result": "landed", "ref": "refs/heads/main", "old": "...", "new": "...",
          "changesets": ["..."], "dropped": [], "attempts": 1}}
```

### 14.5 Land

`POST /api/v1/repos/{repo}/land`

```json
{"changesets": ["<oldest>", "<newest>"], "onto": "main"}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `changesets` | array of revisions | yes, non-empty | A stack, oldest first. Each must have exactly one parent, and each after the first must be the child of the one before it. Otherwise `400`. |
| `onto` | string | yes | The branch: `main` or `refs/heads/main`. An unknown branch is `404`. |
| `committer` | Person | no | If given, every replayed changeset gets this committer. Otherwise each keeps its own. |

The server rebases the stack onto the branch's current tip and moves the
branch to the result with a compare-and-swap. If the branch moved in between,
it rebases again onto the new tip, up to 8 attempts in all. There is no queue
and no global lock. Within one server, lands to a repository take turns, which
only avoids retry storms; the compare-and-swap is what makes a land safe,
including against writers outside Wren.

The rebase looks only at the paths each changeset changed, one changeset
after the other. For each such path, with *base* the path at the changeset's
original parent, *ours* the path at the new base, and *theirs* the path in the
changeset:

* if *ours* equals *base*, the changeset's version is taken;
* else if *ours* already equals *theirs*, nothing is done;
* else, if all three are regular files, the contents are merged with git's
  three-way merge (`git merge-file`), and the modes by the same rule. A clean
  merge is taken; anything else is a conflict;
* otherwise it is a conflict.

A changeset none of whose changes remain is dropped, as `git rebase` does. If
the stack already sits on the tip and no `committer` is given, nothing is
replayed and the branch fast-forwards to the stack's last changeset. Replaying
the same stack onto the same tip twice gives the same ids. Clean results agree
with `git merge-tree --write-tree` on the same inputs (trees and conflicted
paths).

**Response**: a Landed object, tagged by `result`.

`200 OK`, landed:

```json
{"result": "landed", "ref": "refs/heads/main",
 "old": "<tip before>", "new": "<tip after>",
 "changesets": ["<new id>", "..."], "dropped": [], "attempts": 1}
```

| Field | Meaning |
|---|---|
| `ref` | The full branch name. |
| `old`, `new` | The branch before and after. `new` equals `old` when every changeset was dropped. |
| `changesets` | The landed changesets, oldest first: the rebased ids, or the stack itself on a fast-forward. |
| `dropped` | Stack entries whose changes the branch already had. |
| `attempts` | How many rebases it took: 1 unless the branch moved. |

`409 Conflict`, conflicts. Nothing moved:

```json
{"result": "conflicts", "onto": "<tip>",
 "conflicts": [
   {"changeset": "<stack entry>", "path": "README.md", "kind": "content",
    "base":   {"mode": "100644", "oid": "..."},
    "ours":   {"mode": "100644", "oid": "..."},
    "theirs": {"mode": "100644", "oid": "..."}}
 ]}
```

| Field | Meaning |
|---|---|
| `onto` | The tip the stack was rebased onto. |
| `conflicts[].changeset` | The stack entry that could not be applied. Only the first such entry is reported. |
| `conflicts[].path` | The path. Empty when the conflict was found while building the tree; `detail` then explains it. |
| `conflicts[].kind` | `content`, `add/add`, `modify/delete` (the tip modified the path, the changeset deletes it), `delete/modify` (the reverse), `mode`, or `file/directory`. |
| `conflicts[].base`, `.ours`, `.theirs` | A FileVersion ([§4.0.5](#405-the-change-object)) or `null`: the path at the changeset's original parent, at the tip, and in the changeset. |
| `conflicts[].detail` | Optional text. |

`409 Conflict`, rejected by the git host (a hook, branch protection). Nothing
moved:

```json
{"result": "rejected", "reason": "pre-receive hook declined"}
```

`409 Conflict` with an ErrorBody, code `conflict`: the branch kept moving
through all 8 attempts. Nothing moved. The client MAY retry.

### 14.6 Update a ref

`POST /api/v1/repos/{repo}/refs`

```json
{"name": "refs/heads/topic", "expected": "<oid or null>", "new": "<oid or null>"}
```

| Field | Type | Meaning |
|---|---|---|
| `name` | string | A branch. A name without `refs/` means `refs/heads/<name>`. Anything outside `refs/heads/`, for example a tag, is `400`. |
| `expected` | oid or `null` | The value the branch must have now. `null`: the branch must not exist. |
| `new` | oid or `null` | The new value, a known commit (`404` otherwise, after one on-demand sync). `null` deletes the branch. |

This is a compare-and-swap: the branch is set to `new` only if it is
`expected`. `expected` and `new` must be full ids (`400` otherwise).

**Response**: a RefUpdate object.

| Status | Body | Meaning |
|---|---|---|
| `200` | `{"result": "updated"}` | Done. |
| `409` | `{"result": "stale", "current": "<oid or null>"}` | The branch was not `expected`. Nothing changed. `current` is Wren's value of the branch, which can lag the git host until the next sync. |
| `409` | `{"result": "rejected", "reason": "..."}` | The git host refused. Nothing changed. |

### 14.7 Revert

`POST /api/v1/repos/{repo}/revert`

```json
{"commit": "<revision>", "author": {"name": "...", "email": "..."}, "onto": "main"}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `commit` | revision | yes | The commit to revert. A merge or a root commit is `400`. |
| `author` | Person | yes | |
| `committer` | Person | no | Default: the author. |
| `onto` | string | no | A branch to land the revert on. |

The revert is a new changeset whose parent is `commit` and which restores
every path `commit` changed to its first-parent version. Its message is
`Revert "<summary>"`, a blank line, and `This reverts commit <oid>.`, as
`git revert` writes it.

**Response** without `onto`: `201 Created`, `{"oid": "<revert>"}`. No ref
moves. With `onto`, the revert is landed on that branch as by
[land](#145-land), and the response is `{"oid": "<revert>", "land": <Landed>}`
with the land status rule (`200` landed, `409` otherwise). `land.new` is the
revert rebased onto the branch; `oid` is the revert before rebasing.

### 14.8 Write-proxy mode

Until Wren itself is the source of truth, the git host stays authoritative,
and its branch protection and hooks keep working:

1. A ref update (by `POST .../refs`, or the last step of a land) first pushes
   to the host from the server's mirror, with
   `git push --force-with-lease=<ref>:<expected> origin <new>:<ref>`. The lease
   makes the push a compare-and-swap on the host. Only the new commits, trees,
   and blobs are sent.
2. If the host's branch is not `expected`, the answer is `stale`; a land then
   syncs from the host and rebases again. If the host refuses the push, the
   answer is `rejected`, with the host's reason.
3. Only after the host has accepted does Wren compare-and-swap its own ref. If
   a sync already published the same value, the update still counts as done.

So after a `200` the host has the new value, and ordinary git clients see it
at once. Later syncs bring the same commits back from the host with the same
ids, which are already known. A repository without an upstream has only Wren's
ref.

### 14.9 Caches

A cache never relays writes. It presents its own upstream token, which can be
a write token, so relaying would let anyone who can reach the cache write with
the cache's credentials. A cache answers every request under `/api/` other
than `GET`, `HEAD`, and `POST /api/v1/repos/{repo}/sync` with `403` and code
`forbidden`, with a message that writes go directly to the server (the
control plane). It does not contact its upstream for them. A cache does not
advertise `write.v1`. Writers talk to the server. Readers can keep using
caches: a branch moved by a write is visible through a cache as soon as the
server has it, because unpinned queries are always forwarded.
