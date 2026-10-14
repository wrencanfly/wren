# 01 · 问题与设计原则：为什么 git 在 agent 规模下失效，Scion 的六条原则

> 来源：Anthropic Engineering Blog《Redesigning source control》（Scion）。
> 标注约定：**【原文】**= 文章明确陈述；**【推断】**= 本文作者基于原文数字/逻辑的推导；**【外部】**= 文章之外的个人知识（Piper / Sapling / git 内部机制），未经文章证实。

## 1. 关键数字（全部【原文】）

| 指标 | 数值 |
|---|---|
| 日均合并变更 | 2026-05 约 2,500/天 → 2026-09 超过 30,000/天 |
| main 分支历史大小 | 接近 100 GB |
| 压缩后 working copy | 超过 25 GB |
| CI checkout 次数 | 约 2,500 万次/天（不含开发环境、agent sandbox、research job） |
| 朴素 fetch 的流量 | 超过 600 PB/天 |
| 内部 git mirror 峰值 | “several hundred” 个 |
| 迁移结果 | 基础设施 CPU/内存 footprint 缩小 200x；checkout 从分钟级降到秒级（两个数量级）；CI 节省 25% CPU；每层 cache hit rate >99% |
| 团队/工期 | 7 人，5 个月完成 Scion + Antique 并全量迁移，期间日合并量增长 10x |

【推断】几点算术：
- 2,500 万 × 25 GB = 625 PB，与“600 PB 以上”吻合。说明这个数是按**压缩 working copy**（相当于 shallow clone）算的，而非带完整 100 GB 历史的 full clone（那将是约 2.5 EB/天）。
- 5 月→9 月是 **12x**，正文另一处称“10x”，两者口径/时间窗不同，属轻微不一致。
- 3 万次合并/天 ≈ 平均每 3 秒一次；2,500 万 checkout ÷ 3 万合并 ≈ 每次合并对应 ~800 次 CI checkout（时间窗可能不完全对齐，仅作量级参考）。

## 2. 因果链：为什么 git 撑不住

【原文】git 的设计前提是去中心化：每个 clone 都是完整、自洽的仓库，能离线做 diff、history、blame、checkout 任意 revision。这对绝大多数仓库是正确设计。

【原文 + 推断】串起来的链条：
1. **Claude 让写代码不再是瓶颈** → 合并率指数增长（且“latent demand 几乎无限”，每消除一个瓶颈产出就再上升）。
2. 合并率↑ → **历史体积增长加速**（~100 GB）→ 完整 clone 越来越不可行。
3. 合并率↑ → **CI 次数同比上升**（每个变更触发多次 checkout）→ 2,500 万次/天 × 25 GB ≈ 600+ PB。
4. 缓解手段（shallow / sparse / promisor / mirror / seed）全部用上，但【原文】：
   - **mirror**：峰值几百台，且“repo 超出 mirror 的磁盘或内存上限后就无法扩展”——每台 mirror 都是全量副本，只能纵向扩。
   - **seed**（blob storage 中的仓库快照）：合并越快，两次刷新之间 seed 离 main tip 越远 → 增量 fetch 越大。
   - **partial clone**：要让依赖历史的操作（diff、blame）继续工作“需要越来越多的小心”。
5. 【原文】根本矛盾：大家其实都把 git **当中心化系统用**（一个权威副本），而客户端几乎全是 agent 和 job——**按定义就在线**。git 为离线/分布式付出的代价（全量数据下沉到每个客户端）在这里纯属负担。

【外部】补充机制解释：git 的 fetch 协商是 have/want 模式，服务端要为每个客户端计算“缺什么”并生成 packfile（counting objects + delta 压缩），CPU 开销随客户端状态多样性和 tip 漂移增长；这正是 seed 越旧、mirror 越累的原因。

## 3. 核心重构

【原文】“we were using git as a centralized system with one authoritative copy, but nearly all of our clients were agents and jobs that are online by definition.” 问题被重写为：**如果从一开始就假设 agent 永远在线，source control 会长什么样？** 六条原则都从这里推导。

## 4. 六条原则

| # | 原则（【原文】） | 拒绝的 git 假设 | 换来什么 | 代价（多为【推断】） |
|---|---|---|---|---|
| 1 | **Clients are always online**：客户端按需从永远可达的服务端取数据 | 客户端离线工作、需要完整副本 | 客户端无需全量数据 | 离线完全不可用；服务端可用性成为全公司开发的关键路径；每次操作都有网络延迟 |
| 2 | **Client-server asymmetry**：服务端负责 history/blame、建 commit、merge/rebase；客户端只做有界对象缓存、铺文件、监控 FS 变化 | 每个 clone 都能当完整 server | 本地数据量大幅降低（“计算很少，但需要的数据也少得多”） | 计算集中到服务端；客户端无法独立回答问题；与依赖 `.git` 的工具生态断裂 |
| 3 | **Comprehensive service powers new apps**：客户端用的所有查询都以 API 暴露 | 回答仓库问题需本地 repo 或依赖厂商 API | 很多 job 无需本地 repo；自动 bisect / revert pipeline 才“在规模上可行” | API 面变成需要维护、限流（文中提到 rate limiting）和鉴权（IAM）的产品 |
| 4 | **Everything horizontally scalable**：数据分布式，任何地方（含服务端）都没有完整物化的仓库；无 packfile，无需仓库维护 | 仓库是一个可整体打包/复制的单元（mirror 模式） | 摆脱 mirror 的磁盘/内存天花板；无 repack/gc | 依赖 Spanner + blob storage 这类分布式底座；大量小对象的元数据开销；系统复杂度上移 |
| 5 | **Commits cheap, incremental, lock-free**：commit/rebase/merge 在服务端完成，无需 push；成本与变更大小成正比而非 repo 大小；除 main/release 外不鼓励 ref | 本地 commit 再 push；ref 是一等公民（分支随便开） | 服务端能扛住 3 万+/天写入；避免“所有服务器都要同意的全局状态” | 工作流被约束为 trunk-based；feature branch 文化受限；main ref 仍需 CAS（单行 Spanner），并发冲突交给 post-merge 测试和 Claude 修 |
| 6 | **Working copy optional, sparse by design**：diff 等读、以及很多写操作直接走 API；协议以 sparse 和 virtualized checkout 为中心 | checkout 是做任何事的前提；sparse 是事后补丁 | “只为实际读取的内容付费”；CI 无需 checkout 即可拿 merge-base diff | 需要遍历全树的工具（全局 grep、某些构建系统）会触发大量按需物化；虚拟文件系统的延迟与调试复杂度 |

### 原则之间的依赖（【推断】）

- **1 是地基**：只有“永远在线”成立，才能把计算挪到服务端（2）。
- **2 → 3**：计算既然在服务端，把客户端协议直接开放为 API 几乎零边际成本。
- **2 + 3 → 4、5**：所有人的读写都打到服务端，服务端必须水平扩展（4），且每次 commit 必须便宜（5），否则服务端就是新瓶颈。
- **5 依赖“不鼓励 ref”**：commit 与 ref 移动解耦，写入才能无锁。
- **1 + 3 → 6**：能问服务端，就不必 checkout；协议按 want 请求不可变对象，sparse/virtual 自然成立（文中称“不需要特殊支持，客户端只是不请求不要的 tree”）。
- **content-addressing 贯穿 4 和 6**：对象不可变 → 任何层可无协调缓存 → 水平扩展 + 分层 cache（>99% hit）。

## 5. 与先例对比（【外部】，文章只点名 Piper 和 Sapling，未说明借鉴了哪些具体教训）

- **Google Piper**：中心化 monorepo，存储在 Bigtable/Spanner 之上，trunk-based、极少分支；客户端通过 CitC（Clients in the Cloud）基于 FUSE 的虚拟工作区，只本地保存修改过的文件。Scion 的“Spanner + 永远在线 + 虚拟 checkout + 不鼓励 ref”与之高度同构。
- **Meta Sapling**（客户端）+ **Mononoke**（服务端）+ **EdenFS**（虚拟文件系统）：Mononoke 的 *bonsai changeset* 只记录相对父提交的文件变更，并有一套名为 **“derived data”** 的机制（blame、history、fsnode 等按父提交递归派生、可懒计算），commit 图有 skiplist / segmented changelog 加速祖先查询。Scion 的 changeset、“derived data”命名、skip list 几乎是同一套思路——这是我判断其最直接的借鉴来源。差异：Sapling 保留了较强的本地 commit/栈式工作流，Scion 更激进地把 commit 本身挪到服务端。
- **git 自身的缓解手段**：
  - *shallow clone*：砍历史 → merge-base、blame、log 失效或需 deepen。
  - *partial clone / promisor remote*（`--filter=blob:none`）：按需懒取 blob，但 blame/log -p 会触发大量串行往返；服务端仍需做 have/want 协商。
  - *sparse-checkout*（cone mode）：只缩小 working tree，不一定缩小对象下载量；与 partial clone 的组合存在边角问题（文章末尾也提出“sparse 和 shallow 功能可以做到更好地组合”）。
  - *Microsoft VFS for Git / Scalar*：虚拟化 + 预取，是 git 生态里最接近 Scion 第 6 条的尝试。
  - *mirror / seed*：本质是用复制换读带宽，复制单元是“整个仓库”，因此受单机上限约束，并在高合并率下陷入陈旧。
  - Scion 的关键差异：协商只基于“客户端想要的对象”，不告诉服务端自己有什么 → 每个请求都是不可变对象的点查，可被任意 cache 层回答，不需要生成 packfile。

## 6. 模糊与欠说明之处

1. **600 PB 的口径**：按 25 GB working copy 算，未考虑节点内去重；含历史的 full clone 会高出约 4 倍。
2. **10x vs 12x** 增长口径不一致。
3. **“永远在线”对人类开发者的笔记本**是否成立、断网时如何降级，未说明。
4. **commit 全在服务端**：未提交的本地修改如何上传、人类开发者本地有没有 commit 概念、不用分支时用什么标识进行中的工作（推测是 changeset ID / Antique 的 change），未说明。
5. **“lock-free” 与 “write-ahead log”** 的关系：changeset 是 WAL 条目，WAL 是否全局有序、如何在无协调下保证顺序，未解释；main ref 的 CAS 在 3 万+/天下的争用情况也未给数据。
6. **“任何地方都没有完整物化副本”** 与 scion-daemon “从定期刷新的 snapshot 启动”之间的关系——这个 snapshot 本质上是一种 seed，文章没有解释它为何不会重蹈 seed 陈旧的覆辙（【推断】：因为按对象增量补齐很便宜，陈旧代价从“大 packfile”降为“少量点查”）。
7. **目录级权限**与内容寻址、节点共享 cache 如何共存（cache 层如何做 ACL）未说明。
8. **从 Piper/Sapling 学到的具体教训**未列举。
9. 是否仍向外部导出 git 视图/兼容层，未提及。
