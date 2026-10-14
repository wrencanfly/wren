# 评审 02 · 架构与数据结构对 Scion 机制的忠实度

> 视角：机制忠实度。只核对代码是否真的实现了原文"How Scion works"描述的机制，以及 [00-overview.md](../00-overview.md) 映射表的说法是否成立。
> 分级：**MUST-FIX**（会产生静默错误结果，或者让映射表的核心说法不成立）/ **SHOULD** / **NICE**。每条都标注了缺口是 git 兼容定位**固有**的，还是**实现捷径**。
> 原文行号指 `scratchpad/article/article.md`。

## 0. 映射表逐行复核

| Scion 概念 | Flux 声称 | 结论 | 依据 |
|---|---|---|---|
| Changeset WAL | SQLite `changesets` 表 | ✅ 成立 | `meta.rs:14-21`（自增 seq，只有 INSERT OR IGNORE，没有任何 UPDATE/DELETE）；`changes.rs:56-147` 只对比第一父，记录新旧 (mode, oid)；`repo.rs:202-211` |
| Spanner | SQLite（每个仓库一个） | ⚠️ 部分成立 | 只能单机。`design.md:82` 说"多台服务器可以并发写"，这只对共享同一个本地文件的进程成立（见 S5） |
| 内容寻址 blob 存储 | loose 文件，没有 pack | ✅ 成立 | `store.rs:56-77` 原子写、只写一次；没有维护任务。但没有任何淘汰机制（见 N3） |
| Derived data 通用机制 | `trait Derivable` | ⚠️ 言过其实 | 只有 `GraphNode` 实现了它（`graph.rs:29`，全仓库唯一的 `impl Derivable`）。blame 和 path_index 都绕开了它（见 M1、S1） |
| 提交图 skip list | `GraphNode` | ✅ 成立 | `graph.rs:29-51` 递归推导；`graph.rs:97-111`、`211-227` 是 O(log n) 的跳转和 landed |
| Blame / 路径历史 | 惰性增量 blame；`path_index` | ⚠️ 功能成立，机制有缺陷 | path_index 和 changeset 不在同一个事务里，崩溃后永远补不回来（**M1**）；blame 的结果依赖墙钟时间（S2） |
| want-only 协议 | `POST /v1/objects` | ✅ 成立 | `wire.rs:1-17`；`client.rs:147-195` 只请求缺失对象，重试时只请求仍然缺失的部分，所以可以续传 |
| scion-daemon / scion-cache | `flux cache`（RPC 相同，可串联） | ⚠️ 部分成立 | 对象 RPC 相同 ✅（`cache.rs:53-61`），回源有并发上限 ✅（`cache.rs:81`）。但 API 透传路径没有这层保护（S3），没有请求合并，也没有真正对应 daemon 的组件（N1、N3） |
| scn checkout | `flux checkout --sparse … --git` | ⚠️ 与目标自相矛盾 | `--git` 会拉取 cone 外的**全部 tree**（`checkout.rs:98-100`），违反 `design.md:12` 写的"cone 外的 tree 一律不传输"（S4） |
| 服务端查询 API | `/api/v1/...` 和 `flux mcp` | ✅ 成立 | CLI 的每个查询命令都走 HTTP（`main.rs:353-551`）；MCP 也只是 HTTP 客户端（`mcp.rs:44-206`）；没有只能在本地做的查询 |
| 服务端提交、无锁合入 | 暂不实现 | ✅ 如实说明 | 这是 git 兼容定位固有的取舍 |

整体判断：changeset、内容寻址、want-only 协议、相同 RPC 的分层，这四块骨架是忠实的。偏差集中在 derived data："一套通用机制"实际上只覆盖了提交图，而另外两种 derived data 恰好因为不在框架里，失去了框架本来提供的"缺了就重算"的自愈能力。

---

## 1. MUST-FIX

### M1. path_index 与 changeset 分两个事务写入，崩溃后永久缺失，log 和 blame 会静默出错（实现捷径）

**原文**："We call everything that isn't directly part of the changeset 'derived data,' and we compute it all through one generic mechanism."（L30）

**代码**：
- `repo.rs:212-221`：第一个事务只写 changeset 行。
- `repo.rs:224`：推导 GraphNode。它走 `Derivable`，即使丢了也能惰性重算。
- `repo.rs:234-239`：**第二个事务**才写 path_index。
- `repo.rs:122-124`：下次 sync 时，`has_changeset` 为真就直接 `continue`。

如果进程在两个事务之间退出（kill、OOM、部署重启），这个提交的 changeset 已经存在，path_index 却永远不会再写。原因是 path_index 不在 `Derivable` 框架里，没有任何"缺了就补"的路径。后果是：

1. `log -- path`（`repo.rs:365-384`）静默漏掉这个提交；
2. `blame` 通过 `last_change`（`blame.rs:77-79`）找到一个更早的提交。行数碰巧一致时会给出错误归属；而且因为首写胜出（`blame.rs:168-169`、`meta.rs:326-334`），**错误结果会被永久持久化**。

首次 ingest 要连续执行几万次这样的两段式写入（`benchmarks.md` 里 git.git 有 85,545 个提交，耗时 173 秒），这个窗口在真实运维中是能碰到的。`differential.rs` 的"冷启动"测试只覆盖正常退出后的重启。

**最小修复**：在 `ingest_commit` 里先用父节点算出 generation（`1 + max(self.node(p)?.generation)`，父提交此时一定已经 ingest），再把 `insert_changeset` 和全部 `insert_path_index` 放进**同一个** `self.meta.write` 闭包。GraphNode 的推导放到事务之后，丢了也能重算。不需要把 path_index 改造成 `Derivable`。

---

## 2. SHOULD

### S1. 映射表把 derived data 全部归到 `Derivable`，与代码不符（实现捷径，只需改文档）

**原文**说的三个好处是：容易新增、每种可选 eager 或 lazy、客户端看不到（L31）。

**代码**：
- blame 自己定义 `KIND = "blame"`（`blame.rs:18`），key 是 (commit, path)，递归方向是"上一次修改这个路径的提交"，而不是"父提交"。`Derivable::derive` 的签名（`derived.rs:31`）表达不了这种递归。
- path_index 是一张独立的表（`meta.rs:22-27`），在 ingest 时硬编码写入。

因此"可选 eager 或 lazy"只对 GraphNode 成立：path_index 只能 eager，blame 只能 lazy。"客户端看不到"这一条仍然成立，API 只返回结果。

**最小修复**：只改文档，不重构。00-overview 第 101 行改为"`trait Derivable`（提交图）；blame 复用 `derived` 表和首写胜出语义，但递归逻辑是专用的；path_index 是 ingest 时写入的倒排索引"。`design.md:84-88` 的表格加一列"是否经过 Derivable"。把 blame 硬塞进按 changeset 推导的框架，会变成按提交物化所有文件的 blame，属于过度工程。

### S2. blame 的推导结果依赖墙钟时间，却按纯函数持久化（实现捷径）

**原文**："Each type of derived data is defined recursively, so its value for a changeset is computed from its values for that changeset's parents."（L30）

**Flux 自己的不变量**：`design.md:143`，"derived 值必须是 (changeset, 父值) 的纯函数；禁止依赖时间"。

**代码**：`blame.rs:187-188` 给 diff 设了 5 秒的墙钟 deadline，超时后返回一个非最小的 diff。这个结果随后被 `put_derived` 首写胜出地永久保存（`blame.rs:168-169`）。于是同一个输入在负载高的服务器上可能得到不同的、更差的答案，而且这个答案再也不会被修正。

**最小修复**：调用后检查 `Instant::now() >= deadline`。如果超时，就把结果返回给调用方，但不执行 `put_derived`。

### S3. 缓存层的 API 透传绕过了"严格分层"保护（实现捷径）

**原文**："We want to avoid a situation where a failure in a regional scion-cache overwhelms the upstream scion-servers"（L38）

**代码**：
- 对象回源会先获取信号量（`cache.rs:81`）。
- `proxy`（`cache.rs:185-212`）不获取信号量，用的 `reqwest::Client::new()` 也没有设超时（`main.rs:304`）。

blame、merge-base 这类需要计算的查询，恰恰是最贵的上游负载，却完全没有限流。`design.md:134` 声称的故障隔离只覆盖了对象路径。

**最小修复**：在 `proxy` 开头加一行 `let _p = st.upstream_permits.acquire().await?;`（或者单独建一个信号量），并在构造 http client 时设置超时。

### S4. `--git` 模式拉取全部 tree，与设计目标矛盾（不是固有限制）

**原文**："Sparse and even virtualized checkouts don't need special support, because the client simply doesn't ask for trees it doesn't want."（L36）

**代码**：`checkout.rs:98-100` 在 `opts.git` 为真时，cone 外的 tree 也会入队。`design.md:115` 和 `benchmarks.md:30` 都说明了这一点，但 `design.md:12` 的目标仍然写着"sparse 范围外的 tree 和 blob 一律不传输"。映射表把 `--git` 模式对应到 scn checkout，而正是这个模式违背了原文的核心性质。在 monorepo 规模下，所有 tree 的总量会随仓库大小增长，而不是随 cone 大小增长。

这**不是** git 兼容固有的限制：git 的 sparse index（`index.sparse=true`）允许 cone 外的目录在 index 里只占一个指向 tree oid 的条目，这个 oid 在父 tree 里已经有了，不需要再拉。

**最小修复**：先修正 `design.md:12`，标明 `--git` 是例外。之后 `index.rs` 支持输出 sparse-directory 条目和 `sdir` 扩展，`checkout.rs` 在 `--git` 模式下不再递归 cone 外的目录。

### S5. 权威层只能单机，但文档暗示可以多服务器（当前阶段的实现捷径；路线图 v0.2 已列出）

**原文**："any server can cache it without checking with the others … They are essentially large caches that can also compute derived data on the fly and write the results back"（L34）

**代码**：
- 进程状态 `ServerState { store, repos }`（`server.rs:22-25`）包含本地对象目录、本地 SQLite（`meta.rs:97-104`）、本地 `mirror.git`，还有进程内的 `memo` 和 `sync_lock`（`repo.rs:27-28`）。
- 跨机器无法共享这些状态。
- 同一台机器上的多个进程虽然可以共享 data 目录（WAL、`INSERT OR IGNORE` 和 CAS 都能保证安全），但每个进程都会启动自己的 poller（`server.rs:100-123`），`sync_lock` 也只在进程内有效，所以多个进程会同时对同一个 mirror 执行 `git fetch`。

"按需计算并回写"这一点是忠实的：GraphNode 和 blame 都是惰性计算后回写。memo 只缓存不可变的值，不存在一致性问题。

**最小修复**：在 `design.md` §9 加一句："权威层目前是单机的；同一台机器上多进程共享 `--data` 时，只应让一个进程运行 poller（`--poll 0`）。"同时把 `design.md:82` 的"多台服务器"改成"多个进程"。

---

## 3. NICE

- **N1 没有请求合并**（实现捷径，已在 `design.md:159` 说明）：新的 main 提交出现时，大量 CI 任务会同时请求同一批对象，未命中会各自回源。信号量能限制上游压力，但会在缓存层排队。路线图 v0.2 已经计划加 singleflight。
- **N2 对象响应没有真正流式传输**：原文说缓存是"stream objects directly from memory"（L37）。`server.rs:139-147` 先把整个批次（最多 10,000 个对象）拼进一个 `Vec`；`cache.rs:95` 用 `resp.bytes()` 把上游响应整体读进内存，`cache.rs:151-154` 再整体拼装。帧格式本身可以自我分隔，客户端也已经增量解码（`client.rs:207-216`），所以改成 `Body::from_stream` 的成本不高。
- **N3 没有对应 scion-daemon 的组件，存储也没有上限**：原文说客户端维护一个"bounded local object cache"（L17），daemon"starts from a periodically refreshed snapshot"（L37）。Flux 节点上的存储（`main.rs:560-563`）和缓存目录都只增不减（`store.rs` 里没有任何删除路径），同一节点上并发的 checkout 进程之间也没有合并请求。`cache.rs:3` 建议每个节点跑一个 cache，但没有说明要让它的 `--dir` 与 `FLUX_STORE` 指向同一个目录；否则节点上每个对象会存两份。建议在文档中补充这种部署方式。
- **N4 promisor 指向源站，懒拉取会绕过缓存层**（`checkout.rs:283-291`）：这是固有限制，因为 Flux 不实现 git 协议（`git_source.rs:3`）。`design.md:123` 已有说明。
- **N5 WAL 序号没有对外暴露**：`ChangesetRow.seq` 在 API 里没有被使用。外部系统没法"从序号 N 开始订阅 changeset"，只能翻 log。
- **N6 缓存层不校验对象**（`cache.rs:103` 调用的是 `put_raw`，不是 `put_raw_verified`）：这符合原文"缓存不做计算"的设计，客户端也会校验（`client.rs:222-227`）。代价是，一旦缓存里存进一个损坏的对象，它会被永久地返回给所有客户端。可以考虑在客户端校验失败时，记录是哪一层返回的对象，便于定位。

---

## 4. 小结

| 等级 | 数量 | 是否固有 |
|---|---|---|
| MUST-FIX | 1（M1） | 实现捷径，修复只需改一处事务边界 |
| SHOULD | 5 | S1 和 S5 只需改文档；S2 和 S3 各需几行代码；S4 先改文档，再支持 sparse index |
| NICE | 6 | N4 是 git 兼容定位固有的；其余都是阶段性取舍 |

除 M1 外，没有发现需要重构架构的问题。不建议为了"名义上的统一"把 blame 和 path_index 强行塞进 `Derivable`。真正值得借鉴的是这个框架带来的性质：缺失可以重算、结果是纯函数、首写胜出。M1 和 S2 要修的正是这两个性质。
