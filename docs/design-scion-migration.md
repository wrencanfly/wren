# 设计：Wren 向 Scion 架构迁移

> 状态：已确认（D1–D5 均按推荐）。**M1 ✅ 2026-10-13，M2 ✅ 2026-10-14，M3a ✅ 2026-10-14**，其余待做。
> 范围：用户选择"全面 Scion 化"，即下文 M1–M7 全部纳入路线图。
> 这推翻了 v0.1 的定位（只读、git 主机是数据源、"不做服务端写入"），也推翻了
> [reviews/04](reviews/04-positioning-vs-intent.md) 和 [reviews/08](reviews/08-synthesis.md)
> 的建议。本文 §9 写明这次反转的理由和代价。
>
> 记号：**【原文】** 是 Anthropic 文章明确说的；**【图1】【图2】** 指文章中的
> "Shape of a Scion changeset"图和拓扑图；其余是我们的设计。

---

## 0. 一页纸

### 现状（v0.1）和目标对照

| | v0.1 | 目标 |
|---|---|---|
| 数据源 | git 主机；Wren 从 mirror 摄入 | Wren 的 changeset 日志；git 主机变成下游镜像（M5） |
| changeset | 键是 git commit id，存 tree 指针和变更列表；commit 和 tree 对象从 git 原样存下来 | 键仍是 git commit id，**不存 tree**；存作者、消息、变更列表，tree 和 commit 对象都是派生数据【图1】 |
| 写入 | 只能 `git push` 到主机 | 服务端 upload / commit / land / revert，CAS 移动 ref【图2 控制面】 |
| 元数据 | 单机 SQLite | `MetaStore` 接口：SQLite（单机）和 Postgres（多台无状态 server） |
| 文件内容 | loose 对象，一个对象一个文件，4.7 GB | `BlobStore` 接口：只追加的段文件，按路径做 delta，后端可以是本地盘或 S3/GCS |
| 树 | git 的 tree 对象，跟 blob 存在一起 | 派生数据，存在元数据库里【图1：树在 Spanner 框内，虚线】 |
| 拓扑 | server → cache → cache，查询和对象都走缓存 | server ← 区域 cache ← 节点 daemon；控制面直连 server，对象面逐层走【图2】 |
| 节点 | alternates 共享一个不淘汰的对象库 | `wren daemon`：有容量上限、会淘汰、从快照启动【图2 Periodic snapshot】 |
| 旧客户端 | 不支持，stock git 只能连主机 | git smart-HTTP v2 前端（upload-pack / receive-pack）【图2 Antique】 |
| 派生数据 | graph、blame；路径索引在摄入时单独维护 | 统一机制：tree、git_commit、graph、path history、blame、copy/rename、路径搜索 |
| 权限 | 每台 server 一个信任域 | 每个仓库授权、写权限分级；目录级 ACL（M6） |
| 测试 | 对 git 的差分 fuzz、端到端 | 再加：git 往返校验、对 `git merge-tree` 的差分、确定性多层仿真加故障注入 |

### 里程碑

```
M1 数据模型翻转（树变成派生数据）      ← 只改内部，API 不变        ✅
M2 服务端写入（写代理模式）            ← 给 agent 的最大价值        ✅
M3 存储可扩展（段存储、路径 delta、Postgres、多 server）
M4 节点 daemon 和严格分层（有界缓存、快照、仿真测试）
M5 数据源翻转和旧客户端（git 协议前端、git 主机变下游）
M6 派生数据扩展和权限（path history、重命名、搜索、目录 ACL、限流）
M7 虚拟化检出（FUSE，可选）
```

M1→M2 是主线。M3 和 M4 可以并行。M5 依赖 M2 和 M3。M6 和 M7 相互独立。

---

## 1. 关键决策（需要你拍板）

| # | 问题 | 推荐 | 备选 |
|---|---|---|---|
| D1 | changeset 用什么 id | **git commit id**：commit 对象由 changeset 派生，id 就是它的 git 哈希 | 独立的 SHA-256 内容 id 加上 git 映射表（Mononoke "bonsai" 的做法） |
| D2 | 多 server 的元数据后端 | **Postgres** | FoundationDB；或者 SQLite 加单主节点 |
| D3 | 虚拟化检出（M7） | **推迟**，等 M1–M6 完成再定 | 现在就进入范围 |
| D4 | 旧客户端怎么拿 pack | **用 gix 从存储即时生成 pack**，先不做 delta | 维护一份物化的 git 仓库，交给 `git upload-pack`（违反"没有完整物化副本"） |
| D5 | M2 写代理期间谁先落地 | **git 主机先接受，Wren 再移动 ref**（主机仍是数据源） | Wren 先落地，再异步推送（相当于提前做 M5） |

### D1 展开

Scion 的 changeset 有自己的 id。Wren 的推荐方案是：changeset 记录的内容和 Scion
一样（不含 tree），但它的 id 定义为"由它派生出的 git commit 对象的哈希"。

- **好处**：全系统只有一种 id；API、CLI、MCP 不变；git 用户看到的 SHA 就是
  Wren 的 id；object plane 按 commit id 请求 commit 对象时，服务端可以直接按键
  找到 changeset 再派生；不需要映射表，也就没有映射表一致性的问题。
- **代价**：创建 changeset 时必须先同步派生出 tree，才能得到 id。路径复制的
  成本是"改动路径数 × 目录深度"，仍然和改动大小成正比，不违背【原文】"commit
  成本与变更大小成正比"。另外，id 绑定 git 的哈希算法；git 自己的 SHA-256
  对象格式可以按同样方式支持。
- **Mononoke 选独立 id 的原因**是它要同时服务 hg 和 git。Wren 只服务 git，
  所以没有这个需求。

---

## 2. M1：数据模型翻转

### 2.1 changeset 记录

对应【图1】的字段：

```sql
CREATE TABLE changesets(
    seq            INTEGER PRIMARY KEY,     -- 摄入或创建顺序
    oid            BLOB NOT NULL UNIQUE,    -- = 派生出的 git commit id（D1）
    parents        BLOB NOT NULL,           -- p1 在前
    author         BLOB NOT NULL,           -- 原样字节："Name <email> 1726363509 +0000"
    committer      BLOB,                    -- 可选；缺省时派生 commit 用 author【图1 Optional】
    message        BLOB NOT NULL,           -- 原样字节，不做 UTF-8 转换
    extra_headers  BLOB,                    -- gpgsig、mergetag、encoding 等，原样、有序【图1 signature Optional】
    changes        BLOB NOT NULL            -- 相对 p1：[(path, kind, blob_oid?, mode?)]
);
```

- 不再有 `tree` 列，也不再有 `commit_time` 列（从 committer 解析）。
- `changes` 沿用现有编码。路径、文件模式和 blob id 足以重建树；`old` 版本是
  冗余的，可以从 p1 的树查出来，但继续保留，因为 blame 和 diff 要用它。
- gitlink（模式 160000，子模块）照常记为一条变更。现有 `diff_trees` 已经这样做。

### 2.2 派生类型 `git_tree`

```
git_tree(cs) = apply(git_tree(p1), cs.changes)       # 只沿 p1
```

- **路径复制**：只重写从根到被改叶子沿途的目录。未改的子树按 oid 复用。新写入
  的树节点数等于"改动路径数 × 目录深度"。
- 按 git 的规范顺序序列化（目录按 `name/` 排序，模式写成 `40000` 等），这样
  派生出的 oid 和 git 写出的完全一致。
- 删除最后一个文件时，空目录随之消失，与 git 行为一致。
- **树放在哪里**：设计稿原本按【图1】把树放进元数据库。实现时发现 M5 之前做不到：
  git mirror 做增量 fetch 时要用 git 自己的 commit 和 tree 协商，所以这些对象必须
  留在对象存储里。派生出的树与它们逐字节相同，`put` 时直接跳过。M1 因此把派生
  树也写进同一个内容寻址的对象存储；树挪进元数据库的工作推迟到 M3/M5。
- 派生值（根 tree id）**和 changeset 在同一个事务里写入**。后代从它继续派生，
  所以它不能像 graph 那样"丢了再算"。
- 派生走现有的 `Derivable` 机制（`treegen::GitTree`），摄入和创建时都同步、立即派生。

### 2.3 派生类型 `git_commit`

由 tree、父提交 id、author、committer、`extra_headers`、message 按 git 格式拼出
commit 对象；它的哈希必须等于 `changesets.oid`。commit 对象不持久化，按需
生成并放进内存 memo。

### 2.4 从 git 摄入：先校验，必要时写入种子值

现实中的 git 历史有不规范对象，比如零填充的模式、排序错误的树、非 UTF-8 消息。
摄入流程是：

1. 从 git commit C 构造 changeset：字段原样复制，`changes` = diff(tree(p1), tree(C))。
2. 派生 `git_tree(cs)`。**如果不等于 `C.tree`**，把 C 的原始树写进 `trees` 表，
   并把派生值强制设为 `C.tree`，同时计数 `tree_overrides`。之后的子提交从这个
   种子值继续做路径复制，所以不规范的条目只要没被改动，就逐字节保留。
3. 派生 `git_commit(cs)`。**如果哈希不等于 C.oid**，存下 C 的原始字节作为覆盖，
   并计数 `commit_overrides`。
4. 两个计数器写进 `/repos/{repo}` 的信息里，并作为门槛指标（见下）。

### 2.5 存储变化（已按实现修订）

- M5 之前，git 的 tree 和 commit 对象仍从 mirror 导入（原因见 2.2）。
- `Repo::commit` 和 `Repo::commit_object` 不再读对象存储，而是从 changeset 派生；
  `Repo::tree_of` 读派生值。
- object plane 的线格式和客户端不变。

### 2.6 门槛

- git/git 全部历史往返一致（`tests/roundtrip.rs`，默认跳过）：每个 commit 都在有或没有覆盖的
  情况下得到原 oid。覆盖率要报告；预期低于 0.1%。
- 现有差分 fuzz 套件全部通过；fuzz 生成器增加不规范对象。
- 摄入速度回退不超过 20%。
- ~~服务端磁盘占用下降~~：推迟到 M3/M5，原因见 2.2。

**实测结果（2026-10-13，git/git，85,545 个提交）：**

| 门槛 | 结果 |
|---|---|
| 往返一致 | ✅ 85,545 个提交的派生 tree 和派生 commit 对象全部复现 git 的 id |
| override 率 | tree 98 个（0.11%），commit 0 个。98 个全部来自 2005 年 4 月的早期提交：tree 原始字节里文件 mode 写成旧格式 `100664`。git 接受这种格式，但自己已经不会再写；之后的提交由 git 规范重写，派生又对上了 |
| fuzz | ✅ 每轮都校验往返，override 必须为 0；另有专门构造不规范对象的测试 |
| 摄入速度 | 185 s → 217–219 s（+17%，在 20% 以内） |

---

## 3. M2：服务端写入（写代理模式）

这一步对应【原文】"每次 commit、rebase、merge 都在服务端发生"和【图2】控制面的
"upload, commit, land"。在 M5 之前，git 主机仍是数据源（D5）。

### 3.1 API

| 请求 | 作用 |
|---|---|
| `POST /api/v1/repos/{repo}/objects/upload` | 上传 blob，复用现有帧格式；逐个校验哈希；幂等 |
| `POST /api/v1/repos/{repo}/changesets` | `{parents, author, committer?, message, changes:[{path, blob?, mode?, delete?}]}` → `{oid, tree}`。校验：父提交存在、blob 存在、路径规范、文件和目录不冲突。**不移动任何 ref** |
| `POST /api/v1/repos/{repo}/refs/{name}` | `{expected, new}` 做 CAS；不一致时返回 409 和当前值 |
| `POST /api/v1/repos/{repo}/land` | `{changesets:[…], onto:"main"}`：在服务端把一串 changeset rebase 到 ref 的最新位置，再做 CAS；CAS 失败就自动重试 rebase；有冲突时返回 409 和冲突列表 |
| `POST /api/v1/repos/{repo}/revert` | `{oid, onto}`：反转一个 changeset（M 改回 p1 的 blob，A 和 D 互换），然后 land |

### 3.2 服务端 rebase

把 changeset X（原基 B）放到新基 T 上时，**只处理 X 改到的路径**，成本和 X 的
大小成正比，不看整棵树：

- 对 X 改动的每条路径 p：
  - 如果 `blob(T,p) == blob(B,p)`，直接套用；
  - 否则，如果 X 的结果已经等于 `blob(T,p)`，什么都不做；
  - 否则对文本做三方行合并（base 取 `blob(B,p)`，一边是 `blob(T,p)`，另一边是
    X 的新 blob），采用 diff3 语义；合并干净就上传新 blob，不干净就报冲突，冲突
    内容包括路径、三个 blob id 和冲突块。
- 再检查文件和目录之间的冲突。
- **差分测试的参照**是 `git merge-tree --write-tree`（git 2.38+ 提供的、不需要工作区的合并）。对随机历史，合并干净时树要一致，有冲突时冲突路径集合
  要一致。

### 3.3 写回 git 主机（写代理）

1. land 在 Wren 内部算出新的 changeset 链，派生出 git 对象。
2. 用 `git push --force-with-lease=<ref>:<expected>` 推到主机，这一步就是主机上的
   CAS。所需对象只有新 commit、改动的树和新 blob。
3. 主机接受之后，Wren 才对自己的 ref 做 CAS；主机拒绝时返回 409，让调用方重试。
4. 这样，主机的分支保护和必需检查在 M5 之前都继续有效。

### 3.4 给 agent 的接口

- MCP 新增 `wren_commit`（改动用内容或 blob 给出）、`wren_land`、`wren_revert`。
- CLI 新增 `wren commit`（从工作区里改过的文件生成）和 `wren land`。

### 3.5 权限

- token 分成只读和读写两级；写操作必须用读写 token。
- 单个请求的改动数和 blob 大小有上限。

### 3.6 门槛

- land 结果对 `git merge-tree` 差分一致。
- 并发测试：N 个客户端同时向同一个 ref land，最后 ref 的历史是线性的，不丢任何
  提交，而且每个 CAS 都可线性化。
- 端到端：agent 只靠 MCP 完成"修改 → commit → land"，全程没有任何检出。

**核心实测（2026-10-14，`crates/wren-core/tests/writes.rs`）：**

| 门槛 | 结果 |
|---|---|
| 服务端创建 = `git commit` | ✅ 60 轮随机改动（修改、新增、删除、可执行位、软链接、文件和目录互换），commit id 与 git 完全相同；重复创建是幂等的 |
| rebase 对 `git merge-tree --merge-base` | ✅ 80 轮重叠修改：47 轮干净（tree 相同），33 轮冲突（冲突路径集合相同） |
| 并发 land | ✅ 12 个线程同时 land 到 `main`：全部落地，历史线性，git 主机与 Wren 一致，`git fsck` 通过。同一 server 内的 land 依次执行（进程内锁，只为避免重试风暴）；另有一个外部推送者同时直接推主机，迫使 land 走"lease 失败 → sync → 重新 rebase"路径。30 次重复全部通过 |
| 主机被直接推送过 | ✅ 第一次推送 lease 失败，同步后 rebase 到主机的新提交上，第二次成功 |
| revert | ✅ 与 git 的三方合并结果一致 |

实现时的取舍：
- 冲突文件的三方合并调用 `git merge-file`，与 git 的合并算法逐字节一致，代价是每个双方都改过的文件启动一次进程。
- receive-pack 在 ref 事务里抢输时报 `incorrect old value provided`，与客户端 lease 失败一样按"ref 已移动"处理并重试。
- land 只接受线性的 changeset 串；服务端还不创建 merge 提交。

---

## 4. M3：存储可扩展

### 4.0 修订（2026-10-14，实现前）

原设计的段格式是"对象逐个 zstd"。动手前测了 git/git 的对象构成：

| 类型 | 解压后 | 在 git pack 里 |
|---|---:|---:|
| tree | 5,644 MiB | 123 MiB |
| blob | 8,102 MiB | 374 MiB |
| commit | 144 MiB | 80 MiB |

（上游含 PR 引用，数量比 Wren 镜像的多，但比例相同。）大头是 tree：每个提交都重写
根目录和沿途目录，单独压缩时其中的 SHA 几乎压不动，只有对同一目录的上一版本做 delta
才行。所以逐个压缩达不到门槛。

**改为：段 = git pack 格式（`.pack` + `.idx` v2），只追加、写一次、永不 repack。**

- **git mirror 能读**：它通过 alternates 原生读取 `objects/pack/`，fetch 协商和推送
  不受影响。这避开了 M1 时遇到的同一个约束（mirror 需要看到 commit 和 tree）。
- **零拷贝吸收**：fetch 来的 pack 已带 git 算好的 delta，整体改名移入即可。
- **仍符合【原文】"没有 packfile、没有维护"**：原文反对的是按客户端生成 pack、
  have/want 协商和 repack/gc；这里没有按客户端生成的 pack，段也永不 repack。
- **按路径 delta（原 4.2）**：git 的 delta 已经覆盖存储侧的需求；线路上的 delta 帧
  （M3b）由客户端在请求里带上"我已有的同路径旧版本"，服务端据此回 delta，回答仍按
  `(oid, base)` 不可变、可缓存。

M3 拆成四步：

| 步骤 | 内容 | 状态 |
|---|---|---|
| M3a | pack 段存储、进程内 pack 读取器（idx 二分、delta 链、基对象缓存）、整包吸收 | ✅ 2026-10-14 |
| M3b | 线路 delta 帧：增量获取 ≤ thin pack 的 3 倍 | 待做 |
| M3c | `MetaStore` 接口与 Postgres；多台无状态 server | 待做 |
| M3d | 段放到 S3/GCS（段不可变，适合按范围读取），本地磁盘缓存 | 待做 |

**M3a 实测（git/git，85,545 个提交）：**

| | M2（loose） | M3a（段） |
|---|---:|---:|
| 对象库 | 4.7 GB，42 万个文件 | **331 MiB**，1 个段（约 1.06 倍 git pack） |
| 首次摄入 | 约 217 s | **约 55 s** |
| tree override | 98 | 98 |
| 冷 / 热 blame | 62 / 14 ms | 29 / 10 ms |
| 检出与其他查询 | — | 无退步 |

实现中发现并修复的 bug：基对象缓存曾以"pack 在列表中的位置"为键，新段加入后列表
重排，同一个键指向另一个 pack，读出错误对象。端到端测试（多次 fetch 产生很多小段）
抓到了它；现在键是每个打开的 pack 的唯一 id。

服务端自己创建的对象（M2 写入）仍以 loose 形式写入，git 可读；把它们定期封成段
（类似 memtable → segment）留到 M3c 一起做。

### 4.3 `MetaStore` 接口

需要的语义只有这几种：

- 单行 CAS（ref）；
- 不存在才插入（派生值，先写的赢）；
- 前缀范围扫描（路径索引、derived 的键）；
- 一次事务写入 changeset 和它的路径索引行。

实现有两个：SQLite（现在的）和 **Postgres**（D2）。多台无状态 `wren server`
共用一个 Postgres 和同一个对象存储，任意请求可以发给任意一台。

### 4.4 门槛

- ✅ git/git 的存储量不超过 git pack 的 2 倍（M3a 后约 1.06 倍，此前约 15 倍）。
- `HEAD~15` 的增量获取不超过 thin pack 的 3 倍（现在约 41 倍）。
- 3 台 server 共用 Postgres 时，端到端测试全部通过。

---

## 5. M4：节点 daemon 和严格分层

对应【图2】的 `scion-daemon`、`scion-cache` 和"Periodic snapshot"。

### 5.1 `wren daemon`

```
wren daemon --socket /run/wren.sock --upstream https://cache.region.example \
            --budget 50GiB --snapshot https://…/snapshots/latest
```

- 提供和其他层完全相同的 RPC，监听 unix socket。
- 用段文件做存储，按代淘汰：热对象复制到新段，冷段整体删除。不逐个删文件。
- **淘汰和 git 检出的冲突**：现在的 `--git` 检出靠 alternates 指向节点存储，
  对象被淘汰后 git 会报错。解法分两步：
  - M4：daemon 只淘汰 blob，永不淘汰树；检出注册一个租约，租约内的根树所引用的
    blob 不淘汰。
  - M5 以后：检出的 promisor remote 指向 Wren 自己的 git 协议前端，被淘汰的对象
    可以按需取回，租约就不再需要。

### 5.2 快照

- server 或区域 cache 定期把 `main` 最新提交的热门锥形目录打成一个段文件，加上
  索引，按内容哈希发布。
- 新 daemon 启动时先下载快照（可断点续传），之后照常只要缺的对象。这样一批新
  节点同时上线时，区域 cache 不会被冲垮。

### 5.3 区域 cache

- 在磁盘之上加一层内存 LRU。
- 并发 miss 合并成一次上游请求（singleflight）。
- 保留现有的上游并发上限。

### 5.4 严格分层

- 每个进程声明自己的层级（`node`、`region`、`global`）。
- 启动时从上游的 capabilities 读出它的层级，必须恰好是上一层，否则拒绝启动。
- 这避免了【原文】说的情况：区域 cache 故障时，请求绕过它直接打到 server，
  把整个系统拖垮。

### 5.5 控制面和对象面分开

- 客户端分别配置 `WREN_SERVER`（控制面：查询和写入，直连 server）和
  `WREN_OBJECTS`（对象面：通常是 daemon 的 socket）。
- 已锁定提交 id 的查询仍然可以由缓存回答，作为优化保留。

### 5.6 确定性仿真

- 用 `turmoil`（tokio 的确定性网络仿真）把 server、cache、daemon 和客户端放进
  同一个进程运行。
- 注入延迟、丢包、分区和进程崩溃。
- 检查的不变量：每个对象都经过校验；不会混用不同语义版本的答案；CAS 可线性化；
  每层的上游请求不超过上限。
- 这一项把 `principles.md` 里"Hermetic simulation"从 Partial 推到 Follows。

### 5.7 门槛

- 仿真在 1 万个种子上没有违反任何不变量。
- 50 个节点同时冷启动时，区域 cache 的上游请求峰值有上限；有快照和没有快照
  两种情况都要测。

---

## 6. M5：数据源翻转和旧客户端

### 6.1 git 协议前端

- `GET /{repo}.git/info/refs`，以及 `POST git-upload-pack`（协议 v2：`ls-refs`、
  带 `filter` 的 `fetch`）。pack 从存储即时生成，先不做 delta（D4）。
- `POST git-receive-pack`：把推上来的 pack 摄入成 changeset，再对 ref 做 CAS。
  这就是【图2】里的"Antique"客户端通道。
- 这一项也是 [upstream/git-protocol-v2-query.md](upstream/git-protocol-v2-query.md)
  需要的参考实现：在 v2 前端里实验 `query` 命令。

### 6.2 翻转

1. Wren 成为 ref 的权威来源。
2. 导出队列 `export_log(seq, ref, old, new, status)` 异步推送到原 git 主机，
   每次都带 lease。
3. 原主机设成只允许 Wren 的机器人账号推送。如果有人绕过 Wren 直接推，lease 会
   失败并触发告警，而不会覆盖。
4. 记录镜像延迟指标。

### 6.3 翻转后失去的东西

主机的分支保护、必需检查和代码审查不再拦在写入路径上。Wren 至少需要：受保护
ref 的列表、land 前调用的策略钩子（webhook，返回允许或拒绝）、以及审计日志。
合并队列和代码审查仍然不做（见 §9）。

### 6.4 门槛

- stock git 对 Wren 做 clone、fetch（含 partial clone）、push，全部成功。
- 用 `git fsck` 检查导出到原主机的历史。
- 镜像延迟的 p99 小于 10 秒。

---

## 7. M6：派生数据扩展和权限

- **path history**：把路径索引表改成通用派生机制下的一个类型。它的值仍然物化
  进专用表，以保留现在的查询速度。这一项把"派生数据"原则从 Partial 推到 Follows。
- **copy / rename 检测**：派生类型 `copies(cs)`。同一个 changeset 里，删除和
  新增的 blob id 相同，就判定为精确重命名；新增 blob 在"blob → 路径"索引里
  命中，就判定为复制。相似度匹配（git 的 `-M50%`）按需懒派生。
  有了它就能支持 `log --follow` 和跨重命名的 blame。
- **路径搜索**：文件名的 n-gram 派生索引。内容搜索的 trigram 索引（类似 Zoekt）
  放在后面。
- **目录级 ACL**：
  - 查询结果按 ACL 过滤，包括 log、diff、grep、blame 和 tree。
  - 对象面是难点：对象按哈希寻址，而缓存不知道某个对象属于哪个目录。方案草图：
    server 签发短期授权，列出允许访问的子树根；每次对象请求带上从授权根到该
    对象的路径，缓存用手里已有的树逐级验证后才返回。
  - 在这套机制完成之前，ACL 只在查询上生效，对象面仍然是一个信任域。文档会写明
    这一点。
- **按客户端限流**：每个 token 一个令牌桶。

---

## 8. M7：虚拟化检出（可选，D3）

- Linux 用 `fuser`，macOS 用 FSKit。下层是从 daemon 读的只读层，上层是可写覆盖层。
- 主要难点在 git 兼容：`git status` 会遍历所有文件，需要配合 fsmonitor 和
  sparse index。
- 推荐等 M1–M6 完成后再评估。

---

## 9. 定位反转、风险和代价

### 9.1 为什么反转

- v0.1 的定位是"给 git 加一层服务端查询"。reviews/04 和 reviews/08 认为，
  做数据源会和原文"不偏离 git 的整体模型"相冲突。
- 用户现在选择沿 Scion 的迁移路线走到底。原文描述的 Scion 迁移本身也是这个顺序：
  先只回答元数据查询，再有自己的客户端、格式和协议，最后接管写入。
- 我们保留的兼容承诺：
  - id 就是 git 的 SHA（D1）；
  - 对象就是 git 的对象；
  - stock git 始终能读写（M5）；
  - 原主机始终有一份完整、可校验的镜像，随时可以退回去。

### 9.2 需要同步修改的对外文字

M2 开始时一并修改：

- README：删去"只读，写入走 git push"；改写"Not planned"列表，只留合并队列和
  代码审查。
- `principles.md`：每完成一个里程碑，就更新对应行的状态。
- 仓库描述。
- 原文说 Scion"绝不适合作为通用的 git 替代品"。README 继续写明：大多数团队用
  git 就够了，Wren 面向大型 monorepo 和大规模 agent 并发。
- 组件名继续用 `wren server/cache/daemon`，不用 `scion-*`。

### 9.3 风险

| 风险 | 对策 |
|---|---|
| 范围：这相当于一个 Mononoke 级别的系统，Meta 用一个团队做了多年 | 每个里程碑单独可用、单独发布；M1+M2 本身就能交付最主要的价值（agent 不检出就能提交） |
| 往返不一致导致 id 变化 | 摄入时逐个校验，不一致就写入种子值；fuzz 生成不规范对象；门槛指标 |
| 写路径是新的攻击面 | token 分级；改动大小上限；审计日志；M5 之前主机的保护仍然有效 |
| 翻转后失去主机的审查和检查 | 策略钩子和受保护 ref；翻转是按仓库选择开启的 |
| 多个存储后端的维护成本 | 所有后端跑同一套接口一致性测试 |

### 9.4 新增和修改的不变量（合入 design.md §8）

1. changeset 的规范编码一旦发布就冻结，修改必须升版本。
2. `git_tree` 和 `git_commit` 必须能复现 git 的 oid；不能复现的只能写入种子值，
   并且必须计数。
3. 权威层的树永不淘汰；节点层只能淘汰能取回的对象。
4. 任何 ref 移动都必须经过 CAS，包括 land、revert、receive-pack 和导出。
5. 导出到原主机必须带 lease，永不改写主机上的历史。
6. 每一层只能连它的上一层（严格分层）。
7. 每个写 API 都必须有对 git 的差分测试（`git merge-tree`、`git revert`）。

---

## 10. 确认后的第一步：M1 拆分

1. changeset 新 schema，加上迁移方式。v0.1 是实验版本，直接从 mirror 重新摄入，
   不做原地迁移。
2. 用路径复制实现 `git_tree`，按 git 的规范顺序序列化。
3. 实现 `git_commit`，原样保留 `extra_headers` 和原始字节。
4. 摄入时校验，必要时写入种子值，并加计数器。
5. 对象面改成从 `trees` 表和派生出的 commit 取对象；mirror 只吸收 blob。
6. 测试：
   - fuzz 生成器增加不规范的树和提交；
   - git/git 全量往返校验脚本 `bench/roundtrip.py`；
   - 摄入速度和磁盘占用的前后对比。

可以并行派三个 agent：第 2 步、第 3–4 步、第 6 步的测试工具。第 1 步和第 5 步
由我来做，并把各部分合起来。
