# Review 06 · Agent 规模的"墙"：Flux 挡住的是哪几堵？

> 问题：未来 6–12 个月，依赖 AI 编码的中大型团队会撞上 Scion 那一类墙。Flux（v0.1：只读，每个仓库一个权威 server，SQLite，写入走 `git push`）在多大程度上帮得上忙？
> 依据：Scion 原文 [1]；GitHub、GitLab、Mergify、CircleCI 等 2025–2026 年的公开材料（见文末来源）；Flux 的 `README.md`、`docs/design.md`、`docs/benchmarks.md`、`docs/00-overview.md` 和 `crates/flux/src/{mcp,server,main}.rs`。
> 日期：2026-10-13。

---

## 结论（先说直话）

1. **团队最先撞上的墙集中在"过程层"**：code review 吞吐、合入前 CI 的成本和排队、flaky test、合入 main 时的竞争（merge queue 和 non-fast-forward 重试）、forge API 限流。**Flux 几乎一堵都没碰。** 它解决的是"数据层"的墙：clone/fetch 风暴、拿历史之前必须先 clone、镜像机群扩不动。这些墙要等仓库到 GB 级、作业量到每天数万次才会疼，而 forge 自己正在花大钱解决：GitHub 2026-10 公布了存算分离和读缓存 worker [3]；Entire 2026-07 推出了区域只读镜像，push 转发回 GitHub [6]，**和 Flux 的定位几乎一模一样**。
2. **原文本身就承认，Scion 的价值主要在写路径和流程上。** 原文说："A proper merge queue … doesn't make much sense at agent scale"，Anthropic 改成了 "post-merge and pre-deploy automated testing"；提交 "cheap, incremental, and lock-free"；ref 的移动是一行 Spanner CAS [1]。Flux 把这些全部列为非目标或 v0.4（design §1、§12）。**Flux 选择复刻的，恰好是 Scion 里离中等规模团队最远的那一半。**
3. **token 并不是 Flux 的设计目标，现状甚至会浪费 token。** 原文没有说 Scion 节省 token，它的原话是 Anthropic "spend tokens for internal use far exceeds what any other business would realistically want to spend" [1]：token 预算是**负载的来源**，不是被优化的对象。Flux 的 `landed`、`is_ancestor`、`bisect_step` 确实能用几十个 token 回答原本要扫 log 的问题。但 `flux_read_file`、`flux_blame`、`flux_diff`、`flux_show`、`flux_refs` **都没有行范围、截断或分页**。我估算 git/git 上一次 `flux_refs` 约 2.5 万 token，正好撞上 Claude Code 默认的 MCP 输出上限 25,000 [12]（详见 §3.2）。
4. **建议**：未来 6–12 个月别再往"读加速"上加码，比如 S3 后端、FUSE。应该做三件小而准的事：(a) **省 token、可缓存的 MCP**（几天的工作量）；(b) **新鲜度**：webhook 同步，加上 read-your-writes；(c) 用已有的 `landed` 和 `bisect`，做**合入后测试的定位与回滚管线**（culprit finder），再加一个"服务端 rebase + CAS push + 重试"的 landing 原语。(c) 是 Flux 唯一能切进早期之墙的地方，而且它已经有一半零件了。

---

## 1. 墙的清单（大致按撞上的先后排序）

为了讨论方便，把规模分三档：

- **A 档**：5–20 名工程师，并发 10–50 个 agent，仓库小于 1 GB。今天大多数用 AI 写代码的团队在这一档。
- **B 档**：20–200 名工程师，并发几百到上千个 agent，每天几百到几千次合入，仓库 1–20 GB。这是 6–12 个月后的"别的团队"。
- **C 档**：Scion 的规模，每天 30,000 次合入，100 GB 历史，每天 2,500 万次 CI checkout [1]。

| # | 墙 | 谁、在什么规模撞上 | 公开证据 | 原文：Scion 怎么做 |
|---|---|---|---|---|
| 1 | **Code review 吞吐** | A 档第一天就撞上。agent 的产出速度很快超过人的 review 速度 | LinearB 2026：agentic PR 的 pickup time 是无辅助 PR 的 5.3 倍；Faros：未经 review 就合入的 PR 增加 31.3% [9]。Futurum 评 GitHub 改造时说："Faster pushes shift the bottleneck to review" [4] | 自建 code review 系统 Antique；人 "examine the intent of the change"，不再逐行读；审批规则能区分"只改注释"和"改了逻辑" [1] |
| 2 | **合入前 CI 的成本和排队** | A 到 B 档。PR 数乘以每个 PR 的 CI 分钟数，agent 让 PR 数翻好几倍 | CircleCI 2026：日均 workflow 数 +59%，同时 main 分支稳定性下降约 7 个百分点 [8]；GitHub Actions 2026-09 跑了 32.6 亿次，同比约 4 倍 [3] | **放弃逐个 PR 的全局锁式门禁**，转向 "post-merge and pre-deploy automated testing"；checkout 提速让 CI 省了 25% CPU [1] |
| 3 | **Flaky test 被放大** | A 到 B 档。重跑次数随 PR 数线性增长 | 遇到 flaky 的团队比例从 2022 年的 10% 涨到 2025 年的 26%（Bitrise，经二手汇编）[10] | 原文没有直接讨论 |
| 4 | **main 被打断，merge queue 跟不上** | B 档。Mergify 的拐点在 15–20 名共享代码库的工程师。我的推断：agent 应该按"有效贡献者"计入 | Mergify（477 个组织，20 万次合入）：40 人以上团队的 broken-main 率是 12.5%；私有仓库排队 p99 约 20 小时；94% 的合入是一次一个 PR，没有批处理 [7] | 原话："We haven't been able to effectively replicate … a global repository lock on the merge queue"；冲突交给 Claude 修 [1] |
| 5 | **同一个 ref 上的 push 竞争和 non-fast-forward 重试** | B 档。见下方的估算 | GitHub："merge queues funnel all merges onto one ref"；"a push is only as fast as the slowest replica"；push 量同比 4.9 倍 [3] | 服务端 commit、rebase、merge，所以"nothing to push"；创建 changeset 不加锁；ref 的移动是单行 CAS [1] |
| 6 | **Forge API 限流** | A 到 B 档。几十个 agent 共用一个 App 去轮询 PR 和 CI 状态 | REST 接口：用户每小时 5,000 次；GitHub App 安装每小时 5,000 到 12,500 次（Enterprise Cloud 为 15,000 次），另有并发和每分钟点数的二级限流 [11] | 原文不涉及：内部服务，有自己的限流 |
| 7 | **agent 读历史的 token 成本** | 任何档位都有，是一笔持续的费用 | 官方 GitHub MCP 光工具定义就有 4.2 万到 5.5 万 token [13]；Claude Code 的 MCP 输出默认上限是 25,000 token [12] | 原文不把它当问题，Anthropic 的 token 预算是例外（见结论第 3 点）。查询 API 只是间接减少了 agent 自己扫 log |
| 8 | **CI 和 sandbox 的 clone 风暴** | 在 GitHub.com 上，这首先是 GitHub 的问题，但它的故障会落到你头上。自建 GitLab、Gitea 或 Bitbucket DC 时，**B 档就撞上** | GitHub："CI and code scanning clone or fetch the same branch tip thousands of times per minute" [3]；GitLab 的 pack-objects 缓存让 20 个并发浅克隆的服务端 CPU 降了 9 倍，但官方文档仍承认存在 thundering herd [14] | want-only 协议加分层缓存，每层命中率超过 99%；checkout 从分钟级降到秒级 [1] |
| 9 | **拿历史之前必须先 clone**（sandbox 冷启动） | B 档。仓库越大、sandbox 越短命，越疼 | 一份 git/git 的 blobless bare clone 需要 5.5 s 和 117 MiB（benchmarks.md） | 完整的服务端查询 API："in many cases jobs don't need a local repository at all"；自动 bisect 和 revert 都依赖它 [1] |
| 10 | **ref 膨胀和 ref advertisement** | B 到 C 档。agent 分支（`copilot/*`、`claude/*` 等）累积到 10^4 以上 | 某些 git 命令会读每一个 ref，几千个 stale ref 就能让很多命令明显变慢；GitLab 遇到过约 50 万个 ref 的仓库 [15] | "discourages refs"，只保留 main 和 release 分支 [1] |
| 11 | **存储、GC 和仓库维护** | C 档。自建实例在 B 档后期 | GitHub："Compaction and garbage collection are among the heaviest work"，已挪到独立的 worker 上 [3] | 没有 packfile，没有仓库维护 [1] |
| 12 | **镜像机群受单机磁盘和内存上限** | 只有 C 档 | 原文：数百台 git 镜像，单机装不下 [1] | 任何地方都没有完整物化的仓库副本；CPU 和内存降到原来的 1/200 [1] |

**墙 5 的估算**（自己推的，不是引用）：假设一个 PR 从"rebase 到最新 main"到"CI 通过再 push"要 T 分钟，main 平均每分钟移动 λ 次，那么 push 时 main 已经变了的概率约为 1 − e^(−λT)。

- 每天 100 次合入（按 8 小时工作日计，λ ≈ 0.2/分钟），T = 10 分钟时，这个概率约 87%。也就是说，**只要开着"必须基于最新 main"的分支保护，A 档后期就已经到了几乎每个 PR 都要重做一轮 CI 的地步。**
- 到每天 30,000 次合入，这个概率就是 100%。

这就是原文放弃全局锁的原因，也说明这堵墙远比 clone 风暴来得早。

**用户的判断"对大多数团队 git 够用"，我部分同意。** git 的**数据层**对 A、B 档确实够用：GitHub 正在把它改造到 30 倍容量 [5]，GitLab 有 pack 缓存，git 本身也有 protocol v2 的 ref 前缀过滤和 bundle-uri。先垮的是 git 之上的**过程层**，也就是 PR、review、CI 和 merge queue。原文的预测是 "this class of problems will become more commonplace over the next six to 12 months" [1]。这里的 "this class" 涵盖过程层，不只是 clone。

---

## 2. 墙 → Flux：逐一对照

图例：✅ 解决　◐ 部分解决　✗ 没处理　⊘ 明确不做（非目标）

| # | 墙 | Flux 状态 | 依据和说明 |
|---|---|---|---|
| 1 | Code review 吞吐 | ⊘ | 不在范围内。`landed`、`blame` 可以给 review bot 提供上下文，但这只是边角作用 |
| 2 | 合入前 CI 的成本 | ◐（只覆盖 checkout 这一小块） | sparse checkout 冷启动快 2.6 倍（709→269 ms）。CI 时间的大头在构建和测试，Flux 不碰 |
| 3 | Flaky test | ✗ | 没有相关功能 |
| 4 | main 被打断，merge queue | ◐（只有事后定位的零件） | **`landed` 加 `bisect_step` 正好是"合入后测试"模式所需的定位原语**。原文点名 "automated bisect and revert pipelines" 依赖查询 API [1]。但 Flux 没有把它们组成管线，也没有 revert 和 landing |
| 5 | push 竞争和 non-fast-forward | ⊘（v0.4 才考虑） | design §1 把服务端提交和无锁合入列为非目标。**而且 Flux 还引入了新的滞后**：默认每 30 秒轮询一次（`main.rs:58-60`），没有 webhook。agent 刚 push 完，`flux_landed` 可能返回 "not contained"，agent 会重试或者误判 |
| 6 | Forge API 限流 | ◐ | 读文件、log、blame、diff 可以从 forge API 转到 Flux。但 agent 最主要的轮询对象是 PR 和 CI 状态，Flux 不覆盖 |
| 7 | 读历史的 token 成本 | ◐（有好有坏，见 §3.2） | 精确问答类的工具很省；读内容类的工具没有边界，比 agent 自带的 Read（支持 offset 和 limit）还差 |
| 8 | clone 风暴 | ◐（机制对，但缺关键的一块） | want-only 协议加同 RPC 缓存层是对的。但**缓存层没有 singleflight**（design §10）。CI 风暴的典型形态恰好是"同一个 commit 被几百个作业同时冷读"，同一个对象的并发未命中会各自回源，这正是风暴最需要的那一块。另外没有负载测试 |
| 9 | 先 clone 才能查历史 | ✅（Flux 最扎实的一块） | 查询不需要本地仓库；省掉 5.5 s 和 117 MiB 的前置成本（git/git） |
| 10 | ref 膨胀 | ◐ | checkout 走 want-only，不涉及 ref 协商。但 mirror 会同步**全部** `refs/heads/*` 和 `refs/tags/*`，并带 `--prune`（`git_source.rs:72,82,116`），每 30 秒还要做一次完整的 ref advertisement。`flux_refs` 没有前缀过滤，也没有上限 |
| 11 | 存储和 GC | ◐ | server 端不用 pack，不用 repack。但一对象一文件的 loose 存储在 10^7–10^8 个对象时会遇到 inode 和目录压力，这一点**没有验证过**。节点缓存只增不减（design §10） |
| 12 | 镜像机群上限 | ✗（v0.2 路线图） | 每个仓库一个权威 server，SQLite，server 上有一份完整对象集（design §1） |

**判断**：Flux 的 ✅ 和 ◐ 集中在 #8–#12，也就是 C 档和 B 档后期的墙；#1–#6 几乎全是 ✗ 和 ⊘。这就是任务描述里担心的情况：**解决了晚到的墙，忽略了先到的墙。** 更麻烦的是，#8 和 #9 正是 GitHub [3]、Entire [6]、Cursor Origin [16] 在 2026 年下半年集中投入的方向：

- Entire 的公开数据（厂商自测）：每小时 57 万次 clone；"pushes through a mirror are forwarded to GitHub"。这和 Flux 的"只读加 `git push` 到源站"是同一个产品形态。
- Cursor Origin 同样支持镜像 GitHub 并转发 push，还带 MCP。

在这个时间窗口里，读加速正在被商品化。

---

## 3. 量化

### 3.1 什么规模下 Flux 的读路径收益才重要

benchmarks.md 测的是 git/git：85,545 个 commit，42 万个对象，单机，localhost。

| 收益项 | git/git 上的数值 | 每天 1,000 个 sandbox 或 CI 作业 | 每天 10,000 个 |
|---|---|---|---|
| 省掉查历史前的 clone | 5.5 s，117 MiB | 约 1.5 小时 CPU，约 114 GiB 流量 | 约 15 小时，约 1.1 TiB |
| sparse checkout 冷启动 | 每次省 440 ms | 约 7 分钟 | 约 73 分钟 |
| 同节点第二个作业 | 每次省 148 ms | 约 2.5 分钟 | 约 25 分钟 |

结论：

- **在 git/git 这个量级（几百 MB），收益在经济上几乎可以忽略。** 每天 1 万个作业省下来的也就是十几个 CPU 小时。
- 收益要变得实质性，需要两个条件**同时**成立：
  - (a) 仓库到 GB 级，blobless clone 要几十秒到几分钟。比如一个 5 GB 的仓库 clone 一次要 2 分钟，每天 1 万个作业就是约 330 个 CPU 小时。
  - (b) 每天作业量过万，而且每个作业只读一小块（sparse cone）。
- 大致说，这对应 B 档后期。benchmarks.md 自己也承认 "these benchmarks do not demonstrate that"。规模放大以后：
  - loose 存储在千万级对象下的表现，没测过；
  - 单个 SQLite 权威 server 的 QPS 上限，没测过；
  - 冷 blame 622 ms，而且第一次要推导完整的变更链，几百个 agent 同时打到冷文件时会怎样，也没测过。

**一个被忽视的放大器：MCP 的默认值绕过了不可变缓存。** 缓存层只缓存"所有 rev 都是 40 位完整 oid"的查询（`server.rs:167-189`）。但 MCP 的每个工具，rev 默认都是 `"HEAD"`（`mcp.rs:54,76,113,134,146`），也不会先解析成 oid。所以 **agent 走默认路径发出的查询全部穿透缓存层，压到单个权威 server 上。** "缓存层能横向扩展"这个卖点，对 agent 流量基本不成立。

摄入：首次同步 git/git 要 173 s，约 500 个 commit/s。照这个速度，每天 3 万次合入（约 0.35 次/s）的增量摄入不是瓶颈。百万级 commit 仓库的首次摄入约 35 分钟，按线性外推，可以接受。

### 3.2 Flux 能不能减少 agent 的 token？

逐个工具看（`crates/flux/src/mcp.rs`）。token 数是按字符数估的：hex 约 2.2 字符/token，其余约 3.5 字符/token。refs 数据来自 `git ls-remote` 对 git/git 的实测（1,020 个 ref，61.6 KB），blame.c 是 master 上的真实文件（1,334 行，37.5 KB）。

| 工具 | 输出形态 | 估算 | 对比 agent 自己用 git 或 Read | 评价 |
|---|---|---|---|---|
| `flux_landed` | 一个 oid | 约 20 token | `git log --ancestry-path --merges` 加人工筛选，几百到几千 token，还得先有完整历史 | ✅ 好 |
| `flux_is_ancestor` | `true` 或 `false` | 约 1 token | `merge-base --is-ancestor` 相当，但要先 clone | ✅ 好 |
| `flux_bisect_step` | 一行 | 约 30 token | 手动 bisect 时每一步都要读 log | ✅ 好 |
| `flux_merge_base` | oid 列表 | 约 20 token | 相当 | ✅ |
| `flux_log` | 默认 20 行，每行带 40 位 oid、作者、**邮箱**、标题 | 约 40 token/行 | 相当；`--oneline` 更省 | ◐ 有默认上限（好）；**没有游标**（server 只有 `limit`，`server.rs:401`），翻页要用更大的 limit 重新请求，前面的行会重复返回；没有按时间、作者、消息内容过滤 |
| `flux_list_dir` | 文件名列表 | 小 | 相当 | ✅ |
| `flux_read_file` | **整个文件**（`mcp.rs:129-139`） | blame.c 约 1 万 token | 宿主的 Read 工具支持 offset 和 limit | ✗ 比 agent 自带工具还差；二进制文件会被有损转成文本塞进上下文 |
| `flux_blame` | **整个文件的每一行，加上内容**（`mcp.rs:108-127`） | blame.c 约 56 KB，约 1.5 万 token 以上，相当于文件本身的 1.5 倍 | `git blame -L 120,140` 只要约 300 token | ✗ agent 几乎总是只关心几行 |
| `flux_diff` | 完整 unified patch，没有上限 | 跨版本 diff 可达 MB 级 | `git diff --stat` 或 `-- path` | ✗ 没有 stat 模式，没有上下文行数参数，没有单文件或总量上限（`name_only` 是唯一的缓解手段） |
| `flux_show` | **pretty-printed JSON**（`mcp.rs:80`），含全部变更路径 | 大 merge 时没有上限 | `git show --stat` | ✗ JSON 缩进和键名本身就耗 token |
| `flux_refs` | 全部 ref（`mcp.rs:215`） | **git/git 上约 2.5 万 token** | `git ls-remote origin 'refs/heads/main'` | ✗ 已经碰到 Claude Code 默认的 25,000 token MCP 输出上限 [12]，agent 分支一多就肯定超出 |
| 工具定义本身 | 11 个工具，schema 都很短 | 约 1k token | 官方 GitHub MCP 是 4.2 万到 5.5 万 token [13] | ✅ 这是真实的优势 |

**结论**：Flux 能减少 token，**但这是附带效果，不是设计出来的。**

- "问一个精确问题，得到一个精确答案"的工具（landed、ancestry、bisect、merge-base）确实比让 agent 扫 log 省一到两个数量级。
- 读内容的工具（read_file、blame、diff、show、refs）既没有行范围，也没有字节上限、截断说明或游标。这违反了 Anthropic 自己在 *Writing effective tools for agents* 里强调的做法：分页、过滤、截断时告诉 agent 剩余多少以及怎么缩小范围，以及 concise 和 detailed 两种输出 [2]。
- 文档里没有任何关于 token 的设计目标或 benchmark。
- agent 真正最耗 token 的历史类操作，Flux 都没有：在某个 revision 上做代码搜索（grep）、pickaxe（`log -S`，"哪个提交引入了这个字符串"）、行范围历史（`log -L`）。

---

## 4. 建议：未来 6–12 个月做什么，不做什么

按"影响 / 工作量"排序。

### P0：几天就能做完，直接见效

1. **省 token、可缓存的 MCP**（影响高，工作量小）
   - `read_file` 和 `blame` 加 `start_line` 和 `end_line` 参数；所有内容类工具加 `max_bytes`，默认约 8–10k token。超出时附一句截断说明，比如 "N more lines; narrow with start_line/path"。
   - `diff` 默认输出 stat，加 `context` 参数和每个文件的上限；`show` 改成紧凑文本，去掉 pretty JSON；`refs` 加 `prefix` 和 `limit`；`log` 加 `after` 游标和 `since`、`author` 过滤，邮箱可选，oid 缩短到 12 位。
   - **MCP 层先把 `HEAD` 解析成完整 oid，后续查询都带 oid，并把这个 oid 告诉 agent。** 这样同一轮任务里的后续调用都是固定查询，可以被缓存层永久缓存，答案也前后一致。
   - 在 benchmarks.md 里加一张"每回答一类问题花多少 token"的表，对比 git CLI 和 GitHub MCP。**这是 Flux 唯一能和 GitHub、Entire 拉开差距的可量化卖点。**
2. **新鲜度和 read-your-writes**（影响高，工作量小）
   - 加 webhook 触发的同步（路线图里已经有）。
   - 查询带 `min_commit=<oid>` 参数：没看到这个 commit 时，先触发一次同步再等待，超时则返回明确的 `stale` 状态。每个响应都带 `as_of`。
   - agent 的循环是"push，然后查询"，没有这一条，`landed` 在 agent 场景里会给出错误答案。
3. **缓存层的 singleflight**（已在路线图里，工作量小）。这正是 CI 风暴的形态，应该先于 S3 后端做。

### P1：几周，切进先到的墙

4. **合入后测试的定位与回滚管线**（`flux culprit`。影响高，工作量中等）
   - 输入是 main 第一父链上一段从绿变红的区间；用 `bisect_step` 在区间内二分；每一步用 sparse checkout 跑测试；输出 culprit，并生成 revert 分支或 PR（通过 `git push` 提交，不违背只读定位）。
   - 这让 A 和 B 档团队可以照原文的思路做：**把昂贵的合入前全量 CI 降成轻量的合入前检查，加上合入后的全量测试和自动定位。** 它直接减轻墙 2、3、4，而且 Flux 已经有了 `landed`、`bisect`、sparse checkout 这三个零件。flaky 判定可以顺手做：同一步重跑 N 次，结果不一致就标成 flaky，不算作 culprit。
5. **landing 原语，不当权威源**（`flux land`。影响高，工作量中等到偏高）
   - 在服务端用 `git merge-tree --write-tree`（git 2.38 及以上）把分支 rebase 或 merge 到最新的 main，不需要工作区。
   - 用带 `--force-with-lease` 语义的 push 写回源站。源站的原子 ref 更新就是 CAS。
   - 遇到 non-fast-forward 时，在服务端自动重算再重试；冲突时返回结构化的冲突信息给 agent 去修。按目标 ref 排队，支持批处理。
   - 这是原文"无全局锁、冲突交给 Claude 修"思路在 git 上的最小可行版本，直接针对墙 5。**注意别把它做成第二个 Mergify 或 GitHub merge queue**：只做"服务端 rebase + CAS push + 重试 + 冲突报告"这一个原语，CI 门禁和审批仍然交给现有系统。
6. **面向 agent 的历史检索**（影响中到高，工作量中等）
   - 在某个 revision 上做代码搜索：trigram 索引作为 derived data，按 changeset 增量更新。
   - pickaxe（`log -S`）和行范围历史（`log -L`、`blame -L`）。
   - 原文提到 Scion 配套做了 code search [1]；这也是 agent 最耗 token 的读操作。

### P2：只有 C 档才需要，6–12 个月内应该降级

7. S3、GCS 对象存储，Postgres 或 Spanner 元数据，多个无状态 server（路线图 v0.2）。除非有拿着 10 GB 以上仓库的设计伙伴，否则往后放。可以先做一个负载测试，弄清楚单个 server 的 QPS 上限，用它来决定什么时候必须做。
8. 带引用计数的空间回收：中等优先级。CI 节点长期运行时会被磁盘撑爆，但比上面几项晚。

### 明确声明不做（写进 README 和 design §1）

- **Code review 系统**（相当于 Antique）、**CI 调度和测试运行器**、**flaky test 管理平台**：交给现有生态。Flux 只通过 `culprit` 提供数据和定位。
- **完整的 merge queue 产品**和**代理 forge 的 PR、issue、checks API**：在这些方向上和 GitHub、GitLab、Mergify、Graphite（Cursor）正面竞争没有胜算。
- **成为权威源**（v0.4 的服务端 commit 和目录级 ACL）、**FUSE、FSKit 虚拟化 checkout**：都超出 6–12 个月的窗口。
- **C 档的镜像机群替代**：直接承认这是 GitHub 存储改造 [3] 和 Entire [6] 的地盘。

### 定位建议

README 的副标题现在是 "Checkouts that cost what you read"。建议把重心移到 **"History answers for agents, with token-bounded responses, and post-merge culprit finding"**。

- 读加速（checkout、缓存层）保留，作为 self-hosted 场景和 GitLab、Gitea 用户的附加价值。
- 这样 Flux 的差异化落在 derived data 查询、MCP 和合入后测试上。这三样是 GitHub 的存储改造和 Entire 的区域镜像都不提供的，而且 Flux 已经做了一大半。

---

## 来源

1. Anthropic Engineering, *Redesigning source control for agent-scale development*（2026-10-13）. https://www.anthropic.com/engineering/redesigning-source-control
2. Anthropic Engineering, *Writing effective tools for agents*. https://www.anthropic.com/engineering/writing-tools-for-agents
3. Brian Celenza, GitHub Blog, *Building Git infrastructure for agent-scale development*（2026-10-06）. https://github.blog/engineering/architecture-optimization/building-git-infrastructure-for-agent-scale-development/
4. DevOps.com, *Coding Agents Broke Git's Scaling Math. GitHub Is Rebuilding to Keep Up*（2026-10-08，含 Futurum 的 Mitch Ashley 评论）. https://devops.com/?p=199746（转载：https://rssfeedtelegrambot.bnaya.co.il/index.php/2026/10/08/coding-agents-broke-gits-scaling-math-github-is-rebuilding-to-keep-up/）；另见 The Register https://www.theregister.com/a/5302148 和 Techzine https://www.techzine.eu/news/devops/144877/github-is-rebuilding-its-git-infrastructure-35x-higher-write-throughput/
5. The Register, GitHub 30X capacity 报道. https://www.theregister.com/a/5255125；GitHub availability report March 2026. https://github.blog/news-insights/company-news/github-availability-report-march-2026/
6. WinBuzzer, *Former GitHub CEO Launches Git Network Built for AI Coding Agents*（Entire，2026-07-08）. https://winbuzzer.com/2026/07/08/entire-opens-git-network-preview-for-ai-coding-agents-xcxwbn/
7. Mergify, *State of Merge Queues 2026*. https://mergify.com/reports/state-of-merge-queues-2026；*Merge Queues Were Built for Humans. AI Agents Need More.* https://mergify.com/blog/merge-queues-and-ai-coding-agents
8. CircleCI, *2026 State of Software Delivery*（经 PlatformCon 议程和 Codacy 转述）. https://platformcon.com/sessions/your-ai-is-writing-code-faster-than-you-can-ship-it；https://blog.codacy.com/ai-breaking-code-review-how-engineering-teams-survive-pr-bottleneck
9. LinearB 和 Faros 数据（经 Codacy、Flowverify 转述）. https://blog.codacy.com/ai-breaking-code-review-how-engineering-teams-survive-pr-bottleneck；https://www.flowverify.co/blog/ai-code-review-bottleneck-2026-data；另见 https://tianpan.co/blog/2026-07-02-the-merge-queue-is-the-new-bottleneck
10. Flaky test 数据汇编（Bitrise、Google、Atlassian，均为二手）. https://www.getpanto.ai/blog/flaky-test-statistics；https://testdino.com/blog/flaky-test-benchmark/
11. GitHub Docs, *Rate limits for the REST API*. https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api
12. Claude Code MCP 输出上限 25,000 token（`MAX_MCP_OUTPUT_TOKENS`；二手来源，Figma 的文档有实际报错示例）. https://developers.figma.com/docs/figma-mcp-server/mcp-clients-issues
13. GitHub MCP 工具定义的 token 开销（社区测量，口径不一）. https://getunblocked.com/blog/github-mcp-token-cost/；https://policylayer.com/token-cost/github
14. GitLab, *Git fetch performance*（2022，pack-objects 缓存，CPU 降 9 倍）. https://about.gitlab.com/blog/git-fetch-performance/；GitLab CI performance docs. https://docs.gitlab.com/development/pipelines/performance
15. ref 数量对 git 性能的影响：git 邮件列表 https://lore.kernel.ime.usp.br/git/xmqqcyhq3ge7.fsf@gitster.g/T/；GitLab issue https://gitlab.com/gitlab-org/gitlab/-/issues/296625；GitLab infra https://gitlab.com/groups/gitlab-com/gl-infra/-/work_items/400
16. InfoQ, *Cursor Origin*（2026-08）. https://infoq.com/news/2026/08/cursor-origin-alternative-github

**证据强度说明**：

- [1] [3] [11] [14] 是一手来源。
- [7] 是厂商自己的数据，但样本量大。
- [6] [16] 的性能数字是厂商自测。
- [8] [9] [10] [12] [13] 是二手转述，具体数字只能当作量级参考。
- "墙 5 的估算"和 §3 里的 token 估算是本文自己推算的，方法已在正文中写明。
