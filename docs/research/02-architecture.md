# 02 · Scion 技术架构重构（可据此重新实现的精度）

> 来源：Anthropic 工程博客关于 Scion（内部 VCS）的文章，全文 66 行已逐行阅读。
> 标注约定：**【原文】** = 文章明确陈述；**【重构】** = 本文基于原文 + 业界先例（Piper / Sapling-Mononoke，文章明确提到二者为 prior art）做的合理推断，非 Anthropic 公开事实。

---

## 0. 一页纸总览

```
                       写路径（API-first, server-side）
  agent / CI / tool ──RPC──▶ scion-server ──▶ Spanner (changeset WAL, refs, derived 指针)
                                   │        └▶ Cloud Storage (压缩 blob, content-addressed)
                                   ▼
                     derived data（递归、eager/lazy，算完写回 Spanner）

                       读路径 / checkout（want-only, 纯对象请求）
  scn ─socket─▶ scion-daemon(每节点) ─▶ scion-cache(每区域, 内存) ─▶ scion-server ─▶ Spanner/GCS
         同一套 RPC 在每一层都相同；层级严格，不允许跨层直连
```

【原文】核心原则：客户端永远在线；client-server 不对称（服务器算 history/blame/commit/merge/rebase，客户端只有有界对象缓存 + 工作区布局 + 文件系统监控）；所有查询 API 化；一切水平扩展、无 packfile、无全量物化副本（服务器上也没有）；commit 必须廉价、增量、无锁，成本与变更大小成正比而非仓库大小；refs 尽量少用（仅 main 与 release 分支）；工作区可选，默认 sparse/virtual。

---

## 1. 数据模型

### 1.1 Changeset = 一条 WAL 记录

【原文】
- 工作单元是 changeset，“an entry in a write-ahead log”。
- 存的东西大致同 git commit，但**不指向 tree**，而是存“相对 first parent 变化的文件列表” + 指向 blob storage 中压缩文件内容的 content-addressed 指针。
- changeset 在服务器端创建，创建后“everywhere, instantly”可用。
- 脚注：merge 相对 first parent 记录变更是“stylistic choice”；在 monorepo 的 trunk-based 开发中，大家只关心主线上的 history/blame。

```
 Changeset  cs_id = H(canonical bytes)          【重构：ID 计算方式】
 ┌──────────────────────────────────────────────┐
 │ parents:   [p1, p2?]          (p1 = 主线)     │
 │ author / committer / time / message           │
 │ changes (相对 p1):                            │
 │   path              kind     blob_ptr   mode  │
 │   src/a.py          MODIFY   sha256:…   0644  │
 │   src/b.py          ADD      sha256:…   0644  │
 │   old/c.py          DELETE   —          —     │
 │   lib/d.py          COPY←src/x.py (可选) 【重构】│
 └──────────────────────────────────────────────┘
             │ blob_ptr
             ▼
   Cloud Storage: blob/<hash>  (压缩内容，不可变)
```

**First-parent delta 的含义（【重构】）**：merge changeset 的 delta = `tree(merge) − tree(p1)`，即把 p2 侧带入的所有改动都“摊平”进 merge 的 delta。好处：(a) 沿 first-parent 链 replay delta 永远能得到正确的树；(b) blame/history 只沿 p1 走即可满足主线需求；(c) merge 的存储成本等于其对主线的真实影响。代价：沿 p2 侧的精确 blame 需额外计算（脚注说明实践中不需要）。

### 1.2 为什么写路径没有 packfile / 锁 / refs

【原文】高吞吐接收 changeset 的原因：没有 packfile 要接收，没有锁或跨服务器协调，没有 refs 要移动。Scion 技术上支持 refs，但创建和获取 commit 都不涉及 refs，所以移动 ref 是**单个 Spanner 行上的 compare-and-swap**，与创建 changeset 分离。

【重构】写一个 commit 的流程：

```
1. PutBlobs(content…)        → 按 hash 幂等写入 GCS（已存在则跳过）
2. CreateChangeset(parents, changes)
      → 校验 parents 存在、blob 存在
      → INSERT changesets(cs_id, …)   # 新主键，无热点行、无锁竞争
      → 触发 eager derived data（可异步）
3. (可选) UpdateRef("main", expected=old_cs, new=cs_id)
      → UPDATE refs SET target=new WHERE name='main' AND target=old   # 单行 CAS
      → 失败则返回 CONFLICT，调用方 rebase 后重试
```

关键点：changeset 是不可变、content-addressed 的新行，插入天然可并行；唯一的全局共享状态（ref）被压缩成一行一次 CAS。这与文中“没有全局 merge-queue 锁，改为 post-merge 测试、冲突让 Claude 修”的流程哲学一致【原文】。

---

## 2. Derived Data：通用递归机制

### 2.1 原文要点

【原文】
- changeset 只记录变化，无法快速回答“谁最后改了这行”“哪些 commit 影响了这个文件”。
- 于是从 changeset 构建额外结构：file blame、path history、commit graph 的 skip lists。凡不属于 changeset 本身的都叫 derived data，全部经由**一个通用机制**计算。
- 每种 derived data **递归定义**：某 changeset 的值由其 parents 的值计算得出。
- 三个好处：(1) 易于新增类型；(2) 每种类型可选 eager 或 deferred（避免物化不需要的数据）；(3) 客户端看不到内部结构，可随意优化。
- 后来新增的类型示例：copy detection 索引、path-based file search 索引。
- 服务器“compute derived data on the fly and write the results back to Spanner”。

### 2.2 接口草案【重构】

```rust
/// value(cs) = derive(cs, [value(p) for p in cs.parents])
trait DerivedType {
    const NAME: &'static str;              // "tree", "blame", "path_history", ...
    const MODE: Mode;                      // Eager | Lazy
    type Value: ContentAddressed;          // 通常只是一个根指针（大结构本身也是不可变对象）

    fn derive(ctx: &Ctx, cs: &Changeset, parents: &[Self::Value])
        -> Result<Self::Value>;

    // 可选：批量派生一段线性历史，摊薄 IO
    fn derive_batch(ctx: &Ctx, chain: &[Changeset], base: &Self::Value)
        -> Result<Vec<Self::Value>> { /* 默认逐个调用 derive */ }
}

// 通用驱动（所有类型共享）
fn get_or_derive<T: DerivedType>(cs_id) -> T::Value {
    if let Some(v) = spanner.get(derived[T::NAME, cs_id]) { return v }
    // 向祖先回溯直到找到已派生的边界，然后按拓扑序向前派生（迭代，避免深递归）
    let todo = ancestors_until_derived::<T>(cs_id);   // 用 skip list/generation 加速
    for cs in topo_order(todo) {
        let v = T::derive(ctx, cs, parents_values(cs));
        spanner.insert_if_absent(derived[T::NAME, cs.id], v);  // 幂等：确定性 + content-addressed
    }
}
```

**为什么无需加锁**：derive 是纯函数、输入不可变、输出 content-addressed，两个服务器并发算同一个值得到同一结果，`insert_if_absent` 即可。这就是“服务器是能顺手算 derived data 的大缓存”的前提。

**成本与 delta 成正比**：关键约束是 `derive` 只能触碰 `cs.changes` 涉及的路径，复用 parent 结构中未变化的部分（持久化数据结构 / path copying）。

### 2.3 各示例如何套进这个接口【重构】

| 类型 | Value | derive(cs, parents) | 回答的查询 |
|---|---|---|---|
| **tree / manifest** | 根 tree 的 hash | 取 `tree(p1)`，对 `cs.changes` 中每条路径做 path-copying：只重写从根到被改叶子的那条路径上的 tree 节点，其余子树按 hash 复用 | checkout、ls、GetFile(cs, path) |
| **path history (fastlog/unode 类)** | 每个路径节点 → “最后修改它的 cs” + 前驱链接 | 与 tree 同构的 Merkle 结构，但节点里存 `last_changed_cs`；被改路径（含祖先目录）更新为 `cs`，其余复用 | “哪些 commit 影响了 X”：沿链跳，不扫日志 |
| **blame** | 按文件存：每行区间 → 引入它的 cs | 对 `cs.changes` 中 MODIFY 的文件：`diff(parent_blob, new_blob)` 把未变行映射到 `blame(p1, path)`，新行标为 `cs`；未改文件不产生新数据（通过 path history 找到最后修改点的 blame） | Blame(cs, path) |
| **commit-graph skip list** | `gen`、`skip[k]` | `gen = 1 + max(gen(p))`；`skip[0]=p1`，`skip[k] = skip[k-1](skip[k-1])`（2^k 祖先，只依赖 parent 已有值） | IsAncestor、merge-base、区间 log、bisect 中点；O(log n) |
| **copy detection** | 持久化映射 `blob_hash → {(path, cs)}` | 继承 p1 的映射，加入本次 ADD/MODIFY 的 blob；新文件 hash 命中即判定为 copy/rename | 跨重命名的 history/blame |
| **path search** | 持久化路径索引（trie 或 n-gram） | 继承 p1，对 ADD/DELETE 增删 | 按路径/文件名搜索，无需 checkout |

### 2.4 Tree 从哪来（文章隐含）【重构】

文章说 changeset 不存 tree 指针，但 checkout 协议“从 root tree 开始”请求 tree 和 blob——所以 tree 必然存在，且最自然的解释就是 **tree 是一种 derived data**：

```
tree(cs) = apply(tree(p1), cs.changes)       # 只沿 first parent，与脚注一致

  tree(p1)                       tree(cs)  （src/a.py 被修改）
    R ──┬── src(S) ──┬─ a.py(A)      R' ──┬── src(S') ──┬─ a.py(A')   ← 新节点
        │            └─ b.py(B)           │             └─ b.py(B)    ← 复用
        └── lib(L)                        └── lib(L)                  ← 整个子树复用
  新写入节点数 = O(改动路径数 × 目录深度)，与仓库大小无关
```

tree 节点 content-addressed、不可变，因此天然适合多级缓存。很可能是 **eager** 派生（主线上每个新 changeset 都会被 CI 立刻 checkout）；而 blame、copy detection 之类可 lazy。文中“git-to-Scion ingestion pipeline 需维持 ingestion ordering 与数据结构不变量”【原文】也与“按拓扑序派生”相吻合。

---

## 3. 服务器 fleet【原文 + 重构】

【原文】一小批 scion-server 提供客户端所需全部 API（获取 changeset、算 diff、求 merge base、取 blob）。所有数据 content-addressed，一旦写入永不改变，任何服务器无需与其他服务器确认即可缓存——所以水平扩展、几乎无需协调。它们“本质上是大缓存，还能即时计算 derived data 并写回 Spanner”。迁移早期 Scion 只回答“metadata”查询（祖先、blame），文件内容与工作区物化是后来单独设计的。

【重构】服务器内部状态 = 无状态进程 + 两级缓存（内存 LRU、本地 SSD），键空间全部是 hash 或 `(derived_type, cs_id)`；唯一需要强一致读的是 refs 表（走 Spanner 强读）。任意请求可路由到任意副本，适合一致性哈希以提升缓存命中。

---

## 4. Checkout 协议：want-only

### 4.1 与 git 对比

```
git (have/want):
  client ──"want X; have A,B,C…"──▶ server
  server: 计算 reachable(X) − reachable(haves) → 选 delta → 压缩 → 生成 packfile（每个客户端都不同）
  client ◀──── 单个多 GB packfile ────    # 不可缓存、CPU 重、断了从头来

Scion (want-only)【原文】:
  1. 得到 root tree id（来自 cs）
  2. 查本地 store，缺哪些 tree → GetTrees([ids])，递归直到所需子树补齐
  3. 对所需 blob 重复同样过程 → GetBlobs([ids])
  每个请求都是“某个永不改变的具体对象”，任何缓存都能回答
```

【原文】结论：协议高效且**可恢复**（没有单个多 GB packfile 响应）；sparse 乃至 virtualized checkout“不需要特殊支持”，因为客户端根本不去请求不想要的 tree。

### 4.2 客户端算法草案【重构】

```python
def checkout(cs_id, include=["/"], virtual=False):
    root = rpc.ResolveRoot(cs_id)                 # derived "tree"
    frontier = [root]
    while frontier:                                # 每层一个 batch，可流水线化
        missing = [t for t in frontier if t not in store]
        store.put_many(rpc.GetTrees(missing))      # 中断后重跑：已到对象不会再请求 → 天然 resume
        frontier = [child for t in frontier for child in store.tree(t).subtrees
                    if matches(include, child.path)]      # sparse = 剪枝
    if virtual:
        mount_fs(root)                             # 读文件时才 GetBlobs([id])（FUSE/ProjFS 类）
    else:
        blobs = needed_blobs(root, include)
        store.put_many(rpc.GetBlobs([b for b in blobs if b not in store]))
        materialize(root, include)
```

往返次数 ≈ 目录深度（每层批量），而非对象数。代价是需要多轮 RTT，这在“同区域缓存 + 节点 daemon”拓扑下可接受。

---

## 5. 分层拓扑

```
 ┌───────── node ─────────┐   ┌──── region ────┐   ┌──── global ─────┐
 │ job/agent: scn ─┐      │   │                │   │                 │
 │ job/agent: scn ─┼─sock─▶ scion-daemon ─────▶ scion-cache ───────▶ scion-server ──▶ Spanner
 │ job/agent: scn ─┘      │   │ (内存,流式发送 │   │ (权威; 算 derived)│     Cloud Storage
 │  节点级对象 store       │   │  磁盘格式)     │   │                 │
 │  启动自定期刷新的 snapshot│  └────────────────┘   └─────────────────┘
 └────────────────────────┘
   每层：同一套 RPC（GetTrees/GetBlobs…），客户端不感知拓扑
   严格逐层：daemon 只连本区域 cache；cache 只连 server  → 故障域隔离
```

【原文】
- scn 通过 socket 连本节点的 **scion-daemon**，daemon 为整个节点存对象（同节点 job 复用大部分对象）；daemon 从**定期刷新的 snapshot** 启动，只与本区域的 **scion-cache** 通信。
- scion-cache **直接从内存流式发送对象，且就是 daemon 要消费的磁盘格式**，无需计算客户端缺什么、无需 delta 压缩。
- cache miss 时问权威 scion-server，后者按需读 Spanner 或 cloud storage。
- 三层响应**完全相同的 RPC**；理论上任一层可连任一层，但**严格强制分层**，避免区域 cache 故障压垮上游 server 进而拖垮全系统。
- 数字：每层缓存命中率 **>99%**，无长期状态易扩容；基础设施 CPU+内存 footprint 比原 git mirror 体系小 **200x**；CI/研究任务 checkout 快两个数量级（**秒级 vs 分钟级**）；CI 单项 **25% CPU 容量**节省；客户端与缓存都不需要 repo maintenance。
- 背景数字：main 历史近 100 GB，压缩工作区 >25 GB，CI 每日约 2500 万次 checkout，naive 拉取 >600 PB/天；峰值时运行数百个 git mirror。

【重构】严格分层的实现：每层对上游设并发/速率上限 + 请求合并（singleflight，同一 hash 的并发 miss 只发一次上游请求）；snapshot 让新 daemon 冷启动时只需补 snapshot 之后的增量，保护 cache 层免受扩容潮冲击。

---

## 6. API-first

【原文】客户端使用的所有源码控制查询**也都作为 API 暴露**，很多 job 根本不需要本地仓库。明确提到的：获取 changeset、计算 diff（如 commit 与其 merge base 之间）、求 merge base、获取 blob、file history、blame、commit 祖先关系查询；服务器端执行 commit、rebase、merge；很多写负载可以完全不 checkout。依赖它的工具：**自动 bisect 与 revert pipeline**（“without this cheap, programmatic access … wouldn’t be practical at scale”）。另外有目录级权限控制【原文】。

### 6.1 RPC 草案【重构，名称为假设】

```protobuf
service Scion {
  // —— 元数据 / 历史 ——
  rpc GetChangeset(GetChangesetReq{repo, cs_ids[]}) returns (Changesets);
  rpc Log(LogReq{repo, head, path?, limit, first_parent_only}) returns (stream ChangesetSummary);
  rpc PathHistory(PathHistoryReq{repo, cs, path, limit}) returns (stream CsId);   // derived: path_history
  rpc Blame(BlameReq{repo, cs, path, line_range?}) returns (BlameRanges);          // derived: blame
  rpc IsAncestor(IsAncestorReq{repo, a, b}) returns (Bool);                        // derived: skip list
  rpc MergeBase(MergeBaseReq{repo, cs_ids[]}) returns (CsId);
  rpc Diff(DiffReq{repo, base, head, paths[]?, mode: NAMES|UNIFIED}) returns (stream FileDiff);
  rpc SearchPaths(SearchReq{repo, cs, query}) returns (stream Path);               // derived: path index

  // —— 对象（checkout 协议；daemon/cache/server 三层同一组）——
  rpc ResolveRoot(ResolveReq{repo, cs}) returns (TreeId);                          // derived: tree
  rpc GetTrees(ObjIds) returns (stream TreeObj);
  rpc GetBlobs(ObjIds) returns (stream BlobObj);
  rpc PutBlobs(stream BlobObj) returns (ObjIds);

  // —— 服务器端写 ——
  rpc CreateChangeset(CreateReq{repo, parents[], changes[], meta}) returns (CsId);
  rpc Rebase(RebaseReq{repo, cs, onto}) returns (CsId | Conflicts);
  rpc Merge(MergeReq{repo, p1, p2, meta}) returns (CsId | Conflicts);

  // —— refs（少量，CAS）——
  rpc GetRef(GetRefReq{repo, name}) returns (CsId);
  rpc UpdateRef(UpdateRefReq{repo, name, expected_cs, new_cs}) returns (Ok | CasFailed);
}
```

### 6.2 Spanner / 存储 schema 草案【重构】

```sql
CREATE TABLE changesets (repo STRING, cs_id BYTES(32), parents ARRAY<BYTES(32)>,
  author STRING, committer STRING, ts TIMESTAMP, message STRING, ingest_seq INT64
) PRIMARY KEY (repo, cs_id);
CREATE TABLE file_changes (repo STRING, cs_id BYTES(32), path STRING,
  kind STRING /*ADD|MODIFY|DELETE*/, blob_id BYTES(32), mode INT64, copy_from STRING
) PRIMARY KEY (repo, cs_id, path), INTERLEAVE IN PARENT changesets;
CREATE TABLE derived (repo STRING, type STRING, cs_id BYTES(32), value BYTES(32)
) PRIMARY KEY (repo, type, cs_id);
CREATE TABLE objects_small (id BYTES(32), data BYTES) PRIMARY KEY (id);   -- 小 tree/blob 内联
CREATE TABLE refs (repo STRING, name STRING, target BYTES(32), version INT64
) PRIMARY KEY (repo, name);
-- 大 blob：gs://…/blob/<hash>（压缩），永不修改
```

（“server 按需读 Spanner 或 cloud storage”【原文】→ 推断小对象/元数据在 Spanner、大内容在对象存储。）

### 6.3 工具如何使用【重构】

- **Bisect**：`Log(good..bad, first_parent)` 借 skip list 取中点 → 对中点 `ResolveRoot` + sparse/virtual checkout 只物化被测目标所需目录 → 构建测试 → 递归。每步不需要完整仓库。
- **Revert**：`GetChangeset(cs)` 得 delta 与 `p1` 的 blob 指针 → 构造逆向 changes（MODIFY 回到 parent blob，ADD↔DELETE）→ `CreateChangeset(parents=[main])` 或冲突时 `Rebase` → `UpdateRef` CAS。全程零 checkout、零 blob 下载。
- **CI diff**：`MergeBase(head, main)` + `Diff(base, head, NAMES)` 决定受影响目标，然后只 sparse checkout 这些目录。

---

## 7. 实现者备忘【重构】

1. 一切对象 content-addressed + 不可变 → 缓存、并发派生、断点续传都“免费”；唯一可变状态是 refs 行。
2. Derived data 驱动器是核心抽象：先实现 tree、skip list、path history，再加 blame；每种都必须满足“成本 ∝ delta”。
3. 分层缓存要做 singleflight 与上游限流，并强制分层。
4. 对 git 的启示（文章末尾【原文】）：git server 不必与 client 同构；forge 可提供标准化的服务端 RPC；协商协议可以改；sparse/shallow 特性应能良好组合。
