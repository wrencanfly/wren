# Wren 设计文档

Wren 是 **git 的服务端源码控制查询层**。它部署在任意 git 托管（GitHub、GitLab、Gitea、自建）前面，把 Anthropic Scion 的读路径带给 git 用户，不要求迁移。同时，它也是原文建议 forge 提供的"服务端 RPC"的一份可运行实现，可以作为标准化讨论的起点。

Wren 是独立项目，与 Anthropic 无关，也没有得到其背书。

- 背景与动机：[00-overview.md](00-overview.md)
- 对照 Scion 各条原则：[principles.md](principles.md)
- API 参考（草案）：[spec/wren-protocol-v1.md](spec/wren-protocol-v1.md)
- AI agent 撰写的审阅报告：[reviews/](reviews/)，修复情况见 §11

## 1. 目标与非目标

**目标（v0.1）**

- CI 和 agent 不需要本地仓库，就能通过 API 查询历史、blame、diff、祖先关系、"这个提交何时进入 main"，以及做 bisect。
- checkout 只为实际读到的东西付费：sparse cone 外的 blob 永远不传输。`--git` 模式默认写 git 的 sparse index，cone 外只需要**边界那一层** tree，更深的 tree 一律不取。
- **对象读取**可以水平扩展：对象不可变、内容寻址，每层缓存的 RPC 都相同。
- 产出的工作目录对原生 git 完全可用，`git status`、`git commit`、`git push` 都能正常工作，并且不会触发懒拉取。
- 正确性以 git 为准：每一个查询都和 git CLI 做差分测试。
- 面向 agent 的回答有界、可缓存：MCP 先把修订解析成完整 commit id，输出带行范围和长度上限。

**v0.1 的扩展边界（实话实说）**

- 元数据有两种后端（M3c 起）：默认 SQLite，每个仓库一台 server；或 `--meta postgres://…`，多台无状态 server 共用同一个 Postgres 和同一个 `--data` 目录（目前需要共享文件系统，对象存储后端是 M3d）。git mirror 的独占操作（sync、推送、一次 land 尝试）在 Postgres 上用 advisory lock 跨 server 互斥。
- 能横向扩展的是对象读取，以及"固定到完整 commit id 的查询"，后者可以被缓存层永久缓存（§6）。其余查询都由权威 server 计算。
- 权威 server 的对象存储里有一份完整的对象集。M3a 起以 git pack 格式的不可变段保存：git/git 占 331 MiB，与 git pack 相当（此前是 loose，4.7 GB）。
- **单一信任域**：token 不区分仓库，对象存储跨仓库共享，force-push 掉的对象仍然可读。每个信任域部署一套 Wren。

**非目标与方向调整**

- v0.1 曾把"取代 git 托管、成为权威源、服务端提交"列为非目标。2026-10-13 起方向改为全面向 Scion 迁移，见 [design-scion-migration.md](design-scion-migration.md)。M2 已实现服务端提交、rebase、land、revert（写代理模式：git 主机仍是权威源，Wren 先用 `--force-with-lease` 推给它）。
- 仍然不做：merge queue、code review。虚拟文件系统（M7）可选。

## 2. 组件

```
  git 托管 (GitHub/GitLab/…)
        │  git fetch（只拉 heads/tags）
        ▼
  ┌──────────────────── wren server（每个仓库一个）───────────────────┐
  │ mirror.git: 只有 refs ──alternates──▶ ObjectStore（pack 段 + loose）│  /api/v1/...  查询 API
  │   fetch 来的 pack ─absorb─▶ 拆成单个对象 ─▶ 删除 pack              │  /v1/objects  对象协议
  │ MetaDb (SQLite | Postgres): changesets | refs | path_index | derived | kv │  /v1/capabilities
  └──────────────────────────────▲─────────────────────────────────────┘
                                 │ 相同 RPC（bearer token 可选）
                     ┌───────────┴───────────┐
                     │ wren cache（可多层串联）│ 对象 + 固定查询的不可变缓存
                     └───────────▲───────────┘
                                 │ 相同 RPC
   ┌─────────────────────────────┴───────────────────────────────┐
   │ wren CLI / wren mcp / wren bisect                             │
   │ 节点级共享对象目录 ~/.cache/wren/objects ─alternates─▶ 每个 checkout 的 .git │
   └───────────────────────────────────────────────────────────────┘
```

| 模块 | 文件 | 职责 |
|---|---|---|
| 对象模型 | `wren-core/src/object.rs` | 解析 commit/tree/tag；loose 编码（zlib）同时用作存储格式和传输格式 |
| 对象存储 | `wren-core/src/store.rs` | 一对象一文件，原子写，与 git `objects/` 目录布局相同 |
| 源站 | `wren-core/src/git_source.rs` | 只保留 refs 的 mirror；fetch 后吸收并删除 pack |
| 元数据 | `wren-core/src/meta/`（`mod.rs` 接口，`sqlite.rs`、`pg.rs` 两个后端） | changeset 日志、ref CAS、路径索引、derived 表；Postgres 上另有跨 server 的 advisory lock |
| 段存储 | `wren-core/src/pack.rs` | git pack 格式的不可变段：idx 查找、delta 链还原、基对象缓存；查不到时重新列目录（其他 server 可能加了段） |
| 线路 delta | `wren-core/src/wire.rs` | 对象帧，含以请求方 base 为字典的 zstd delta 帧 |
| Changeset | `wren-core/src/changes.rs` | 相对第一父的变更；tree diff 的成本与变更大小成正比 |
| Derived data | `wren-core/src/derived.rs` | 按父提交递归推导的通用框架 |
| 提交图 | `wren-core/src/graph.rs` | generation 加 skip list：ancestry、merge-base、landed、bisect |
| Blame | `wren-core/src/blame.rs` | 惰性、增量；只持久化查询结果和检查点 |
| 对象协议 | `wren-core/src/wire.rs` | want-only 帧格式 |
| Sparse | `wren-core/src/sparse.rs` | 与 git cone mode 语义一致 |
| Index | `wren-core/src/index.rs` | 直接写 git index（v2/v3，skip-worktree 位，sparse 目录条目 + `sdir` 扩展） |
| Server / Cache / CLI / MCP | `wren/src/*.rs` | HTTP 服务、缓存层、客户端、MCP 工具 |

## 3. 数据模型

### Changeset

每个 commit ingest 后在 `changesets` 表里追加一行：`seq`（自增，相当于 WAL 序号）、`oid`、`parents`、原样字节的 `author`、`committer`、其余头部（`encoding`、`mergetag`、`gpgsig` 等）和 `message`，以及**相对第一父的变更列表**（路径、状态、新旧 mode 和 oid）。merge 提交也只相对第一父记录，原因同 Scion 的脚注：在 trunk-based 开发中，有意义的是主线历史。

**changeset 不存 tree。** 和 Scion 一样，tree 是派生数据：`git_tree(cs) = apply(git_tree(p1), cs.changes)`，用路径复制只重写改动路径沿途的目录，按 git 的规范格式序列化，所以派生出的 id 就是 git 的 id。commit 对象也由 changeset 加派生 tree 拼出，哈希等于 `oid`。这样服务端可以只凭"父提交 + 改了哪些文件"创建提交（迁移设计 M2），不需要客户端给出树。

git 接受一些它自己不会写的对象（如 `100664` 这类旧 mode、头部顺序不规范的 commit）。ingest 时逐个校验：派生 tree 不等于 git 的 tree，就在该行记下 `tree_override`；commit 不能按规范拆分还原，就记下 `raw_commit`。id 永远不变，后代从覆盖值继续派生。覆盖数量在 `GET /api/v1/repos` 里报告。

**changeset 与它的路径索引行在同一个事务里写入。** 已经存在的 changeset 永远不会被重新 ingest，所以如果分成两次写，中间崩溃丢掉的索引行就再也补不回来了。审阅报告 02 发现过这个 bug，现已修复。

### 源站 mirror 与对象导入

mirror 是一个 bare 仓库，通过 `objects/info/alternates` 借用 Wren 的对象存储。每次 fetch 之后：

1. 新 pack（连同 `.idx`、`.rev`）整体移进 Wren 存储的 `pack/`，成为一个不可变的段，`.idx` 最后移动，没有 `.idx` 的段不可见；
2. 零散的 loose 对象逐个写入 Wren 存储后删除。

mirror 因此只剩 refs（git.git 实测 4.3 MiB）。段永不 repack，也不 gc；Wren 用进程内的 pack 读取器（`pack.rs`：idx 二分查找、delta 链还原、按字节上限的基对象缓存）读取，git mirror 通过 alternates 原生读取同一批段。客户端和缓存层仍收发、保存 loose 编码，服务端对段里的对象按需重新编码。

mirror 的增量 fetch 要用 git 自己的 commit 和 tree 做协商，所以在迁移设计 M5（Wren 成为数据源）之前，这些对象仍然导入对象存储。派生出的 tree 与它们逐字节相同，写入时直接跳过。

fetch 下来的对象顺序是任意的，所以"tree 存在就意味着整棵子树都存在"这个不变量只在导入完成后才成立。导入永远在 ingest 提交之前做完；中断的话，因为 pack 还没删，下次会整个重做。

### Derived data

```rust
pub trait Derivable {
    const KIND: &'static str;
    fn derive(repo: &Repo, cs: &ChangesetRow, parents: &[Arc<Self>]) -> Result<Self>;
    fn encode(&self) -> Vec<u8>;
    fn decode(b: &[u8]) -> Result<Self>;
}
```

`repo.derived::<D>(oid)` 先迭代地推导所有缺值的祖先（不递归，避免深历史栈溢出），再推导自身，结果写入 `derived(kind, key, value)`。值是不可变输入的纯函数，所以 `INSERT OR IGNORE`（首写胜出）是安全的。在 Postgres 后端上，共享这张表的是多台主机上的多个 server（`ON CONFLICT DO NOTHING`）。

各种派生数据**并非都走同一个框架**，下表如实列出：

| 数据 | 机制 | 时机 | 用途 |
|---|---|---|---|
| `GitTree` | `Derivable`：由第一父的 tree 加变更列表推出 | ingest 时随 changeset 同一事务写入 | changeset 的根 tree；commit 对象据此派生 |
| `GraphNode` | `Derivable`：值由父提交的值递归推出 | ingest 时提前算，丢失可重算 | generation、first-parent 深度、parents、skip list（`skip[k]` 是往回 2^k 步的第一父祖先） |
| `path_index` | ingest 时直接写的索引，和 changeset 同一事务 | ingest 时 | 每个变更路径及其所有父目录 → (generation, commit) |
| `blame` | 同样是递归思想，但按 (path, commit) 存，沿**该文件的变更链**往回递推，而不是沿父提交；只存查询结果和检查点 | 查询时再算 | 每行的来源（commit、原行号） |

blame 之所以不套 `Derivable`：那样就得为每个 changeset 都推导"所有文件的 blame"，属于过度工程（审阅报告 02 的结论）。

### 主要算法

- **is_ancestor(a, b)**：先比较 generation 剪枝。快速路径是用 skip list 在 O(log n) 内判断 a 是否在 b 的第一父链上；否则做按 generation 剪枝的搜索。
- **merge_bases(a, b)**：按 generation 降序处理（这本身就是一种拓扑序），给节点打 P1/P2/STALE 标记。结果天然就是"最佳公共祖先"，不需要事后去冗余。
- **landed(c, main)**：在 main 的第一父链上用 skip list 做二分（binary lifting），O(log n) 次 ancestry 判断就能找到"第一个包含 c 的 main 提交"。原文的反面例子是"线性扫 commit log 来查合入时间"，这里正是为了避免它。
- **bisect_step(good, bad)**：在 bad 的第一父链上，以 `landed(good, bad)` 为下界，用 `depth` 直接算出中点。每一步 O(log n)，不扫 log，也不需要 checkout。
- **log -- path**：用 `path_index` 只访问改过该路径的提交，(generation, oid) keyset 分页，再按"是否在第一父链上"过滤。
- **blame**：一次扫 `path_index`，取出该文件在第一父链上的整条变更链，直到遇到一个已持久化的结果（或文件被创建的那次变更）。第一父链只会收缩，所以一批候选可以并行判定。每一步的新旧 blob 直接取自 changeset 的 change 记录，不读树；blob 按批并行读取解压，上一步的新内容就是下一步的旧内容。然后往前逐个做行 diff。行 diff 用 git 的默认算法：Myers（带 xdiff 的代价启发式）加 indent heuristic 的 hunk 滑动（imara-diff），确定性、无时间预算，结果是纯函数。只持久化本次结果和每 32 步一个检查点（同一个事务）；查更老的版本时，从最近的检查点往前推，最多 31 步。存储键是 (path, commit)，一次范围扫描就能找到该文件所有已持久化的结果。

## 4. 对象协议

规范全文见 [spec/wren-protocol-v1.md](spec/wren-protocol-v1.md)。

```
POST /v1/objects   {"oids": ["<hex>", ...]}            (≤ 10,000 个)
→ 每个 oid 一帧：oid(20) | status(1: 0=有 1=无) | len(4, BE) | data(loose zlib)
GET  /v1/objects/{oid}  → 单个对象，Cache-Control: immutable（CDN 友好）
GET  /v1/capabilities   → {"protocol":1,"object_format":"sha1","tier":"server|cache",…}
所有响应都带 wren-protocol: 1 响应头
```

- 客户端从不描述"我有什么"。服务端不需要计算差集，也不需要做 delta 压缩。
- 各层缓存和传输都直接使用 git 的 loose 编码，没有任何一层需要重新编码。
- 客户端收到对象后先做 SHA-1 校验，再写入本地存储，所以缓存层不需要被信任。
- 失败重试只会请求仍然缺失的对象，天然支持断点续传。

## 5. Checkout

1. 解析 rev，取 commit。
2. 从根 tree 开始，**逐层广度优先**：每层收集本地缺失的 tree oid，批量并行请求（每批 1000 个，16 路并发）。
3. 只深入 sparse cone 内的目录。
4. 批量拉取需要的 blob，用 rayon 并行写出文件（包括可执行位、symlink、gitlink 占位目录）。
5. `--git` 模式生成 `.git`：

| 配置项 | 作用 |
|---|---|
| `objects/info/alternates` → 节点级 Wren 存储 | 对象零拷贝，同一台机器上所有 checkout 共享 |
| `shallow` = 当前 commit | 更早的历史通过 Wren API 查询 |
| `extensions.partialclone=origin`，promisor remote 指向源站 | 万一需要缺失对象，git 会自己去源站按需拉 |
| cone 模式 sparse-checkout | 与 `git sparse-checkout` 语义一致 |
| **sparse index**（`index.sparse=true`）：cone 外每个目录是一条 `dir/` 条目，指向 tree oid，带 skip-worktree 位，外加 `sdir` 扩展 | cone 外只需要边界那一层 tree |
| index 由 Wren 直接写，带 stat 数据 | `git status` 立即是干净的，不需要 re-hash |

两个实验结论：

- **为什么 Wren 自己写 index**：用 `git read-tree` 加 `sparse-checkout reapply` 构建 index，会触发 promisor 去拉 cone 外的 blob。
- **为什么边界 tree 必须拉**：sparse index 下，`git status` 拿 HEAD 和 index 做比较时，**仍然会读每个 sparse 目录自己那一层 tree**，但不会读更深的。我们用 git 自己生成的 sparse index 验证过，行为相同。所以边界 tree 要拉，更深的不拉。用 `GIT_NO_LAZY_FETCH=1` 验证过，status、diff、commit、push 都不会触发懒拉取。

`--full-index` 会改写成经典的完整 index：要拉 cone 外的全部 tree，但仍然不拉 blob。这个模式留给不支持 sparse index 的 git 命令。

## 6. 缓存层

`wren cache --upstream <server-or-cache>`：

- 对象请求先查本地存储，未命中的合并成一个批次回源。
- **固定查询的不可变缓存**：所有 rev 参数都是 40 位完整 commit id 的查询，答案永远不变。server 会给这类响应加 `Cache-Control: immutable`，缓存层把它落盘，以后直接回答。分支名或默认 HEAD 的查询一律透传。
- **查询语义版本**：答案是"不可变输入 + 算法"的纯函数。算法改变答案时（例如换 blame 的行 diff 启发式），`QUERY_SEMANTICS` 加一，并通过 `wren-semantics` 响应头告知。缓存层用它作为缓存 key 的一部分，新旧答案不会混在一起。
- 所有回源请求（对象和查询）共用一个并发上限（`--upstream-concurrency`），并且有超时（`--upstream-timeout`），避免冷缓存把上层打垮。这对应原文的"严格分层，隔离故障域"。
- `/v1/stats` 报告对象和查询的命中情况。

## 7. 鉴权与新鲜度

- `--token` / `WREN_TOKEN`：除 `/healthz` 外，所有请求都必须带 `Authorization: Bearer <token>`，比较用常数时间。缓存层用 `--token` 校验自己的客户端，用 `--upstream-token` 向上游证明身份。token 不会写进 checkout 的 `.wren/config.json`。
- server 监听非回环地址却没设 token 时，会打印警告：所有镜像的仓库都会被任何能连上端口的人读到。
- `GET /api/v1/repos` 返回 `last_sync`（上次成功同步的时间）和 `last_sync_error`。像 `landed` 这类回答反映的是那个时刻的源站状态。
- 每个未固定的查询响应都带 `wren-synced-at` 头。
- **按需同步（read-your-writes）**：查询里出现 Wren 没见过的修订（例如刚 push 的 commit 或新分支）时，server 先同步一次再回答。同一仓库的并发请求共享一次同步，两次按需同步至少间隔 1 秒。源站的 push webhook 也可以直接指向 `POST /api/v1/repos/{repo}/sync`。
- 开启 token 时，不可变响应的 `Cache-Control` 是 `private`，只允许 Wren 自己的缓存层保存，CDN 和共享 HTTP 缓存不得保存。

## 8. 不变量（给人和 agent 的"不"清单）

1. 对象一旦写入就永不改变；所有写入都是 tmp 文件加 rename。
2. tree 只能在其全部子对象落盘后写入。mirror 吸收 pack 期间例外，所以吸收必须在任何 ingest 之前完成，中断后整体重做。
3. changeset 与其路径索引行、派生 tree 必须在同一事务里写入。
4. ref 只能通过 `cas_ref` 修改，并且只在它指向的全部 changeset ingest 完成之后才发布。
5. 持久化的 derived 值必须是不可变输入的纯函数；依赖时间、配置或外部服务的结果不得持久化。
6. 每一层对外的 RPC 完全相同，客户端不能依赖拓扑。
7. 新的历史查询必须用 derived data 或索引实现，禁止线性扫描 commit log。
8. 每个查询 API 都必须有对 git 的差分测试。
9. 派生出的 tree 和 commit 对象必须复现 git 的 id；复现不了的只能写入覆盖值（`tree_override`、`raw_commit`），不能改 id。changeset 的字段按原样字节保存，不做编码转换。
10. 任何 ref 移动都必须经过 CAS。写代理模式下先对 git 主机做 `--force-with-lease`，主机接受之后 Wren 的 ref 才跟进；land 抢输就 sync 后重新 rebase。同一 server 内，对同一仓库的 land 在 sync 锁下依次执行（避免重试风暴，也避免推送读 mirror 的 pack 时被 sync 的吸收删掉）；这把锁不承担正确性，正确性靠 CAS。
11. 服务端创建的 changeset，其变更记录必须与两棵树的 diff 完全一致（创建时校验）；每个写 API 都必须有对 git 的差分测试（`git commit`、`git merge-tree`）。

## 9. 测试

- **`cargo test -p wren-vcs-core`**：单元测试，加上 **`tests/differential.rs`**（以 git 为 oracle 的 fuzz）。
  - 种子由 `WREN_FUZZ_SEEDS` 指定。CI 跑一个种子，可以在夜间任务里跑多个。
  - 随机历史包含分支、merge、tag、mode 变更、删除，以及**大块插入、删除、重写**（这类编辑在 git.git 上暴露过 blame 的 diff bug）。
  - 场景分四轮：首次 ingest、增量 ingest、**force-push 主干并删除分支和 tag**、冷启动。
  - 比对内容：is-ancestor、merge-base --all、rev 语法、first-parent log（整体和按路径）、blame（全部文件）、diff、changeset、landed、refs。每轮还断言 mirror 里没有残留 pack。
- **`cargo test -p wren-vcs --test e2e`**：真实进程拓扑，全程设置 `GIT_NO_LAZY_FETCH=1`。
  - 拓扑是 server → cache → CLI，两层都开启 token。
  - 覆盖 sparse index checkout，以及之后用原生 git commit 并 push；还有 `--full-index` 模式。
  - 覆盖能力声明、`last_sync`、固定查询的缓存命中、bisect 和 MCP。

## 10. 已知局限

- 每个仓库只有一个权威 server（§1）。
- 只支持 SHA-1；非 UTF-8 路径在 API 层是有损的；修订语法不支持 `A..B`。
- 不跟踪重命名（blame 和 log 与 `git --no-renames` 语义一致）。git.git 的 `builtin/blame.c` 上，去掉重命名的影响后，blame 与 git 逐行一致（884/884）；行 diff 与 git 一样用 Myers 加 indent heuristic，但不保证每个文件上都与 xdiff 逐行相同。
- 节点存储和缓存目录目前**只增不减**。要回收空间，必须先确认没有 checkout 还在通过 alternates 引用这些对象，所以这需要单独设计，见路线图。
- git 在 checkout 里触发的懒拉取直接去源站，绕过了 Wren 的缓存层。正常的 status、commit、push 流程不会触发懒拉取。
- 缓存层还没有请求合并（singleflight），同一个对象的并发未命中会各自回源。

## 11. 审阅后的修复

由 AI agent 撰写的内部审阅报告见 [reviews/](reviews/)，不是独立第三方审阅。

| 来源 | 问题 | 状态 |
|---|---|---|
| 02 M1 | changeset 与路径索引分两个事务写，崩溃后索引永久缺失 | ✅ 已改成同一事务 |
| 02 S2 | blame 的时间预算破坏"纯函数"不变量 | ✅ 行 diff 改为确定性、无时间预算（imara-diff），不再有不精确结果 |
| 03 M1–M3 | benchmark 的 git 对照组配置有误（filter 没生效、没有 commit-graph、热节点场景是重新 clone） | ✅ 脚本已修复（filter 静默失效会直接报错），重跑后的数据见 benchmarks.md |
| 01 M1、02 S4 | `--git` 模式会拉全部 tree，与"只为读到的东西付费"矛盾 | ✅ 改用 sparse index，只拉边界 tree；`--full-index` 保留旧行为 |
| 01 M2 | server 上有完整 mirror 加 loose 存储，两份副本 | ✅ mirror 只留 refs；server 存储里仍有一份完整对象集，已如实记入 §1 |
| 01 M3、02 S5 | "水平扩展"的说法过头 | ✅ 文档改为如实描述；固定查询可以被缓存层永久缓存 |
| 01 M4、02 S1 | "所有派生数据走同一套机制"不实 | ✅ 文档如实列出（§3） |
| 02 S3 | 缓存层的查询转发不受并发上限约束，也没有超时 | ✅ |
| 01 S1 | 客户端看不到同步新鲜度 | ✅ 加了 `last_sync` 和 `last_sync_error` |
| 01 S2 | 固定查询应该可缓存 | ✅ |
| 03 S1 | 没有协议规范 | ✅ spec v1、`/v1/capabilities`、`wren-protocol` 响应头、类型化的 404 |
| 03 S3 | 没有 bisect 原语 | ✅ `depth` 字段、`/bisect` 端点、`wren bisect` |
| 03 S4 | 差分测试算不上 fuzz | ✅ 多种子、大块编辑、force-push |
| 03 S5 | 没有鉴权 | ✅ bearer token，暴露在非回环地址时警告 |
| 03 S6 | sparse 与 promisor 的组合问题应反馈给 git | 📝 已起草，见 [upstream/git-sparse-promisor.md](upstream/git-sparse-promisor.md)，**尚未发送** |
| 01 S3 | 缓存只增不减 | ⏳ 路线图 |
| 05、07 | 固定答案的不可变承诺与算法实现绑死 | ✅ `wren-semantics` 版本，缓存 key 包含它 |
| 05 | 错误没有机器可读的错误码；私有仓库的响应被标为 `public` | ✅ `ErrorBody.code`；有 token 时用 `private` |
| 06 | MCP 默认查 `HEAD`，全部穿透缓存；输出没有上限 | ✅ 先解析成完整 id；行范围和输出上限 |
| 06 | push 后读不到自己的提交 | ✅ 按需同步、`wren-synced-at` |
| 06、07 | agent 缺少代码搜索 | ✅ `grep`（API、CLI、MCP） |
| 05、07 | 存储放大和线上字节数没有披露；"省掉协商"的归因不成立 | ✅ benchmarks.md 加了 Costs 一节，附 `bench/wire_bytes.py` |
| 07 | 名字和 FluxCD、crates.io 冲突 | ✅ 改名 Wren |
| 04、07 | 定位与原文建议冲突（"另造一个系统"） | ✅ 重新定位为服务端查询 RPC 的实现，见 principles.md；砍掉虚拟化 checkout 和"成为权威源"两项 |
| spec #1–3 | 修订语法太宽松（`main~1x`）；40 位 id 可被同名 ref 遮蔽；ref 查找顺序与 git 不同 | ✅ 严格解析，并按 git 的规则查找 |
| spec #14 | bisect 下界错误：good 不在主干上时会误判 | ✅ 下界改为"bad 第一父链上、同时是 good 祖先的最新提交"，并加了差分测试 |
| spec #6–9、#11、#12、#21 | 错误码不一致；缓存不校验哈希；降级的 blame 结果被标成不可变；frame 长度溢出；mode 表示不一致；`no-store` 头被丢掉 | ✅ |
| spec 其余 | 剩余待办项见 [spec §13.2](spec/wren-protocol-v1.md) | ⏳ |

## 12. 路线图

见 [design-scion-migration.md](design-scion-migration.md)：M1（派生树）和 M2（服务端写入）已完成；接下来是 M3 存储、M4 节点 daemon、M5 数据源翻转和 git 协议前端、M6 派生数据扩展和权限、M7 虚拟化检出（可选）。上游工作（sparse/promisor 报告、protocol v2 提案）照旧。不做：merge queue、code review。
