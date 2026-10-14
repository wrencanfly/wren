# 评审 07：唱反调（公开发布前）

> 视角：一个持怀疑态度的开源维护者兼 staff engineer。他不关心 Flux 是否忠实于 Scion（01–03 已经审过），只问三件事：这个项目该不该存在？谁会真的用？发到 Hacker News 或 git 邮件列表之后，会被从哪里攻击？
> 范围：README.md、docs/design.md、docs/benchmarks.md、docs/spec/flux-protocol-v1.md、docs/research/03、04、docs/reviews/01–03、`crates/*/src`、测试，以及 `bench/run.py`。
> 实测：`cargo test --release` 全部通过（9 个测试函数：7 个单元测试、1 个差分测试、1 个 e2e，耗时 2 分 11 秒）。另外在 benchmark 留下的 git.git 数据目录上量了磁盘占用（见 §4.1）。本次没有修改任何代码或其他文档。

---

## 0. 一句话结论

**Flux 值得存在，但它现在的定位是错的。** 它把自己包装成"Agent-scale source control"，像一个要被部署的新系统。可它真正站得住的价值是两样东西：一份**能跑的参考实现**，演示原文要求 forge 和 git 去做的服务端查询 RPC；外加一个**单一信任域内**的历史查询旁路服务。按现在的 README 发出去，第一条高赞评论大概率是："server 端的对象存储比 git pack 大 15 倍，却自称 lean；号称 agent-scale，只在一台笔记本上测过 85k 个提交。"

---

## 1. 和原文的建议冲突吗？

### 1.1 原文到底在对谁说话

原文 "Contributing to the wider ecosystem" 一节的建议，**收件人是 git 服务端、git 托管平台（forge）和 git 客户端**：

- "A git server doesn't have to work the same way as a git client does"
- "Git platforms can provide comprehensive server-side RPCs like Scion, perhaps even in a standardized way across forges **that the git client can learn to talk to**"
- "The negotiation protocol doesn't have to look exactly like it does today"
- "the various sparse and shallow features in git can be made to compose well"
- "so that we can partner with GitHub, GitLab, and the OSS community"

这些话里**没有一句**是"再建一个第三方层"。Flux 实际做的是：一套新的 HTTP API、一个新 CLI、一个新的 MCP server、一种新的对象线格式，外加自己的存储。严格说这是"又一个系统"，只是部署方式不需要迁移。README:42-45 写 "Flux follows that advice"，这是**过度解读**。准确的说法是：Flux 用一个独立服务**演示**了原文建议 forge 去做的事。

### 1.2 Flux 不该存在：最强的论证

1. **决定权不在 Flux。** 原文要的是"跨 forge 标准化、git 客户端能直接调用"的 RPC。这种标准只会在 git 邮件列表（protocol v2 新命令）或 GitHub、GitLab 的 API 里诞生，不会出自一个第三方 Rust 项目写的 1568 行 REST 规范。Flux 的 spec 在标准化层面没有分量，自己也承认 §9 "Nothing here is implemented"。
2. **时间窗口对 Flux 不利。** 原文说 6–12 个月内问题会普遍出现，forge 有同样的时间表。一个需要运维、有状态、只有单机的服务，等它成熟（多租户鉴权、HA、GC），forge 很可能已经补上了 blame、merge-base、path log 这类 RPC。GitLab Gitaly 早就有 `raw_blame`、`FindMergeBase`；GitHub 有 GraphQL blame、compare 和官方 MCP server。
3. **对象协议这一半在和 git 自己赛跑，而且赢得不多。** 对 git 的最佳配置，checkout 只快 2.5×（benchmarks.md:29-30）。为这点收益，用户要多运维一个服务，服务端存储放大约 15 倍（§4.1）。git 的 bundle-uri、partial clone、sparse 一旦组合得更好（这正是原文点名要修的），这部分优势就会消失。
4. **会分散力量。** 如果真想推动生态，把 `docs/upstream/git-sparse-promisor.md` 发到 git 邮件列表（design.md:227 写着"尚未发送"），或者给 Gitaly 提一个 `landed`/first-parent blame 的 RPC，影响面都比单独维护一个服务大。

### 1.3 Flux 应该存在：最强的论证

1. **原文本身就给了先例。** "Early in the migration, before we had our own client, repo format, or protocols, Scion only answered these 'metadata' queries." 一个只回答查询的旁路服务，恰好是 Scion 的第一阶段。Flux 复刻的就是这条路径。
2. **"能跑的代码"比提案更容易说服 forge。** git 邮件列表和 forge 都不会凭一篇博客去实现 `landed`、`bisect-step` 或 want-only 取对象。一个有差分测试（以 git 为 oracle）、有规范、有实测数据的参考实现，是推动标准化最有效的工具。
3. **有些团队等不了 6–12 个月。** agent 对 forge API 的调用会撞上速率限制（GitHub REST 每个 token 每小时 5000 次，还有 secondary limit），也撞上 blame 这种昂贵接口的超时。一个自托管、可缓存的查询层能把 agent 流量从 forge 身上卸掉。这个痛点是真实的，而且**今天就有**。
4. **`landed` 这类查询，git 确实没有原语。** 我实测过：`git rev-list --first-parent --ancestry-path X..HEAD` 对不在主干上的提交返回空结果，只能像 `bench/run.py:181-187` 那样用两次 rev-list 做交集（实测 73–100 ms）。Flux 用 skip list 做到 7 ms，这是实打实的能力差异，不是 benchmark 技巧。

### 1.4 我的判断

**作为"独立部署的新系统"来卖，它和原文的建议相冲突；作为"给 git/forge 的参考实现 + 过渡期旁路服务"，它和原文高度一致。** 区别全在定位和措辞上，见 §5。

---

## 2. 未来 6–12 个月，谁会真的部署它？

### 2.1 现实的采用者画像（按可能性排序）

| 画像 | 为什么会用 | 为什么会放弃 |
|---|---|---|
| **A. 中大型公司（200–2000 名工程师）的 DevProd/平台团队，有一个热点 monorepo**（历史 1–20 GB，每天几百到几千次 agent/CI 运行），自托管或使用 GHES/GitLab | 单仓库意味着单一信任域，Flux 的"一个 token 读全部"可以接受。MCP 查询能直接接入内部 agent。`landed` 和 bisect 正好对应 post-merge 的质量流程 | 见 §2.2 全部阻碍。最致命的三条是存储放大、单机 SQLite 无 HA、没有在大仓库上验证过 |
| **B. agent 沙箱或 CI 平台厂商**（托管 coding agent、远程 CI runner 一类） | 每天为客户仓库起成千上万个一次性沙箱，clone 成本就是他们的成本 | **多租户是硬性要求**。Flux 的对象存储跨仓库共享，`/v1/objects` 对任何持 token 者返回任何对象（spec §13.2 #10），这一条就直接出局 |
| **C. 个人或研究者**，想给 agent 接一个历史查询的 MCP | 安装后马上能用 | 他们已经有本地 git 和 GitHub 官方 MCP server，没有理由多跑一个服务 |

**实话：** 6–12 个月内，现实的生产用户最多是**个位数的 A 类团队**，前提是 §3.2 的缺口补上。B 类在 v2 协议（按仓库划分对象作用域）出来之前用不了。C 类是 star 的来源，但不是用户。

### 2.2 阻碍清单（A 类团队安全评审和运维评审时会问的问题）

**运维负担**

- **又一个有状态服务。** 只能单机运行：元数据在 SQLite 里，同步锁在进程内（design.md:21），没有 HA，没有备份恢复方案。server 挂了，所有查询都挂，只有固定到完整 commit id 的查询能从缓存层拿到。
- **存储放大约 15 倍**（§4.1）。git.git 在服务端占 4.8 GB，同一组对象打包后只有 313 MiB。按这个比例，一个 20 GB 历史的 monorepo，服务端要几百 GB 的 loose 文件，以及上千万个 inode。
- **只增不减。** 没有 GC（design.md:203），缓存层也没有 LRU。
- **首次 ingest 很慢。** git.git 要 173 秒（benchmarks.md:76）。在 Linux 内核或更大的仓库上没有测过；按提交数线性外推是小时级，而且没有文档说明中断后能否续传。
- **部署材料几乎为零。** 没有 Dockerfile、Helm chart、systemd unit，没有配置文件（仓库只能用重复的 `--repo NAME=URL` 命令行参数传入，main.rs:296-316），没有 Prometheus 指标（只有缓存层的 `/v1/stats`），没有 TLS（spec §11 要求使用者自己加反向代理）。

**鉴权与安全评审**

- **一个 token 能读整台 server 上的全部仓库。** 缓存层只是把这件事再转发一次：它用一个 `--upstream-token` 代表所有客户端（cache.rs:39-42），没有用户身份，没有审计日志，也说不出"谁读了什么"。
- **对象命名空间跨仓库共享。** 只要知道 oid，持有 repo A 权限的人就能读到 repo B 的对象（server.rs:282-295，spec §13.2 #10）。
- **访问源站用的是 server 进程所在主机的 git 凭据**（git_source.rs 直接调用 `git fetch`）。这实际上是一个能读所有镜像仓库的服务账号，没有 GitHub App 或 GitLab deploy token 的接入说明。
- **和 forge 的权限不一致。** 有人在 GitHub 上被移出团队，Flux 不知道，也无从知道。安全团队第一个问题就会是这个，而 research/03:130 承诺的"MVP 继承源站 token 的仓库级权限"并没有实现。
- **泄露的密钥删不掉。** 有人把密钥推上去，再 force-push 清掉，Flux 会**永远**保留并继续提供那个提交和它的对象：spec:58 写明 "Changesets are never deleted… stays queryable by id"，对象存储只增不减，固定查询在各层缓存里带着 `immutable` 头保存一年。没有 purge 工具。任何安全团队都会因为这一条否决。
- **`POST /sync` 对任何持 token 者开放**（spec §11）。客户端可以让 server 不停地去源站 fetch，打到 forge 的速率限制。

**数据新鲜度与一致性**

- 默认 30 秒轮询一次（main.rs:58-60），没有 webhook。每个仓库每 30 秒做一次完整的 `git fetch`；仓库一多，对 forge 也是负担。
- **读不到自己刚写的东西。** agent 刚 `git push`，紧接着问 `landed` 或 `log`，会拿到旧答案。`last_sync` 只在 `/api/v1/repos` 里有（design.md:173），单个查询的响应里没有"这个答案对应哪个 ref 状态"，也不能"等到 ref 至少到达 X 再回答"。spec §11 自己也写了 query 响应不可自证。
- **缓存失效。** 固定查询号称"永不改变"，但答案其实还取决于**算法版本**：
  - blame 的 derived 键是 `("blame", commit+path)`，不含算法版本（blame.rs:18、186）；
  - 缓存层的键是 `sha1(path?query)`，也不含协议或实现版本（cache.rs:260）。
  - research/04 的方案 B（换成 imara-diff）、方案 C（linelog）一旦落地，blame 的输出就会变。到那时新旧结果会在 SQLite 和各层缓存里混在一起，并且永远不会失效。spec 的 #26 已经因为这个问题出过一次事。

---

## 3. 什么做多了，什么缺了

### 3.1 对 MVP 来说过度建设的部分

| 项 | 问题 | 建议 |
|---|---|---|
| **1568 行、RFC 2119 风格的协议规范**，带 conformance class | 没有第二个实现，没有用户，而且 v1 还没冻结（spec 头部）。规范越正式，"draft 期间答案变了"就越难看 | 降级为 "API reference (unstable)"。等有第二个实现（比如一个 Gitaly PoC）再谈 conformance |
| **多级缓存串联**（region → node） | 没有任何用户有 region 拓扑；文档里的 mermaid 图（README:110-118）把一个从没在真实网络上测过的东西（benchmarks.md:117-118）当成卖点 | 保留单层缓存，删掉"可多层串联"的宣传 |
| **固定查询的永久缓存** | 没有版本化，正确性隐患见 §2.2 | 键里加上 `impl_version`，或者在 v1 冻结前别用 `immutable`，改用 `max-age` |
| **`bisect` 端点和 `flux bisect`** | 小众功能；真正的 bisect 还是得在 checkout 里跑测试 | 保留 API，从 README 首屏拿掉 |
| **手写 git index**（index.rs，v2/v3 + `sdir` 扩展） | 风险面最大：直接写 git 的磁盘格式，`index.rs` 自己**没有一个单元测试**，只靠 e2e 间接覆盖。git 升级改动 index 扩展、出现一个 bug，就会损坏用户的工作区 | 保留，但加上 git 多版本矩阵测试；同时把 sparse+promisor 的问题发给上游，争取最终不用自己写 |
| **路线图 v0.3 VFS、v0.4 "Flux 作为权威源"**（design.md:240-241） | v0.4 就是 research/03 说的选项 B，正好是原文劝阻的"新系统"。公开路线图里写它，会让读者认定 Flux 的终极目标是替代 forge | 从公开路线图删掉，或者明确标成"不打算做" |
| **logo 和 lockup 设计**（assets/logo/ 下 5 个 SVG）、徽章 | 无害，但一个 v0.1、没有用户的项目先做品牌，会强化"营销先于工程"的印象 | 无所谓，但先改名（§4.4） |
| **3 份 review、4 份 research，全是中文** | 这些是内部过程文档。英文读者从 README 点进去，看到的是中文 | 移到 wiki 或 `docs/internal/`，README 不再链接 |

### 3.2 阻碍第一个真实用户的关键缺口（按优先级）

1. **存储效率。** loose 对象没有 delta，比 pack 大 15 倍。最低限度：对象存储支持 pack（或者直接复用 mirror 的 pack，加上 `git cat-file --batch`），loose 只作缓存层的格式。**这是 server 端唯一的硬伤，而 README 把它写成了优点。**
2. **按仓库鉴权。** 最小可行做法：用调用者自己的 forge token 去问一次"能否读这个仓库"（结果缓存几分钟），再把对象路由改成按仓库划分作用域（`/v1/repos/{repo}/objects`，必须带上它所属的 repo）。没有这一步，Flux 只能部署在单一信任域里，README 必须用醒目的方式写清楚。
3. **purge 能力。** 一条 `flux admin purge <oid>` 命令，加上缓存层的失效广播（或者干脆规定泄密时清空缓存）。安全评审一定会问。
4. **新鲜度。** 支持 webhook 触发同步；每个查询响应带上 `x-flux-synced-at` 和相关 ref 的 oid；支持 `?min_ref=main@<oid>`，未达到时返回 409 或等待，以支持"读到自己刚写的"。
5. **在大仓库上验证。** research/03:132 自己定的 MVP 成功标准是：在 ≥10 GB 历史的仓库上，sparse checkout 比 git 最佳组合快 ≥10 倍，查询 p99 < 100 ms。**这几条一条都没测，文档也没提。** 至少应在 linux.git 或 chromium 上跑一次 ingest、存储和查询，结果不好也要公开。
6. **agent 真正需要的东西。** agent 最常用的操作是 **grep 或代码搜索**，其次是带行号范围读文件、跟踪重命名的 log 和 blame（`--follow`）。MCP 里这些都没有（mcp.rs:14-38），所以 agent 想搜代码还是得 clone，"不用 checkout"的卖点只成立一半。一个按 rev 的 `flux_grep`（哪怕是服务端暴力扫一个 tree）比 bisect 重要得多。
7. **打包和分发。** crates.io 上 `flux`、`flux-core` 两个名字**都已被占用**（下面 §4.4），所以 `cargo install` 都做不了。没有 release 二进制，没有容器镜像。
8. **运维基础。** Prometheus 指标（同步延迟、ingest 积压、各端点 p99、存储量）、配置文件、readiness（同步落后时返回非 ready）、SQLite 备份说明。

---

## 4. 公开发布的可信度风险

### 4.1 最致命：server 存储量没有披露，而且被写成了优点

在 benchmark 留下的 git.git 数据目录（`scratchpad/bench/data`）上实测：

| | 大小 |
|---|---:|
| Flux 对象存储 `data/objects`（421,115 个 loose 文件） | **4.7 GB** 磁盘占用（逻辑大小 3.8 GB） |
| Flux 元数据 `meta.sqlite` | 144 MB |
| Flux mirror（README 和 benchmarks 宣传的数字） | 5.3 MiB |
| **同一组对象（`--branches --tags`）用 git pack-objects 打包** | **313 MiB** |
| 上游 mirror 整体（含 refs/pull/*，85 万个对象） | 601 MiB |

- README:133-134 写着 "**Lean server.** The git mirror keeps refs only"，benchmarks.md:78 加粗了 "5.3 MiB"。可真正的存储是那 4.7 GB，**大约是 git pack 的 15 倍**。design.md:23 只轻描淡写地提了一句"完整的对象集"。
- git 邮件列表的读者五分钟就能算出这个数字，然后整个项目的诚信都会被质疑，包括那些本来诚实的部分（冷 blame 更慢、Corrections 一节）。
- **修法**：在 benchmarks.md 的 "Ingest and footprint" 里如实列出这张表；删掉 "Lean server" 这个说法；路线图把 pack 存储列为 P0。

### 4.2 HN 和 git 邮件列表会攻击的其他说法

| 位置 | 说法 | 会怎么被攻击 |
|---|---|---|
| README:9 | "**Agent-scale** source control on top of git" | "你的最大测试仓库是 85k 个提交、300 MB 的 git.git，在一台笔记本上跑；Scion 处理的是 100 GB。agent-scale 在哪？" benchmarks.md:112-116 自己承认"these benchmarks do not demonstrate that" |
| README:42-45 | "That post recommends… and Flux follows that advice." | 原文建议 forge 和 git 去改，不是建议再造一个服务（§1.1）。这还会被读成借 Anthropic 的名气 |
| README:40 | "checks **every answer** against git itself" | spec §13.2 #13 自己列了一串没对比的项目（generation、tree、file、CommitInfo、patch 文本、bisect 每一步的字段、HTTP 层）。blame 一致率 98.8%，而且不跟踪重命名 |
| README:130-132 | pinned query "can **never** change its answer… caches keep it forever" | spec 头部说 v1 没冻结，答案可能变；#26 已经变过一次；research/04 的方案 B、C 还会再变（§2.2） |
| README:166 | "**Independent** reviews against the Scion article" | 这些 review 是同一个工作流里的 AI agent 写的（reviews/03:4、reviews/02:5 都留着 `scratchpad/` 路径）。叫"independent"会被当成刷信誉。改成 "Internal design reviews (AI-assisted)" |
| README:148 | "History to download before the first query: 5.5 s, 117 MiB vs **none**" | 只计客户端，不计 server：Flux 那边是 173 s 的 ingest 加 4.8 GB。更关键的是，**"不 clone 就能查历史"真正的对手是 forge API**（GitHub GraphQL blame、compare 的 merge_base，GitLab 的 blame 和 merge_base API），而 benchmark 完全没和它们比 |
| README:153 | "blame, warm 6×" | 这是拿"读已持久化的结果"去比"git 现场计算"。如果 `FLUX_SERVER` 指向缓存层，pinned 的 blame 就是读一个磁盘文件。不算错，但应写明"memoized vs recomputed"，并拿 GitLab/Gitiles 的 blame 缓存做参照 |
| benchmarks.md:7-9 | 3 次取中位数，单机 | 没有方差、没有冷热区分说明，Flux server 和客户端在同一台机器上。改成 ≥10 次并报告 p50/p90 |
| research/03:117 | "checkout 快两个数量级，基础设施缩小 200 倍" | 这是 Scion 的数字。如果被引用到对外材料里，就是把别人的结果当成自己的。确认对外文档里不出现 |

**做对了、应该保留的地方**：冷 blame 慢 7 倍写在了首页表格里（README:154）；Corrections 一节主动承认了之前 4.8× 和 12× 的错误（benchmarks.md:90-108）；`landed` 的 13× 是真实能力差异（§1.3 第 4 点我实测核对过）。这些诚实的披露是这个项目最好的资产，所以 §4.1 那个隐瞒才更伤。

### 4.3 文档和流程上的小瑕疵（会被当成"粗糙"的证据）

- CONTRIBUTING.md:3 写的是"§7 Invariants"，实际在 design.md §8。
- spec:1531 的 #21 标着 "Resolved"，后面的正文却还在描述 bug 存在时的行为，前后矛盾。
- 英文 README 多处链接进中文文档（README:52、164-166、176）。
- 仓库目前**没有任何 commit**（`git log` 报 "does not have any commits yet"），可文档反复提到"earlier version""first published version"。公开时最好带上真实历史，或者在 Corrections 里说明那些旧版本在哪里能看到。
- 测试只有 9 个测试函数。`graph`、`blame`、`meta`、`index`、`cache`、`server` 都没有单元测试，全靠一个 129 秒的大差分测试和一个 e2e 覆盖。出了问题很难定位，外部贡献者也很难上手。

### 4.4 命名与商标："Flux" 基本不可用

| 冲突对象 | 严重程度 |
|---|---|
| **FluxCD**：CNCF 毕业项目（2022 年），GitOps 工具，CLI 二进制**就叫 `flux`**，用户就是 Kubernetes 平台团队 | **极高**。Flux 的目标用户（平台/DevProd 团队）机器上很可能已经装了 FluxCD 的 `flux`，PATH 直接冲突。"Flux + git" 的搜索结果全被 FluxCD 占据（它的口号就是 "flux puts the git into gitops"） |
| **FLUX.1**（Black Forest Labs 的图像生成模型） | 高。"Flux + AI" 的搜索结果被它占据 |
| **InfluxData Flux** 查询语言 | 中。Homebrew 核心库里的 `flux` formula 就是它（实测 formulae.brew.sh） |
| crates.io 上的 `flux`（v3.0.1）和 `flux-core`（2026-05 还在更新） | **阻断**：两个包名都已被占用，无法发布 |
| 其他：Flux.jl（Julia 机器学习库）、Facebook 的 Flux 前端架构、f.lux | 低到中 |

CNCF 项目的商标一般由 Linux Foundation 持有；我没有查到 "Flux" 在软件类别的注册信息，需要上 USPTO 核实。但即使法律上没问题，**可发现性和 PATH 冲突**这两条已经足够要求改名。Cargo.toml 里的 `github.com/flux-vcs` 目前不存在（返回 404），还来得及换。

### 4.5 "inspired by Anthropic" 的关联风险

- **被读成蹭热度。** 文章发布后，标题里带 "Scion"、"Anthropic" 的项目会被集中审视。README 首屏就提 Anthropic，加上"follows that advice"，容易被理解为 Anthropic 背书。
- **AI 生成的痕迹。** 文档风格统一、review 由 agent 生成、提交历史为空。HN 读者会推断"这是 Claude 在一个下午写的"，然后用这个视角挑刺。这本身不是问题（原文就在讲用 Claude 写基础设施），但**必须主动说明**，不能被人"发现"。
- **建议**：README 里只提一次 Anthropic，并加一句 "Not affiliated with or endorsed by Anthropic"。把"受 Scion 启发"放到 "Background" 小节，不放首屏。可以在 CONTRIBUTING 或 README 里坦白写一句开发方式（AI 辅助，以 git 为 oracle 做差分测试），这反而是加分项。

---

## 5. 公开前该改什么（按优先级）

### P0：不改就别发

1. **改名。** 避开 FluxCD、FLUX.1 和 crates.io 上已被占用的名字。选一个能拿到 crates.io 包名、GitHub org，并且不和常见 CLI 冲突的名字，并在 Cargo.toml:9 同步修改。

2. **披露存储放大，并删掉 "Lean server"。** 修改 README:133-134、benchmarks.md:72-78，加上 §4.1 那张表；路线图把"pack 存储"放在第一位。

3. **重写首屏定位**（README:8-11、30-45）。建议的一句话：

   > **A read-only query server and object cache for git: CI jobs and agents ask log, blame, merge-base and "which merge landed this?" over HTTP or MCP, without cloning. A working prototype of the server-side RPCs that git forges could standardize.**

   中文：**给 git 用的只读查询服务和对象缓存：CI 和 agent 不用 clone，就能通过 HTTP 或 MCP 问 log、blame、merge-base 和"哪次合并带进来的"。它也是 forge 可以标准化的服务端 RPC 的一个可运行原型。**

   同时把 "Agent-scale source control" 删掉，改成能被证明的说法。

4. **醒目写出部署前提**（放在 README 的 NOTE 块里，而不是 design §1）：
   - 单一信任域：任何 token 持有者都能读所有仓库和所有对象，不继承 forge 权限；
   - force-push 删掉的内容**仍然可以通过 Flux 读到**，并且没有 purge；
   - 只有单机，没有 HA；数据最多滞后一个轮询周期。

5. **修正过度声明**：README:40（"every answer" 改成"each query endpoint, with known gaps listed in spec §13"）、README:130-132（"never change" 改成"immutable within a protocol version; v1 is a draft"）、README:166（"Independent" 改成 "Internal, AI-assisted"）、README:42-45（改成 "a prototype of what the post asks forges to build"，并加上 not affiliated 声明）。

### P1：发布时就该有，否则第一批试用者会卡住

6. **给固定查询的缓存加版本。** derived 键和缓存键都要带上 `impl_version`（blame.rs:18、cache.rs:260）。或者在 v1 冻结前改用有限的 `max-age`。必须在 research/04 方案 B、C 落地**之前**完成。
7. **benchmarks 补一个 forge API 对照组**（GitHub GraphQL blame、REST compare 和 commits?path=），以及 server 端的成本（ingest 时间、存储、内存）。在 linux.git 上至少跑一次，结果不好也公开。
8. **在 MCP 里加 `flux_grep`**（按 rev 搜索），并让读文件支持行号范围。这比 bisect 更决定 agent 能否真的"不 checkout"。
9. **每个查询响应带同步水位**（`x-flux-synced-at` 和 ref oid）；支持 webhook 触发同步。
10. **最小部署材料**：Dockerfile、一个配置文件示例、Prometheus 指标、"放在 TLS 反向代理后面"的示例。

### P2：缩减范围，让项目看起来更聚焦

11. 把 spec 降级为 "API reference (unstable)"，删掉 conformance class 一节，或者加一句"等第二个实现出现再正式化"。
12. 从公开路线图删掉 v0.3 VFS 和 v0.4"权威源"（design.md:240-241）。替换成"上游化"路线：把 sparse+promisor 的报告发到 git 邮件列表（design.md:227）；写一份 `landed`/`object-get` 作为 protocol v2 命令的 RFC 草稿；给 Gitaly 提一个 first-parent blame RPC 的 issue。**这一条最能回应"你为什么不去改 git/forge"的质疑。**
13. README 首屏只保留三件事：不 clone 的历史查询、MCP、sparse checkout。多级缓存、bisect、固定查询缓存移到 "How it works" 后面。
14. 把中文的 research 和 reviews 移出 README 的链接路径，至少把 design.md 翻译成英文。
15. 修正小瑕疵：CONTRIBUTING.md:3 的 §7 改为 §8；spec:1531 的 #21 前后矛盾；给 `index.rs` 补单元测试，并在 CI 里跑多个 git 版本。

### 发布方式建议

- **先发 git 邮件列表，后发 HN。** 先把 sparse+promisor 的最小复现发到 git@vger.kernel.org，附上 Flux 作为背景链接。拿到上游的反馈（哪怕是批评），再发 HN，标题用 "Show HN: a read-only history query server for git (prototype of Scion-style RPCs)"，不要用 "Anthropic"。
- **在 README 里明确"成功的标准"**：如果 GitHub 或 GitLab 实现了等价的 RPC，Flux 就完成了使命。这句话能消解"你在和 forge 竞争"的质疑，也和原文的精神一致。

---

## 附：本评审引用的外部来源

- Anthropic, [Redesigning source control for agent-scale development](https://www.anthropic.com/engineering/redesigning-source-control)："Contributing to the wider ecosystem" 和 metadata 查询两段原文
- FluxCD：[fluxcd.io](https://main.docs.fluxcd.io/)，[CNCF 毕业公告](https://www.cncf.io/announcements/2022/11/30/flux-graduates-from-cncf-incubator/)，[flux puts the git into gitops](https://fluxcd.io/blog/2022/03/flux-puts-the-git-into-gitops)
- Gitaly 的 blame 和 merge-base RPC：[gitaly#376](https://gitlab.com/gitlab-org/gitaly/issues/376)，[gitaly#799](https://gitlab.com/gitlab-org/gitaly/issues/799)，[gitlab#525103](https://gitlab.com/gitlab-org/gitlab/-/issues/525103)
- crates.io API：`flux` 3.0.1、`flux-core` 0.5.2 都已被占用；Homebrew `flux` formula 是 InfluxData Flux（formulae.brew.sh）
