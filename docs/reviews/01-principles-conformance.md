# 评审 01：六条原则逐条一致性审计

> 范围：Scion 原文六条原则 vs. Flux v0.1 的文档（README、00-overview、design、benchmarks）与代码（`crates/flux-core/src`、`crates/flux/src`）。
> 前提：Flux 定位为 git 兼容的只读加速层，写入走 `git push`。因此凡是"服务端写路径"相关的偏离，只要文档如实说明，就判为 NOT-APPLICABLE-BY-DESIGN，不算缺陷。
> 分级：**MUST-FIX**：文档说法与事实不符，或设计与核心原则相悖却没有写明；**SHOULD**：值得列入路线图的实质缺口；**NICE**：小问题。

## 总表

| # | 原则 | 结论 | MUST-FIX | SHOULD | NICE |
|---|---|---|---|---|---|
| 1 | 客户端永远在线 | CONFORMS | – | S1 新鲜度 | N1 |
| 2 | 客户端与服务端不对称 | PARTIAL（写路径不适用，属设计取舍） | – | S3 本地缓存无上限 | – |
| 3 | 完整的源码控制服务 / API | CONFORMS（只读部分） | – | S1、S2 | N2 |
| 4 | 一切都能水平扩展 | **DEVIATES**（对象路径符合，权威层和查询路径不符合） | **M2、M3** | S2、S3 | N3 |
| 5 | 提交便宜、增量、无锁 | 写入不适用；ingest 符合 | **M4** | – | N4 |
| 6 | 工作副本可选，默认 sparse | PARTIAL | **M1** | S4 | – |

---

## 原则 1：客户端永远在线

**原文**："we designed Scion's client and protocols around a server that is always reachable, and have clients fetch data when they need it."

**Flux 的做法**：客户端只按 oid 拉取缺失对象（`client.rs:147-195`）。查询全部走 HTTP，不需要本地仓库（`server.rs:78-97`、`mcp.rs:14-38`）。`--git` checkout 写入 `shallow`，更早的历史改由 API 查询（`checkout.rs:264-265`）。

**结论**：CONFORMS。

- **S1（SHOULD）新鲜度不可见。** 原文的 changeset "available everywhere, instantly"。Flux 是镜像，默认每 30 秒轮询一次（`main.rs:56-57`、`server.rs:100-123`），但 API 响应里没有任何同步水位：`RepoInfo` 不含最后同步时间（`api.rs:16-21`）。后果是刚 push 的提交在下一轮同步之前查不到，`landed` 会对尚未同步的 main 返回 `"not contained"`（`graph.rs:212-214`、`mcp.rs:193-195`），而 agent 无法区分"没合入"和"还没同步"。滞后本身是 git 兼容定位的固有代价，但文档没有提到。建议：在响应里带上 ref 的同步时间或 oid，并在 design §9 写明这一点。webhook 已列入 v0.2 路线图。
- **N1（NICE）** promisor remote 指向的是源站，不是 Flux（`checkout.rs:283-286`）。扩大 cone 时，懒拉取会绕过所有缓存层，直接打到 git 托管上。design.md:123 只写了"git 会自己按需拉"，没有说明这个负载去向。

## 原则 2：客户端与服务端不对称

**原文**："the server answers questions about file history and blame, creates commits, performs merges and rebases… The client maintains a bounded local object cache…"

**Flux 的做法**：历史、blame、diff、merge-base 都由服务端计算（`repo.rs:376-387`、`blame.rs:89-120`、`graph.rs:153-206`）。服务端不提供 commit、merge、rebase，design.md:20 把它列为非目标，路线图 v0.4（design.md:167）也写了，属于诚实的设计取舍。

**结论**：PARTIAL。

- **S3（SHOULD）本地对象缓存没有上限。** `ObjectStore` 没有任何删除或淘汰逻辑（`store.rs` 全文）。节点级存储通过 alternates 挂到每个 checkout 上（`checkout.rs:259-263`），所以现有设计下淘汰对象并不安全。cache 层同样只增不减（`cache.rs:99-108`）。原文强调的是 "bounded"，而路线图 v0.2 只提到"内存 LRU"（design.md:165），没有涉及磁盘淘汰。

## 原则 3：完整的源码控制服务

**原文**："all of the source control queries the client uses are also exposed via API, so in many cases jobs don't need a local repository at all."

**Flux 的做法**：提供 12 个 HTTP 端点（`server.rs:78-97`）和 10 个 MCP 工具（`mcp.rs:14-38`）。`landed` 用 binary lifting 实现，没有线性扫 log（`graph.rs:211-227`），正好避开了原文点名的反例。每个查询都有对 git 的差分测试（design.md:150）。

**结论**：CONFORMS（只读范围内）。缺写入 API 是设计取舍，见原则 2。

- **N2（NICE）** `log` 的 `limit` 上限是 10,000，并且没有游标（`server.rs:261`）。想取超过 1 万条的路径历史，只能靠调用方自己用 rev 拼分页。

## 原则 4：一切都能水平扩展

**原文**："the repository's data is distributed, and every component is independently scalable; there is no fully materialized copy of the repo anywhere, even on the server. There is no equivalent of packfiles…, so no repository maintenance is needed."

**Flux 的做法**：

- 对象路径符合原则：数据内容寻址、不可变，各层 RPC 相同，cache 可以多层串联（`cache.rs:53-61`）。
- 权威层是单机。对象存储在本地磁盘（`main.rs:242-256`）；每个仓库一个 SQLite 文件（`repo.rs:48`、`meta.rs:97-104`）；`sync_lock` 只是进程内的 `Mutex`（`repo.rs:28,75`），没有跨进程协调，所以两个 server 实例不能共享同一个仓库。
- 查询路径无法扩展。cache 对 `/api/*` 原样透传，不做缓存（`cache.rs:184-212`），所有 blame、log、merge-base 计算都落在同一台 server 上。
- 服务端保存了**两份完整副本**：一是 `mirror.git` 完整 bare 镜像（`repo.rs:78-79`、`git_source.rs:47-97`），它是 packfile 格式，持续 fetch 就需要 git 的 gc 和维护；二是首次同步时用 `--batch-all-objects` 把全部对象再导入一遍 loose 存储（`repo.rs:89-100`、`git_source.rs:191-208`）。

**结论**：**DEVIATES**。镜像是"挂在 git 托管前面"这个定位所必需的，有合理性；问题在于文档的说法与事实相反。

- **M2（MUST-FIX）"没有 packfile、不需要维护"的说法不成立。** README.md:71 写 "content-addressed objects, no packfiles"，`store.rs:4` 写 "There are no packfiles and therefore no repository maintenance"，00-overview.md:100 写"没有 packfile"。这些话对 `ObjectStore` 本身成立，对整个系统不成立：server 同时持有一个 packfile 镜像和一份 loose 全量副本，与原文"even on the server"恰好相反。design.md:158 只把 loose 导入当成规模问题提了一句，没有提到镜像，也没有承认这是对原则 4 的偏离。v0.2 路线图换成 S3 之后，`mirror.git` 依然是单盘全量副本。**修复**：在 design §9 写明"server 持有两份全量副本，其中镜像需要 git 维护"，并在路线图里说明镜像怎么处理。
- **M3（MUST-FIX）水平扩展说过头了。** design.md:13 写"读路径可以水平扩展"；README.md:77-78、`derived.rs:11-12` 和 design.md:82 写"多台服务器可以并发写同一个值，不需要协调"。v0.1 的代码里只能跑一台 server，而查询同样属于读路径。design.md:135 虽然如实写了"查询 API 原样透传"，但没有点明后果。**修复**：明确写出"v0.1 只有单个权威节点，只有对象路径能水平扩展，查询路径的扩展留到 v0.2（Postgres/Spanner）"。
- **S2（SHOULD）查询结果可以被缓存。** 当请求里的 rev 全部是完整 oid 时，`blame`、`diff`、`merge-base`、`is-ancestor` 的结果是不可变的，但 server 没有设置 `Cache-Control`（`server.rs:386-400`），cache 层也不缓存这些结果。这样做正符合原文 "servers are essentially large caches that can also compute derived data"，而且不需要换数据库。
- **N3（NICE）** 批量对象响应在发送前会完整缓冲进内存，没有流式输出（`server.rs:139-147`、`cache.rs:95`）。原文强调 cache "stream objects directly from memory"。

## 原则 5：提交便宜、增量、无锁

**原文**："The cost of creating a commit and updating its downstream data structures has to scale with the size of the change, not the overall size of the repo."

**Flux 的做法**：Flux 不在服务端创建提交，所以"无锁提交"这一项不适用（design.md:20）。把同一条标准套到 ingest 上则符合：`import_tree` 跳过已知子树（`repo.rs:174-196`）；`diff_trees` 只递归进入 id 不同的子树（`changes.rs:54-72`）；skip list 的推导是 O(log n)（`graph.rs:36-44`）；每次同步只处理新提交（`repo.rs:119-127`）；ref 用逐条 CAS 更新（`meta.rs:277-310`）。`sync_lock` 加上 SQLite 单写者会把 ingest 串行化，但在只读镜像场景下可以接受。

**结论**：写入 NOT-APPLICABLE-BY-DESIGN；ingest CONFORMS。

- **M4（MUST-FIX）"一套通用 derived 机制"的说法与代码不符。** README.md:74-79 写道 "Every index is computed through one recursive mechanism… This mechanism builds generation numbers, first-parent skip lists, the path index and blame"；design.md:84-88 把 `path_index` 和 `blame` 列为 Derived 类型；00-overview.md:101 也有同样的说法。实际上只有 `GraphNode` 实现了 `Derivable`（`graph.rs:29`，全仓只有这一处 `impl Derivable`）。`path_index` 是在 ingest 里直接写进专用表的（`repo.rs:222-239`）。blame 用的是自己的变更链回溯，再直接调用 `put_derived`，键是 (commit, path)（`blame.rs:123-173`），不是按 changeset 递归推导。**修复**：二选一，要么改文档（"graph 走 `Derivable`，path_index 和 blame 是专用的增量结构"），要么重构代码。只改文档的成本最低。
- **N4（NICE）** `is_ancestor` 的慢路径是按 generation 剪枝的 DFS（`graph.rs:132-145`），对侧枝提交来说，代价并不以 O(log n) 为界。`landed` 的"O(log n) 次判断"（design.md:94）字面上没错，但每次判断本身可能接近线性。建议在文档里补一句。

## 原则 6：工作副本可选，默认 sparse

**原文**："Scion's protocol is centered around sparse checkouts… and virtualized checkouts… so when a job does need files on disk, the client only pays for what it actually reads."

**Flux 的做法**：`flux diff`、`cat`、`ls`、`blame` 都不需要工作副本（`main.rs:428-521`）。普通 sparse 模式只下钻 cone 内的目录（`checkout.rs:93-100`）。虚拟化 checkout 列为非目标和 v0.3 路线图（design.md:21,166）。通过 API 写入属于设计取舍。

**结论**：PARTIAL。

- **M1（MUST-FIX）关于 sparse 的说法自相矛盾。** README.md:15-16 写 "A sparse checkout never transfers anything outside its cone"，design.md:12 写"sparse 范围外的 tree 和 blob 一律不传输"。但 `--git` 模式（也就是 README 快速开始里演示的主打用法）会拉取 cone 外的**所有 tree**（`checkout.rs:98`），index 也会给全仓每个文件写一条记录（`checkout.rs:314-320`）。design.md:115 和 benchmarks.md:30-31 其实承认了这一点，于是文档前后矛盾。在 monorepo 上，全量 tree 加上 O(全仓文件数) 的 index 恰恰是原文要避免的成本。**修复**：把 README 和 design §1 的措辞改为"cone 外的 blob 永不传输；`--git` 模式需要全部 tree"。
- **S4（SHOULD）用 git 的 sparse index 修掉 M1 的根因。** `index.rs` 只写 v2/v3 格式，不支持 sparse-directory 条目（`index.rs:54-92`）。如果写成 sparse index（`index.sparse=true`，cone 边界外每个目录只占一条 tree 条目），cone 外只需要边界上那一层 tree，index 大小也变成 O(cone)。这正是原文所说的 "the various sparse and shallow features in git can be made to compose well"。

---

## 总评

Flux 在**对象协议、查询 API、ingest 的增量性**三方面忠实地实现了原文的思想；把写路径和虚拟化留给后续版本，也是诚实、站得住的取舍。四个 MUST-FIX 都只需要改文档，不需要改代码。它们的共同点是：把 Scion 的性质（没有 packfile、没有完整副本、可以水平扩展、一套通用 derived 机制）写成了 Flux v0.1 的性质。这里没有提出任何新的防御性机制，以免落入原文提醒的"对抗式 review 导致过度工程"。
