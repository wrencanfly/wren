# Scion 文章精读总览：核心思想与架构

> 原文：Anthropic Engineering，[Redesigning source control at Anthropic](https://www.anthropic.com/engineering/redesigning-source-control)（2026）。
>
> 本文是对原文的阅读笔记。Wren 是独立项目，与 Anthropic 无关，也没有得到其背书。
>
> 本篇是总纲，三份分析各管一个专题：
> - [01 · 问题与六条原则](research/01-problem-and-principles.md)
> - [02 · 架构重构（可据此复现）](research/02-architecture.md)
> - [03 · 组织经验与"公开版"战略](research/03-lessons-and-public-strategy.md)

---

## 一句话

**当绝大多数客户端都是一直在线的 agent 和 CI 任务时，"每个 clone 都是完整仓库"的 git 模型就成了负担。正确的做法是把计算搬到服务端：服务器回答问题、生成提交；客户端只按需取它确实要读的对象。**

## 1. 问题：规模被 AI 撑破了

| 指标 | 数值 |
|---|---|
| 日合入变更 | 2026 年 5 月约 2,500，9 月超过 30,000（约 12 倍） |
| main 分支历史 | 约 100 GB |
| 压缩后的工作副本 | 超过 25 GB |
| CI checkout 次数 | 约 2,500 万次/天（不含开发环境、agent 沙箱、研究任务） |
| 朴素 fetch 的流量 | 超过 600 PB/天 |
| 迁移前的 git 镜像 | 数百台 |

他们把 git 的扩容手段都用过了：shallow clone、sparse checkout、promisor remote、内部镜像、blob 存储里的 seed 快照。这些办法都只是拖延：

- 合入越快，seed 离 main 的最新提交就越远；
- 每台镜像都要装下整个仓库，迟早撞上单机磁盘和内存的上限；
- diff、blame 这类依赖历史的操作在 partial clone 上越来越难维护。

**关键洞察**：大家其实一直把 git 当中心化系统用（只有一个权威副本），而客户端几乎全是在线的。git 为离线、分布式设计的那些特性，反过来成了扩容的阻力。

## 2. 六条原则

| # | 原则 | 拒绝了 git 的哪个假设 | 换来什么 | 代价 |
|---|---|---|---|---|
| 1 | **客户端永远在线** | 客户端离线工作，所以需要完整副本 | 客户端按需取数 | 断网就无法工作 |
| 2 | **客户端与服务端不对称** | 每个 clone 都能当服务器 | 本地数据量极小 | 客户端几乎什么都算不了，服务器成了关键依赖 |
| 3 | **完整的源码控制服务** | 想查询就得有本地仓库，或者依赖厂商 API | CI 和工具（bisect、revert）不需要 checkout | 需要设计并维护一整套 API |
| 4 | **一切都能水平扩展** | packfile 加单机镜像 | 任何地方都没有完整的仓库副本，也不需要仓库维护 | 系统必须做成分布式 |
| 5 | **提交便宜、增量、无锁** | 本地 commit 后 push，ref 随便建 | 成本只和变更大小成正比，与仓库大小无关 | 不鼓励建 ref，必须走 trunk-based 开发 |
| 6 | **工作副本可选，并且默认 sparse** | checkout 是使用仓库的前提 | 只为实际读到的东西付费 | 遍历全树的工具会触发大量按需拉取 |

依赖关系：原则 1 是地基，有了它才谈得上 2 和 6。内容寻址（对象不可变）让 4 和 6 可以随意缓存。5 是 3 的写路径版本。

## 3. 架构

```
                  ┌──────────────────────────────────────────────┐
  写路径          │  scion-server（无状态，水平扩展）              │
  commit/rebase ─▶│  · 创建 changeset（WAL 一条记录，无锁）        │
  merge 在服务端  │  · 计算 / 缓存 derived data，回写 Spanner      │
                  │  · ref 移动 = 单行 Spanner CAS                 │
                  └───────────┬─────────────────────▲──────────────┘
                              │ Spanner + 云存储（内容寻址 blob）
                              │                     │ 未命中才回源
  读路径      ┌───────────────▼──────┐   ┌──────────┴──────────┐
  (相同 RPC)  │ scion-cache（区域）  │◀──│ scion-daemon（每节点）│◀── scn 客户端 / CI job
              │ 内存直出，磁盘格式   │   │ 从快照启动，共享对象 │    （socket）
              └──────────────────────┘   └─────────────────────┘
                     每层命中率 > 99%，严格分层隔离故障域
```

**数据结构**

- **Changeset**：WAL 里的一条记录，内容是"相对第一父提交改了哪些文件"，外加指向压缩内容的内容寻址指针。写入时不涉及 packfile、锁、跨服务器协调，也不碰 ref。只看第一父，是因为在 trunk-based 的 monorepo 里，大家只关心主线的历史和 blame（原文脚注）。
- **Derived data**：changeset 以外的一切都由**一套通用机制**递归推导：`value(cs) = f(cs, value(parents))`。blame、路径历史、提交图的 skip list、复制检测、路径搜索都是这样来的。好处有三：新增一种索引很容易；每种可以选择在 ingest 时提前算（eager）或者查询时再算（lazy）；客户端看不到这些结构，所以可以随时优化。

**协议**

- **只说"要什么"（want-only）**：客户端不告诉服务器自己已有什么，只按 id 请求具体对象。从根 tree 开始，递归请求本地缺失的 tree，然后请求 blob。每个请求的答案永远不变，所以任何一层缓存都能直接回答。传输可以断点续传，sparse 和虚拟化 checkout 也不需要专门支持：不想要的 tree 不请求就行。
- **每层 RPC 完全相同**：daemon、cache、server 对外接口一样，客户端不需要知道拓扑。

**结果**：基础设施占用缩小到原来的 1/200（CPU 和内存），checkout 从分钟级降到秒级，CI 节省了 25% 的 CPU。

## 4. 组织层面的经验

- 7 个人、5 个月，完成了 Scion、Antique（code review）以及约半打相关系统，并全量迁移。同期日合入量涨了 10 倍。
- 工程师的价值转向**设计取舍和守住系统不变量**，很多时候工作就是对 Claude 说"不"。他们 review 的是变更意图，不再逐行读代码。还要防止对抗式 review 把系统推向过度工程。
- 代码质量反而更高：hermetic simulation、fuzzing、限流都配齐了。每个组件有一个专门盯着它的 "superfan" Claude。
- 重新审视各种默认做法：在 agent 规模下，全局锁式的 merge queue 已经撑不住，于是改成合入后测试加部署前测试，冲突交给 Claude 去修。
- 安全：支持目录级 ACL；code review 能理解变更的语义类别，比如区分"只改了注释"和"改了逻辑"。

## 5. 作者对"公开版"的态度，以及 Wren 的定位

作者明确说，**Scion 不适合作为通用的 git 替代品**：它和 git 差异太大，还需要配套的 review、search、CI，而且只对超大 monorepo 才划算。他们建议**把 Scion 最成功的部分带回 git**：

1. git 服务端不必和客户端同构；
2. 由 forge 提供完整的服务端 RPC，并且最好跨 forge 标准化；
3. 换一种协商协议；
4. 让 sparse 和 shallow 能良好组合。

**Wren 正是按这个建议定位的：git 兼容的加速层。** 它部署在现有 git 托管前面，用户不需要迁移。它实现了 Scion 读路径的核心：changeset、derived data、want-only 协议、相同 RPC 的分层缓存、sparse checkout、查询 API。生成的工作目录对原生 git 完全可用。设计细节见 [design.md](design.md)。

| Scion 概念 | Wren 实现（经[审阅](reviews/)核实） |
|---|---|
| Changeset WAL | SQLite `changesets` 表（自增 seq，相对第一父的变更列表），与路径索引同一事务写入 |
| Spanner | SQLite（每个仓库一个），v0.1 只能单主机，见 design §1 |
| 内容寻址 blob 存储 | 一对象一文件的 loose 格式存储，没有 packfile；git mirror 只留 refs |
| Derived data 通用机制 | `trait Derivable`（递归、按需推导、首写胜出）。**只有提交图走这个框架**；路径索引在 ingest 时写入；blame 沿文件的变更链递推 |
| 提交图 skip list | `GraphNode`：generation number 加 first-parent skip list，用于 ancestry、merge-base、landed、bisect |
| Blame / 路径历史 | 惰性增量 blame（只持久化精确结果）；`path_index` 路径索引 |
| want-only 协议 | `POST /v1/objects`（按 oid 批量取，结果不可变）；规范见 [spec](spec/wren-protocol-v1.md) |
| scion-daemon / scion-cache | `wren cache`：与 server 完全相同的 RPC，可以多层串联，缓存对象和固定查询；节点级共享对象目录（没有独立的 daemon 进程） |
| scn checkout | `wren checkout --sparse … --git`：sparse index，只拉 cone 边界那一层 tree |
| 服务端查询 API | `/api/v1/...`、`wren mcp`（供 agent 使用）、`wren bisect` |
| 服务端提交、无锁合入 | **暂不实现**，写入仍然走 `git push`（见路线图） |
