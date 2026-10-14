# Review 04 · 定位与原文意图：Flux 该为谁做、以什么形态做

> 视角：战略定位。前三份审阅核对的是"Flux 有没有忠实实现 Scion 的机制"，这一份问的是另一件事：**按原文作者真正想要的结果，Flux 这个载体对不对。**
> 方法：
> 1. 逐句核对用户给出的"原文意图"表述，引文来自原文 [Redesigning source control at Anthropic](https://www.anthropic.com/engineering/redesigning-source-control)（2026-10-13 发布）。
> 2. 阅读本仓库 README、00-overview、design、benchmarks、research/01–04、reviews/01–03、spec v1、upstream 草稿。
> 3. 网络调研现有的重叠方案。
>
> 只新增本文件，没有修改其他任何文件。

---

## 结论（先看这里）

1. **用户的理解大方向正确，但有三处超出原文。**
   - 原文**没有**说"不开源 Scion"，只说"很难提供一个对别的组织普遍有用的形态"。
   - 原文**没有**说"git 服务端可以完全重新设计"。它的约束恰恰相反：**"without departing from the overall model"**（不脱离 git 的整体模型）。
   - 原文**没有**"为人类局限打的补丁都要重建"这层意思。它的语气是尊重 git 的："git 社区花了近二十年为数百万开发者扩展这个平台。"
   - 用户的表述**漏掉了两条**原文点名的建议：git 的 sparse 与 shallow 特性要能良好组合；"git 客户端能学会调用"的标准化 RPC。这两条恰好是 Flux 现在最弱的地方。
2. **"git 兼容的只读加速层"这个方向是对的，"自托管产品"这个载体是错的。**
   - 原文点名的受众是 **forge（GitHub、GitLab）、git 上游和 OSS 社区**，不是"再多一个要部署的服务"。
   - 原文要的是"**跨 forge 标准化、git 客户端能学会调用**"的服务端 RPC。一个独立的第三方 Rust 服务，无论做得多好，都不会被 GitHub/GitLab 直接部署，git 客户端也不会去调它。
   - Flux 真正能被 forge 和上游吸收的，是**协议规范、以 git 为 oracle 的一致性测试套件**，以及若干**可以提交上游的具体发现**。
3. **真正的差异化只有窄窄几条，其余大多重复。**
   - 差异化：
     - "固定到完整 commit id 的查询永久可缓存"这一语义，加上同构缓存层；
     - `landed`、`bisect` 这类 forge 普遍没有标准化的主线原语；
     - 以 git 为 oracle 的差分一致性测试；
     - 面向 agent、与 forge 无关的统一 API 和 MCP。
   - 重复：
     - want-only 取对象，与微软 GVFS 协议的 `POST /gvfs/objects` 加 cache server 几乎同构；
     - checkout 加速只有 2–3×；
     - 查询速度在 git 打开 commit-graph 后大多只领先个位数倍，冷 blame 还更慢。
4. **最有价值的单一交付物**：一份面向 forge 的**《git 源码控制查询与对象 RPC 规范》+ 可以对任意实现运行的一致性测试套件（git 作 oracle）**，Flux server 降级为它的参考实现。同时把已经写好却**尚未发送**的 sparse/promisor 问题报告发到 git 邮件列表。
5. **建议砍掉**路线图 v0.3（FUSE 虚拟化 checkout）和 v0.4（Flux 作为权威源、服务端提交）。v0.4 本质上是在造"公开版 Scion"，正是原文明确说"不会真的有用"的那条路。

---

## 1. 原文意图逐句核实

### 1.1 核对表

| 用户表述 | 判定 | 原文依据 |
|---|---|---|
| Anthropic **没有计划开源** Scion 来取代 Git | ⚠️ **推断，原文未明说** | 原文没有任何一句提到开源或不开源 Scion 代码（经两次全文检索确认）。原文说的是："Practically speaking, it's difficult to offer Scion in a form that would be broadly useful to other organizations." "Our internal versions of these components are highly bespoke" "we worry that attempting to build a public version that tries to please everyone wouldn't actually be very useful." 以及 "Scion is by no means a suitable replacement for git for general use." 结论：**"不打算让 Scion 取代 git"有原文支撑；"不开源"是合理推断，不是原文陈述。** 部分媒体写成 "Anthropic has also confirmed Scion will not be open-sourced"（见 §3 来源），这是二手夸大。 |
| 设计绑定**超大 monorepo、海量并发 agent** | ✅ 成立 | "The concepts we've outlined here really only make sense for truly enormous monorepos, with either tens of thousands of engineers or swarms of agents prolifically writing code." 规模数据：日合入从 2,500 涨到 30,000+；main 历史约 100 GB；CI checkout 约 2,500 万次/天。 |
| **token 消耗**远超普通公司 | ✅ 成立（语境略有不同） | "Plus, Anthropic is unique in that our ability to spend tokens for internal use far exceeds what any other business would realistically want to spend. We do this both to address our own software development needs, and to test Claude's capabilities." 这句是"为什么 Scion 不适合别人"的理由之一。它说的是 Anthropic **愿意花的** token 量独一无二，并没有说 token 消耗是 Scion 的设计约束。 |
| 对大多数普通开发，**Git 依然优秀** | ✅ 成立 | "For the vast majority of repositories, this is the right design."（指 git 的离线、完整副本设计。） 还有 "the git community has spent nearly two decades scaling the platform for millions of developers." |
| **6–12 个月内**依赖 AI 写代码的其他团队会撞上同一堵墙 | ⚠️ **基本成立，但"同一堵墙"说过头了** | 原文："we anticipate that this class of problems will become more commonplace over the next six to 12 months"。原文说的是"**这一类问题**会更普遍"，没有说"**其他团队会撞上 Anthropic 那堵墙**"。同一段紧接着说 Scion 的概念"只对超大 monorepo 才有意义"。所以原文的逻辑是：问题会普及，但大多数团队的解法应该是**改进 git**，而不是上 Scion 式系统。 |
| 发布设计是为了给开源社区、GitHub、GitLab 提供思路 | ✅ 成立 | "the most impactful thing we can do is to share our thinking, so that other developers can apply some of the most successful parts of Scion to git without departing from the overall model" 以及 "so that we can partner with GitHub, GitLab, and the OSS community to support the changing needs of developers." |
| git 的服务端可以**完全重新设计** | ❌ **措辞过度** | 原文："A git server doesn't have to work the same way as a git client does; novel server architectures exist"。原文说的是服务端**不必和客户端同构**，前提是"**without departing from the overall model**"。"完全重新设计"会把读者引向原文明确劝阻的方向。 |
| 更高效的 **RPC 查询机制** | ✅ 成立 | "Git platforms can provide comprehensive server-side RPCs like Scion, perhaps even in a standardized way across forges that the git client can learn to talk to." 注意两个修饰语：**跨 forge 标准化**、**git 客户端学会调用**。 |
| 更灵活的**协商协议** | ✅ 成立 | "The negotiation protocol doesn't have to look exactly like it does today, either," |
| 而不是遵循几十年前为**人类手写代码**定的规则 | ❌ **原文没有这个意思** | 最接近的说法是引言里的 "developer infrastructure built for human scale"。原文对 git 设计假设的描述是"**客户端离线、需要完整副本**"："Git assumes that clients work offline and need a complete copy of the repository." 原文批评的是"离线分布式"假设，不是"为人类手写代码"。另外 git 诞生于 2005 年，是"近二十年"，不是"几十年"。 |
| AI 编码速度指数上升，**所有为人类局限做的补丁都必须重建** | ❌ **原文没有这个意思** | 最接近的是结尾那句 "We're all in it together as the software development lifecycle intersects with the AI exponential."，以及 merge queue 一节的 "We're completely rethinking our priors"。原文反而明确保留了余地："we don't expect our solution, or our considerations, to suit every case." |

### 1.2 用户表述漏掉的部分（对 Flux 定位影响最大）

1. **"without departing from the overall model"**。这是整个生态章节的约束条件。凡是需要用户离开 git 模型的路线（Flux 的 v0.4 "权威源 + 服务端提交"），都与原文的建议相反。
2. **"the various sparse and shallow features in git can be made to compose well."** 这是对 git **上游**提的具体要求。Flux 恰好已经撞到了一个实例（`docs/upstream/git-sparse-promisor.md`），但**草稿至今没有发出**。
3. **"…that the git client can learn to talk to."** 原文设想的是 **git 客户端本身**学会说新 RPC，而不是再引入一个第三方客户端（`flux` CLI）。
4. **迁移的第一阶段正是 Flux 现在的范围。** "Early in the migration, before we had our own client, repo format, or protocols … Scion only answered these 'metadata' queries"。这有两层含义：
   - Flux 的"只读查询层"是 Anthropic 验证过的**最先产生价值**的切入点；
   - 但 Anthropic 是把它当成自建系统的**过渡阶段**，不是终态。
5. **发布时间**：原文 2026-10-13 发布，与本仓库的立项是同一天。目前还没有任何 forge 或 git 上游的公开回应（见 §3）。窗口是真的，但也意味着**还没有人定义标准**，这是 Flux 唯一的先发机会。

### 1.3 修正后的意图表述（建议写进 00-overview §5）

> Anthropic 没有发布 Scion，也不建议别人照搬。它认为这类扩展问题会在 6–12 个月内变得普遍，但对绝大多数仓库，正确的回应是**在不离开 git 模型的前提下**改进 git 生态。具体有四点：服务端不必与客户端同构；forge 提供全面的服务端 RPC，最好跨 forge 标准化，让 git 客户端能直接调用；协商协议可以改；sparse 与 shallow 要能良好组合。它希望和 GitHub、GitLab、OSS 社区合作推动这件事。

---

## 2. Flux 的定位是否合理

### 2.1 Flux 能为谁服务（按团队规模）

| 画像 | 典型规模 | 痛点 | Flux 的价值 | 判断 |
|---|---|---|---|---|
| 小团队 | < 50 人；仓库历史 < 1 GB；agent/CI checkout 每天数百次 | 几乎没有。full clone 只要几秒，GitHub API 够用 | 负值：多一个要运维的服务、一份完整镜像、一个新的鉴权面 | **不是目标用户**。原文 "vast majority of repositories" 指的就是这一档 |
| 中大型团队（原文说的"6–12 个月内"人群） | 100–2,000 人，或少量工程师加大量 agent；历史 1–20 GB；每天上万次一次性 sandbox 或 CI checkout；托管在 GitHub/GitLab | 一次性 sandbox 反复 clone；history 查询需要本地历史；forge API 限流且语义各家不同；"这个提交什么时候进了 main"靠扫 log | **有真实价值**：不 clone 就能查历史、`landed`、`bisect`、MCP、节点级共享对象 | **唯一现实的目标人群**。但他们会先比较：blobless clone + bundle-uri + 节点缓存、forge 自带 API、Sourcegraph |
| Scion 级 | 上万工程师或成群 agent；历史约 100 GB；每天千万次 checkout | 一切 | v0.1 撑不住：单机 SQLite，一对象一文件的 loose 存储，权威层持有完整对象集（design §1） | **不是目标用户**。这一档会像 Anthropic、Meta、Google 一样自建 |

**一个必须正视的事实**：现在**没有任何证据**表明 Flux 在中大型仓库上的优势会扩大。

- 唯一的基准是 git.git（85k 提交），checkout 只快 2–3×，冷 blame 还慢 7 倍（benchmarks.md）。
- benchmarks.md 自己也写了 "That gap should widen with repository size, but **these benchmarks do not demonstrate that.**"
- 对目标人群来说，这句话就是整个价值主张中缺失的那一环。

### 2.2 自托管加速层的三个结构性障碍

这三点前三份审阅都没有从"谁来部署"的角度提过。

1. **鉴权模型不适合多租户。**
   - spec §2.1："The token is not repository-scoped. Any valid token grants access to every [mirrored repository]"。
   - 对中大型团队来说，私有仓库的权限必须跟随 forge（GitHub 的 team 和 repo 权限，GitLab 的 project membership）。
   - 在 Flux 能做到"按调用者在 forge 上的权限过滤"之前，平台团队只能每个权限域部署一套，或者干脆不部署。
   - **这是阻挡"自托管产品"路线的第一道墙，比性能更关键。**
2. **双份数据加一致性延迟。** Flux 要镜像整个仓库，权威层持有完整对象集；答案反映的是 `last_sync` 时刻的源站状态。这些问题 forge 自己实现时都不存在。
3. **对象协议的命运已有先例。**
   - 微软 VFS for Git 的 GVFS 协议有 `POST /gvfs/objects`（提交 OID 列表）、给缓存代理用的 loose-object 响应格式，以及专门的 cache server。这与 Flux 的 `POST /v1/objects` 加同构缓存层几乎是同一个设计。
   - 微软后来的结论是："Git will never adopt the GVFS protocol"，partial clone 才是 "the official version of that functionality"，于是 Scalar 转向上游（来源见 §3）。
   - Flux 如果把一个 git 之外的对象协议当作卖点，很可能重蹈覆辙。

### 2.3 三种载体对比

| | (i) 向 git 上游提案 | (ii) 面向 forge 的规范 + 参考实现 | (iii) 自托管产品（Flux 现状） |
|---|---|---|---|
| 与原文建议的契合度 | **最高**。"sparse/shallow compose well"、"negotiation protocol"、"git client can learn to talk to" 三条只有上游能兑现 | **高**。"standardized way across forges" 一条直接对应 | 中。原文的第一阶段做的正是这件事，但原文的受众不是终端部署者 |
| 采用路径 | 邮件列表、C 代码、长周期。成功后所有 git 用户都受益 | forge 照着规范实现（Gitea/Forgejo 最现实，GitLab 有 Gitaly，GitHub 最难），或由 Flux 提供 forge 适配器 | 平台团队自己部署、运维、解决鉴权 |
| 被 forge 自研"吃掉"的风险 | 不存在，这本来就是目的 | 低。被采纳就是成功 | **高**。GitHub/GitLab 内部早已有这些原语（§3） |
| Flux 现有资产能复用多少 | upstream 草稿、sparse index 实验结论、want-only 设计经验 | **几乎全部**：spec v1 草案、差分测试、参考实现、MCP | 全部，但还要补鉴权、多租户、存储后端、运维 |
| 成本 | 中，主要是人和时间 | 低到中 | **高**：v0.2–v0.4 路线图的工作量都在这里 |

**判断**：

- **(ii) 作为主线**。规范草案的开头已经写了 "written so that a git forge (GitHub, GitLab, Gitea, ...) or an independent implementation can serve the same API"，方向是对的，但目前它仍然只是"Flux 的规范"。
- **(i) 作为并行支线**。成本最低、最该马上做的一件事是把 sparse/promisor 报告发出去。
- **(iii) 降级**为"参考实现，顺便可以自托管"。不再以产品化（多后端、FUSE、权威源）为路线图主轴。

---

## 3. 现有方案的重叠调研

### 3.1 逐项对比

| 方案 | 覆盖了什么 | 与 Flux 的重叠 | Flux 是否有差异 |
|---|---|---|---|
| **git partial clone + cone sparse-checkout + sparse index** | 只取 cone 内 blob，按需懒拉 | Flux 的 `--git` checkout 本身就建立在这些机制上 | 很弱。同样的对象数（1,850 对 1,966），Flux 只赢在协议机制，2.6× |
| **commit-graph + changed-path Bloom filters** | 本地 history 查询加速 | 打开之后 git 的 merge-base 与 Flux **持平**，path log 只差 3.3× | 只在"不需要本地历史"这一点上有差异 |
| **bundle-uri**（git 客户端已支持；GitLab Gitaly 2025 年起作为实验特性，把 clone 卸载到对象存储或 CDN） | 批量对象走 CDN | 与 Flux 缓存层"把读流量从源站卸载出去"的目标重叠 | Flux 是按对象、可续传、可缓存，粒度更细；bundle-uri 已经进入 git 上游 |
| **promisor-remote 协议能力 / Large Object Promisors**（2025-03 合入 git，topic `cc/lop-remote`） | 服务端在协议里告诉客户端"某些对象去别的 promisor 取" | 这是 git 上游在做的"分层对象来源"，与 Flux 的 tier 概念同方向 | 这恰恰是 Flux 的对象层应该**接入**而不是**替代**的机制 |
| **protocol v2 `object-info` 命令** | 按 OID 查询对象属性（目前只有 size） | 先例：v2 已经接受"按 OID 的 RPC" | 如果要把 want-only 取对象推向上游，最自然的路径就是仿照它加一个命令 |
| **`git backfill`**（2.49）、**`git last-modified`**（2.52，来自 GitHub 内部的 blame-tree，GitLab 协助整理成可审阅的补丁） | 上游在持续增加对服务端友好的历史命令 | 说明 **GitHub、GitLab 正在把服务端需要的历史能力推进 git 上游** | 这正是原文想看到的路径，Flux 应该加入它，而不是绕开它 |
| **Scalar / microsoft/git / GVFS 协议** | `POST /gvfs/objects` 加 loose-object cache server | **几乎同构**于 Flux 的对象协议加缓存层 | 无本质差异。教训：git 之外的对象协议最终被搁置 |
| **GitHub API** | REST compare（返回 `merge_base_commit`）、按路径列提交；GraphQL `Commit.blame(path:)` | 覆盖 merge-base、log、blame | 有差异：GitHub 的 API 有限流、各家 forge 互不相同、没有"pinned 查询永久可缓存"的语义；我没有查到 GitHub 提供标准化的 `landed` 或 bisect 原语 |
| **GitLab REST + Gitaly gRPC** | REST blame、compare 和 merge-base；Gitaly 有 `CommitIsAncestor`、`FindMergeBase` 等 RPC | **Gitaly 本身就是"git 的服务端 RPC 层"**，原文原则 3 的 forge 内部版本 | 差异在于**标准化和对外暴露**，而不在于能力本身 |
| **Gitiles**（Google/Gerrit） | HTTP 上的 `+log`（JSON）、`+blame`、`+diff` | Google 早就有"不 clone、经 HTTP 查历史"的先例 | 差异小：Gitiles 是浏览器导向的，没有缓存语义，也没有对 agent 的设计 |
| **Sapling / Mononoke / EdenFS** | 完整的 Scion 式栈 | 设计目标重叠（derived data 等） | 不同赛道。Mononoke 开源版缺少生产级后端（Thrift API、生产 SQL、S3 blobstore 均未导出），是"把巨型服务端开源了也没人部署"的先例，正好印证原文对公开版 Scion 的担忧 |
| **Jujutsu (jj)** | 客户端 UX；后端可抽象 | 几乎不重叠 | jj 可以成为 Flux API 的**消费者**，不是竞争者 |
| **Gitoxide** | Rust 版 git 库；有 `gix-blame` crate | Flux 自己实现了对象解析、tree diff、blame | 无差异。值得评估复用 gix，以降低"与 git 语义一致"的维护成本 |
| **Sourcegraph MCP** | 企业版提供面向 agent 的搜索、提交历史、diff | 与"agent 不 clone 就能查历史"重叠 | 部分差异：Sourcegraph 是闭源企业产品，以搜索为中心；Flux 以 git 语义的正确性为中心，有差分验证 |
| **第三方 git MCP**（mcp-git-tools、gitlog-mcp、GitHub DeepBlame MCP 等） | blame、log、diff | 工具形态重叠 | 有差异：它们大多需要本地仓库，或者绑定 GitHub API |
| **对 Scion 文章的回应** | 原文今天（2026-10-13）才发布 | 没有找到 forge 或 git 上游的官方回应，也没有找到开源的 Scion 式项目；媒体报道普遍误读（"Anthropic ditched git"，"confirmed will not be open-sourced"） | **窗口期真实存在**，标准尚无人定义 |

### 3.2 小结

**Flux 重复造轮子的部分：**

- 对象传输协议（GVFS、partial clone、bundle-uri、LOP 都覆盖了这一层）；
- checkout 加速（2–3×，量级不足以让人迁移工具链）；
- 在维护良好的 git 仓库上做查询（大多是个位数倍，冷 blame 更慢）；
- 通用的 git MCP 工具。

**Flux 真正有差异的部分**（按价值排序）：

1. **以 git 为 oracle 的差分一致性测试。** 没有哪个 forge API 公开承诺"与 `git` CLI 输出逐项一致"，更没有可以复用的测试套件。这是 Flux 唯一能直接交给 forge 的资产。
2. **"pinned 查询永久可缓存"的语义，加上同构缓存层。** 所有 rev 参数都是完整 oid 的查询，答案永远不变，因此可以放进 CDN。现有 forge API 都没有把它标准化。这是对原文 "standardized way across forges" 最有内容的一条贡献。
3. **`landed` 和 `bisect_step` 两个主线原语。** 原文专门点名了"线性扫描 commit log 查合入时间"这个反面例子，以及自动 bisect/revert 流水线。forge API 没有统一提供这两个原语。
4. **与 forge 无关、面向 agent 的统一接口（HTTP、CLI、MCP）。** 同一套语义可以覆盖 GitHub、GitLab、Gitea。
5. **直接写 sparse index 的工程经验**，以及由此发现的 git 上游问题。这一条更适合作为**上游贡献**，而不是产品卖点。

---

## 4. 建议

### 4.1 保留、修改、砍掉

| 处置 | 项目 | 理由 |
|---|---|---|
| **保留** | 查询 API，尤其是 `landed`、`bisect`、path log、blame | 差异化来源，对应原文原则 3 和迁移第一阶段 |
| **保留** | 差分测试（git 作 oracle） | 最可移植的资产，应该升格为一致性套件 |
| **保留** | spec v1 草案、pinned 查询缓存语义、`/v1/capabilities` | 标准化的基础 |
| **保留** | MCP | 成本低，直接服务 agent 用户 |
| **保留**（降级为实验） | want-only 对象协议、`flux checkout --git` | 作为"协商协议可以怎么改"的**可运行论据**和上游提案素材，不作为主卖点 |
| **修改** | README 定位文案 | 见 §4.2 |
| **修改** | "Flux follows that advice" | 原文的建议包括 "git client can learn to talk to"，Flux 用的是自己的客户端。应改成"follows the first half of that advice"，并说明另一半要靠上游 |
| **修改** | 基准测试 | 必须补一个大仓库（linux.git，或 chromium 的公开镜像）的数据，证明或证伪"差距随仓库规模扩大"。没有这组数据，对中大型团队就没有说服力 |
| **修改** | 鉴权 | 在宣传"自托管"之前，至少做到按仓库授权，或者把调用者的 forge token 透传到源站校验。否则多仓库部署就是越权读取 |
| **修改** | 00-overview §5 对原文意图的表述 | 采用 §1.3 的修正版，删掉任何"完全重新设计"的暗示 |
| **砍掉** | 路线图 v0.3：FUSE/FSKit 虚拟化 checkout | VFS for Git 的教训是跨平台虚拟文件系统成本极高，最终退回 Scalar 加 partial clone。这与"标准化"主线无关 |
| **砍掉** | 路线图 v0.4：Flux 作为权威源、服务端 commit/rebase/merge | 这就是在造公开版 Scion，与 "without departing from the overall model" 直接冲突，也是原文说 "wouldn't actually be very useful" 的那条路。research/03 的选项 B 应从"长期演进方向"改为"明确不做" |
| **推迟** | v0.2 的 Postgres/Spanner 元数据、S3 对象后端 | 等到出现真实的部署方再做。现在做是为"产品"铺路，与主线无关 |

### 4.2 README 重写建议（英文，因为 README 对外）

当前标语 "Agent-scale source control on top of git." 暗示这是一个要部署的产品。建议改为：

> **Flux — a proposed standard for server-side git queries and want-only object fetch, with a reference implementation you can run in front of any git host today.**
>
> Anthropic's [Scion post](https://www.anthropic.com/engineering/redesigning-source-control) asks git forges to offer "comprehensive server-side RPCs … in a standardized way across forges that the git client can learn to talk to." Flux is a concrete, testable draft of that standard:
> - **Spec** — query API (log, blame, diff, merge-base, landed, bisect) and object protocol, with caching rules for immutable answers.
> - **Conformance suite** — point it at any implementation; git itself is the oracle.
> - **Reference server** — runs in front of GitHub, GitLab, Gitea or plain git, so you can use the API (and MCP for agents) before your forge implements it.
>
> Who it's for: teams whose CI jobs and agent sandboxes read history far more than humans do. If your repo clones in seconds, plain git is the right tool.

此外：

- 把 "Verified against git" 一行提到第一位；
- 把基准表下移，并标明"仅 git.git 规模"；
- 文末 "Documentation" 表第一行改为规范和一致性套件。

### 4.3 新路线图建议

| 轨道 | 内容 | 对应原文 |
|---|---|---|
| **A. 标准**（主线） | 1. spec 与 Flux 实现解耦，改名为与 forge 无关的名字，加入 object-format 协商（为 SHA-256 预留，git 3.0 会默认 SHA-256）。2. 把 `differential.rs` 抽成独立的 **conformance runner**：输入任意服务端 URL，输出逐项与 git 的对比报告。3. 在 spec 里加一节"与现有 forge API 的映射"：GitHub REST/GraphQL、GitLab REST、Gitaly RPC、Gitiles，说明哪些已有、哪些缺失 | "standardized way across forges" |
| **B. forge 适配**（验证规范可实现） | 先为 **Gitea/Forgejo** 写一个实现规范的适配（插件，或者代理它们的内部 API）。它们开源、社区驱动，是最可能第一个采纳的 forge。其次是 GitLab，Gitaly 已有大部分原语 | "partner with GitHub, GitLab, and the OSS community" |
| **C. 上游**（并行） | 1. **立刻**在最新 master 上复测，然后发出 `docs/upstream/git-sparse-promisor.md`。2. 起草 RFC：在 protocol v2 中仿照 `object-info` 增加"按 OID 取对象、响应可按对象缓存"的命令，与 promisor-remote/LOP 衔接，而不是另起协议。3. 评估把 `landed`（第一个包含某提交的主线提交）做成 git 命令或 `rev-list` 选项的可行性 | "negotiation protocol"、"sparse and shallow … compose well"、"git client can learn to talk to" |
| **D. 参考实现**（维护） | 只做规范和一致性所需的工作：按仓库鉴权、大仓库基准、冷 blame 优化（research/04）。不扩展存储后端，不做 FUSE，不做写路径 | — |

### 4.4 单一最有价值的交付物

**《git 服务端查询与对象 RPC 规范 v1》+ 一键运行的一致性测试套件（git 作 oracle）。** Flux server 是它的第一个合规实现。

理由：

1. **它就是原文点名要的东西。** 原文说 "the most impactful thing we can do is to share our thinking"，并希望 RPC "standardized across forges"。原文的受众（GitHub、GitLab、OSS 社区）需要的是**可以照着实现、可以验证对错**的东西，而不是一个要额外部署的 Rust 服务。
2. **它是 Flux 唯一别人没有的资产。**
   - 对象协议有 GVFS 和 partial clone；
   - 查询能力有 Gitaly、GitHub API、Gitiles；
   - 但"一份跨 forge 的语义定义，加上以 git 为 oracle 的自动化一致性验证"，目前不存在。
3. **它对被吸收的风险免疫。** 如果 GitHub 或 GitLab 自己做，最好的结果是它们实现这份规范，并用这个套件自证一致性。到那时 Flux 赢的是标准，而不是部署量。
4. **窗口期很短。** 原文今天发布，6–12 个月是原文给的时间。在 forge 各自定义私有 API 之前把规范和套件放到桌面上，Flux 才有机会；而产品化路线（v0.2–v0.4）在这个时间窗内拿不到有意义的部署量。

一句话：**Flux 应该从"又一个要部署的服务"转为"forge 和 git 上游可以直接采纳的规范、测试与证据"，参考实现只用来证明规范可行。**

---

## 来源

- 原文：[Redesigning source control at Anthropic](https://www.anthropic.com/engineering/redesigning-source-control)（Anthropic Engineering，2026-10-13）
- 媒体的二手报道及误读示例：[DEV Community](https://dev.to/alifar/anthropics-scion-shows-why-ai-agents-are-forcing-a-rethink-of-source-control-21bl)、[cryptobriefing](https://cryptobriefing.com/anthropic-scion-custom-source-control/)、[pasqualepillitteri.it](https://pasqualepillitteri.it/en/news/23527/anthropic-unveils-scion-source-control-replaces-git)、[alphasignal](https://alphasignal.ai/news/anthropic-ditched-git-and-built-scion-after-claude-caused-12x-merge-surge)
- GVFS 协议（`POST /gvfs/objects`、cache server 用的 loose 格式）：[VFS-for-Git Protocol.md 镜像](https://portrait.gitee.com/mirrors/VFS-for-Git/blob/master/Protocol.md)
- Scalar 与 GVFS 的关系（"Git will never adopt the GVFS protocol"）：[microsoft/git](https://github.com/microsoft/git)、[microsoft/scalar](https://best-of-web.builder.io/library/microsoft/scalar)
- GitLab bundle-uri：[GitLab 博客 2025-06](https://about.gitlab.com/blog/reduce-the-load-on-gitlab-gitaly-with-bundle-uri/)、[Gitaly Bundle URIs 文档](https://docs.gitlab.com/ee/administration/gitaly/bundle_uris.html)
- promisor-remote / LOP：[gitprotocol-v2 提交记录](https://timplab.syktsu.ru/admin/git/commits/branch/master/Documentation/gitprotocol-v2.adoc)、[邮件列表](https://lore.kernel.ime.usp.br/git/xmqq4j1kq7qz.fsf@gitster.g/)
- protocol v2 `object-info`：[lore.kernel.org](https://lore.kernel.org/git/xmqqmttznqj6.fsf@gitster.g/T/)
- `git backfill`（2.49）：[GitLab：What's new in Git 2.49](https://about.gitlab.com/blog/whats-new-in-git-2-49-0)、[Phoronix](https://www.phoronix.com/news/Git-2.49-rc0-Released)
- `git last-modified`（2.52）：[GitHub：Highlights from Git 2.52](https://github.blog/open-source/git/highlights-from-git-2-52/)、[GitLab：What's new in Git 2.52](https://about.gitlab.com/blog/whats-new-in-git-2-52-0/)
- Gitaly RPC：[CommitIsAncestor](https://gitlab.com/gitlab-org/gitaly/-/issues/308)、[FindMergeBase（Heptapod）](https://foss.heptapod.net/heptapod/heptapod/-/issues/369)
- GitLab blame API：[Repository files API](https://docs.GitLab.com/ee/api/repository_files.html)
- GitHub GraphQL blame：[GraphQL commits 参考](https://docs.github.com/en/graphql/reference/commits)
- Gitiles API：[api-reference.md](https://gerrit.googlesource.com/gitiles/+/HEAD/Documentation/api-reference.md)
- Mononoke 开源版的缺失部分：[Mononoke README](https://github.com/facebook/sapling/blob/main/eden/mononoke/README.md)
- Jujutsu 后端抽象：[LWN](https://lwn.net/Articles/958900/)
- Gitoxide / gix-blame：[Debian tracker](https://tracker.debian.org/rust-gix-blame)
- Sourcegraph MCP：[sourcegraph.com/mcp](https://sourcegraph.com/mcp)
- 第三方 git MCP：[gitlog-mcp](https://pypi.org/project/gitlog-mcp/)、[GitHub DeepBlame MCP](https://github.com/gtnao/github_deep_blame_mcp)

**未能核实**（文中已避免依赖）：Gitaly 是否有 blame RPC 及其当前名称；GitHub 官方 MCP server 是否提供 blame/log 工具；GitLab bundle-uri 在 GitLab.com 上是否已经默认开启（官方文档与合作方博客说法冲突，本文以官方文档"实验特性"为准）；git 邮件列表上 2026 年是否已有响应 Scion 的讨论（检索到的索引里没有，但索引可能滞后）。
