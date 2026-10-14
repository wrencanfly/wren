# 03 · 工作流与组织经验，以及 "公开版" 的战略选择

> 来源：Anthropic Engineering 博客《Scion》全文（66 行，已逐行阅读）。本篇聚焦工作流变化、安全、团队经验、生态章节，最后为开源项目 **Flux** 做战略选项分析。

---

## 1. 工作流变化：Scion 改掉了哪些 "人类尺度" 的默认假设

| 维度 | git 惯例 | Scion 做法 | 原文依据 |
|---|---|---|---|
| Refs | 分支、标签随便建 | **只鼓励 `main` 和 release 分支**；refs 被视为 "所有服务器都必须达成一致的全局状态"，是潜在的扩展瓶颈 | Principles: "discourages refs… except for the main ref or release branches" |
| 提交 | 本地 commit，然后 push | commit / rebase / merge **全部在服务器上执行**，所以 "nothing to push"；代价必须与变更大小成正比，而不是与仓库大小成正比 | "every commit, rebase, or merge happens on the server" |
| Ref 移动 | push 时一并更新 | 和创建 changeset 解耦，只是**单行 Spanner 上的一次 CAS** | Data structures 一节 |
| 只读查询 | 本地 clone 后用 `git log/blame` | 客户端用的所有查询**都以 API 形式开放**；很多 CI 任务完全不需要本地仓库（自动 bisect、自动 revert 流水线都依赖这一点） | "comprehensive source control service" |
| 写操作 | 必须先 checkout | 很多写操作同样可以**不 checkout、纯走 API** | "The working copy is optional" |
| Merge queue | 全局仓库锁，串行化合入 | **放弃全局锁**（"我们没能有效复刻这一行业标准"），转向 **post-merge + pre-deploy 自动化测试**；并发变更会冲突，由 Claude 来修，速度比人发现冲突还快 | "We're completely rethinking our priors" |

核心问题被原文直接抛出：**"当测试还在跑的时候就有成百上千个变更合入，CI 到底意味着什么？"** 每天合入量从 2,500（5 月）涨到 30,000+（9 月）。在这个量级下，"合入前保证 main 永远是绿的" 这一前提已经不成立，质量保障的重心转移到了合入之后：便宜的 ancestry/diff 查询、自动 bisect、自动 revert。

**对 Flux 的启示**：merge 策略是组织层面的选择，不该由工具强加；但工具必须把 post-merge 世界需要的**原语**做得足够便宜，包括 merge-base、is-ancestor、"某次变更何时进了 main"、按路径过滤的 log、blame。原文里有一个反例：多个系统靠**线性扫描 commit log** 来查 "某个变更何时合入 main"。这正说明这类查询需要一个一等公民 API。

## 2. 安全模型

- **仓库级 ACL + 目录级 ACL**：在 monorepo 内部保护最敏感的代码，同时不丢掉 monorepo 的好处。Scion 能做到这一点，是因为客户端只能拿到服务器愿意给的 tree/blob（want-only 协议），服务器可以按路径拒绝请求。原生 git 做不到：任何能 clone 的人都拿得到全部对象。
- **带语义约束的 code review 审批**（Antique）：区分 "只改注释" 和 "改了敏感逻辑"，让人类 reviewer 把注意力放在最要紧的变更上。在 AI 写代码的规模下，review 带宽是稀缺资源，审批规则必须理解变更的**语义类别**，不能只看路径。

**对 Flux 的启示**：目录级 ACL 只有在 "客户端无法绕过服务器拿全量对象" 时才成立。如果 Flux 部署在现有 git 托管前面，只要用户还能直接 `git clone` 源站，目录 ACL 就只是**读加速层上的策略**，算不上安全边界。这一点必须在文档里写清楚，不能夸大。

## 3. 团队与流程经验

1. **小团队、高速度**：7 个人，5 个月，交付了 Scion、Antique 和大约半打相关系统，并完成全量迁移；同一时期日合入量涨了 10 倍。Claude Mythos Preview 在没有额外指令的情况下重写了手写的 git→Scion ingestion 原型，内存降到 1/20，速度提升 4 倍，并且保住了所有隐含的排序和数据结构不变量。
2. **工程师的角色变了**：所有参与者都认为 Claude 写代码比自己强。工程师的价值转向以下几件事。
   - **对 Claude 说 "不"**：agent 有时会为了迁就一个本不该被允许的模式而违反核心系统约束（例如线性扫描 commit log 的例子）。
   - **守护系统不变量**和跨系统的设计取舍。
   - **抵制对抗式 review 引起的过度工程**：对抗式 review 很有效，但视野可能很窄。例如有一次 review 对另一个内部服务过度不信任，导致 Claude 加了大量多余的校验和复杂度。
   - **review 意图，而非逐行阅读**：每个变更仍然有人 review，但看的是 intent。
3. **质量反而更高**：Claude 产出代码的能力，使团队即使在紧迫的时间线下，也能为每一种出现过的基础设施和源码控制交互写 **hermetic simulation**，再加上 **fuzzing** 和精细的 **rate limiting**。Claude 还顺手发现了一个漏用索引的 ancestry 查询优化，而且第二天又做了一遍类似的事。
4. **运维上用 "superfan" Claude**：每个组件一个 Slack 频道，一个 Claude 专门盯这一个组件（scion-daemon、scion-cache、Antique merge queue……），能读遥测和日志，像一个 "除了刷图表没别的事干" 的资深工程师。它能发现传统告警发现不了的细微和新型问题。传统告警仍然保留。

**对 Flux 开发流程的直接借鉴**：

- Flux 有一个天然的 **oracle**：git 本身。每一个 API（blame、merge-base、diff、log -- path）都可以对 `git` CLI 做 **differential fuzzing**，这是 Scion 式 "最强防御的代码库" 在开源项目里最便宜的实现方式。
- 在仓库里维护一份 `INVARIANTS.md`，作为 "对 agent 说不" 的书面依据。例如：对象不可变；缓存层之间 RPC 完全一致；缓存永远不需要回源确认有效性；分层严格隔离，下层故障不能把上层压垮。

## 4. 生态章节：Anthropic 为什么说公开版 Scion 没多大用

原文的论证可以拆成四点：

1. **适用面窄**：只对 "数万名工程师" 或 "成群结队的 agent" 驱动的超大 monorepo 才有意义；而且 Anthropic 的内部 token 预算远超任何正常公司愿意花的量。
2. **偏离 git 太远**：采用它意味着迁移整个开发生命周期。
3. **连带依赖**：它需要配套的 code review、code search、CI，而 Anthropic 内部的这些组件 "highly bespoke"。
4. **讨好所有人的公开版不会真的有用。**

他们给出的替代方案是**把 Scion 最成功的部分带回 git，但不脱离 git 的整体模型**：

- git **服务器**不必和客户端同构，新颖的服务器架构可以带来显著的扩展收益；
- 各 forge 提供**全面的服务端 RPC**，最好跨 forge **标准化**，让 git 客户端学会调用；
- **协商协议**不必是今天的样子（暗指 Scion 的 want-only、按对象请求、可缓存的方式）；
- git 现有的 **sparse 与 shallow 特性应当能良好组合**。

他们预计这类问题会在 **6–12 个月内**变得普遍，并表示希望与 GitHub、GitLab 和 OSS 社区合作。这对 Flux 既是窗口期，也是风险：forge 可能自己把这件事做了。

---

## 5. Flux 的战略选项

### 选项 A：全新 VCS（开源版 Scion）

- **目标用户**：拥有超大 monorepo、愿意整体迁移的组织。全球大概几十家，而且它们要么已经有 Piper/Sapling 一类的系统，要么在自建。
- **采用门槛**：极高。要替换托管、review、search、CI 集成、IDE 插件，还要重新培训开发者。
- **能继承的 Scion 设计**：全部，包括 changeset WAL、derived data、服务端提交、无 packfile。
- **风险**：正好撞上原文的警告。Meta 的 Sapling 开源了客户端，服务端 Mononoke 却很难被外部部署，这是前车之鉴。项目会在 "没有用户也就没有反馈" 中死掉。**不推荐。**

### 选项 B：git 兼容的服务端/后端 + 新客户端协议

线上（wire）保留 git 对象模型（commit/tree/blob 及 OID），服务端内部换成 Scion 式存储（changeset 日志 + derived data + 对象存储），对外同时提供 git smart HTTP（兼容原生 git）、want-only 对象协议和富 RPC（兼容 Flux 客户端）。

- **目标用户**：自托管大型仓库的平台团队，以及 agent 沙箱或 CI 平台提供方。
- **采用门槛**：中到高。它要成为**权威源（source of truth）**，用户就得离开 GitHub/GitLab 的托管，或者解决双写问题；PR、review 生态也要重新接。
- **能继承的设计**：大部分，包括服务端提交、无锁 ref CAS、refs 最小化、derived data，以及服务端 ACL（这里的目录级 ACL 能成为真正的安全边界）。
- **风险**：必须精确复刻 git 的哈希语义（tree 序列化、签名 commit、SHA-1/SHA-256 过渡），还要接收任意 packfile 推送；工程量接近 A，市场却只比 A 稍大。它更适合作为长期演进方向，而不是起点。

### 选项 C：git 加速层（accelerator），部署在现有托管前面

- **它是什么**：Flux 作为**只读镜像 + 计算层**挂在 GitHub、GitLab、Gitea 或自建 git 前面，写入仍然通过 `git push` 或 forge API 回到源站。
- **目标用户**：checkout 成本高、正在大量引入 agent/CI 的中大型仓库团队（这正是原文说 6–12 个月内会普遍遇到问题的人群），以及 agent 沙箱平台。
- **采用门槛**：**最低**。不需要迁移，可以按任务逐个启用：先让 CI 的一个 job 换成 `flux checkout`，或者让 bisect 工具改调 API。
- **能继承的设计**：恰好是 Scion 迁移**第一阶段**走过的路。原文提到，在有自己的客户端和协议之前，Scion 只回答 "metadata" 查询。具体可以继承：
  - 服务端 RPC：diff、merge-base、ancestry、blame、path history；
  - 通用 derived data 框架：递归定义，可选择 eager 或 lazy 计算；
  - **want-only 协商**：从 root tree 开始递归请求缺失对象；
  - **各层 RPC 完全一致的分层缓存**（daemon → regional cache → server），内容寻址、不可变、天然适合 CDN；
  - 节点级 daemon 共享对象；
  - sparse checkout 和虚拟化 checkout。
- **不能继承的部分**：服务端提交、放弃 refs、真正的目录级 ACL（源站仍可被直接 clone）、无锁合入。
- **风险**：
  - 与源站之间的**一致性延迟**（webhook 加轮询）；
  - git 自身持续进化（partial clone、bundle-uri、cone 模式 sparse-checkout），可能吃掉一部分价值，所以 Flux 必须在基准测试里明显胜出；
  - forge 可能自己做同样的东西；
  - 虚拟文件系统跨平台很难（FUSE / macOS FSKit / Windows ProjFS）。Microsoft VFS for Git 最终退回到 Scalar 加 partial clone，这是警示。
  - **对冲方式**：把 RPC 和对象协议做成**开放规范**，正好回应原文 "跨 forge 标准化" 的呼吁。即使 forge 自己实现，Flux 也是参考实现。

### 对比小结

| | A 新 VCS | B 兼容后端 | C 加速层 |
|---|---|---|---|
| 迁移成本 | 全量 | 托管迁移 | 无 |
| 首个用户的时间 | 以年计 | 以季度计 | 以周计 |
| 与原文建议的一致度 | 违背 | 部分符合 | 高度符合 |
| 继承 Scion 的程度 | 100% | ~70% | ~50%（读路径基本全拿） |
| 主要风险 | 无人采用 | 哈希兼容和托管替代 | 被 git/forge 吸收 |

## 6. 建议

**选 C 作为切入点，并在架构上为 B 预留演进路径，明确不做 A。**

理由如下。C 避开了原文列出的所有 "公开版无用" 的理由：不偏离 git，不需要自带 review/search/CI，也不用讨好所有人，只解决 "agent/CI 读仓库太贵" 这一个具体痛点。同时，它复用的是 Scion 收益最大的部分：checkout 快两个数量级，基础设施缩小 200 倍，这些收益都来自读路径。等 C 积累了用户和存储层，再让 Flux server 可选地成为权威源（B），解锁服务端提交和真正的目录级 ACL，就是自然的下一步。

## 7. MVP 范围（建议 8–12 周）

**包含**：

1. **flux-server（ingest + derived data）**：从 git 源站 fetch，并订阅 webhook；对象按 git OID 内容寻址存入对象存储（S3/GCS/本地盘）。derived data 首批包括：带 generation number 的 commit 图和 skip list、path history、增量 blame。
2. **查询 API**（gRPC + HTTP/JSON）：`commit`、`diff(a,b)`、`merge-base`、`is-ancestor`、`log --path`、`blame`、`read-file@rev`、`list-tree`。同时附带一个 **MCP server** 包装，让 agent 不用 checkout 就能直接查询。这是相对 git 最直观的差异化。
3. **want-only 对象协议**：`POST /objects:batchGet {oids}`。对象不可变，带长缓存头；先请求 tree，再请求 blob；可续传。
4. **flux-cache**：与 server 的 RPC 完全一致的无状态缓存层，可以水平扩展。MVP 中只需要一层。
5. **`flux` CLI**：`flux checkout <rev> --sparse <dirs>`，在本地生成工作区，以及一个能让 `git status/diff/commit/push` 正常工作的最小 `.git`（例如 shallow + sparse，并把 promisor remote 指回源站）。另有一个**节点级本地对象缓存目录**，供同机多个 job 共享。
6. **基准测试与正确性**：在 linux、chromium 镜像这类大型公开仓库上，对比 `git clone --filter=blob:none --sparse` 和 shallow clone。所有 API 都要对 git CLI 做 differential fuzzing，并配 hermetic simulation（模拟源站延迟、强推、缓存故障）。

**明确不做**（v2 以后再考虑）：虚拟化（FUSE）checkout、服务端提交和 merge queue、目录级 ACL（MVP 只继承源站 token 的仓库级权限）、code review 和 code search、以及成为权威源。

**MVP 成功标准**：对一个 ≥10 GB 历史的仓库，CI 风格的 sparse checkout 比 git 原生最佳组合快 ≥10 倍；blame、merge-base 这类查询无需本地仓库，p99 < 100 ms；与 git 的 differential fuzzing 零差异。
