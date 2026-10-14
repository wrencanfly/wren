# Review 03 · 生态对齐、工作流经验与对外声明的诚实度

> 视角：Scion 原文非架构章节（"What we learned"、"Contributing to the wider ecosystem"）。分级：MUST-FIX 只用于错误或误导性的公开声明，或违背原文核心建议的地方；SHOULD 表示应该做；NICE 表示可以做。
> 复现目录：`scratchpad/review03/`，复用了 `scratchpad/bench/` 下原有的 `upstream.git` 和 `data/`。用只读方式起了一个 Flux server（`--poll 0`），没有修改任何代码或文档。

## 结论

架构方向和原文的生态建议高度一致。**但 `docs/benchmarks.md` 里三组最醒目的对比，git 一侧的基线配置有问题**，修正之后倍数大幅缩水。这是本次唯一的 MUST-FIX 类问题，修起来也最便宜：只需要改 bench 脚本和文档。

---

## MUST-FIX

### M1. sparse checkout 的 git 基线根本没有启用 `--filter=blob:none`

- **证据**
  - `bench/run.py:80` 通过 `GIT_CONFIG_PARAMETERS` 传入 `uploadpack.allowfilter`。但走 `file://` 时，git 会为本地仓库的 upload-pack 清掉这个环境变量。
  - 复现时 git 输出 `warning: filtering not recognized by server, ignoring`。
  - bench 留下的 `work/git-sparse/.git` 里只有一个 pack，`verify-pack` 显示其中有 **4,740 个 blob，也就是 HEAD 的全部 blob**，大小 13.2 MiB。
  - 旁证：表中 git sparse 用时 1057 ms，和 git 全量 1168 ms 几乎一样。
- **修正后的数据**：改用 `--upload-pack="git -c uploadpack.allowfilter=true upload-pack"`。

| | 用时 | 对象数 | 磁盘占用 |
|---|---:|---:|---:|
| git | ≈ 0.70 s | 1,966 | `.git` 4.98 MiB |
| Flux | 0.23–0.26 s | 1,966 | store 5.6 MiB |

- **影响**
  - 加速比从 **4.8× 降到约 3×**。
  - `benchmarks.md:29-31` 的体积注释方向反了：同样的对象，git 的 pack 带 delta 压缩，比 Flux 的 loose 存储更小。
- **修法**：修 `run.py`，重跑这一行，删掉或改写这条体积注释。

### M2. "warm node 12×" 和 "Git has no equivalent" 是拿稻草人对比

- **原文怎么说**：节点本地 mirror 或 seed 本来就是标准做法，原文写的是 "standard workarounds… internal git mirrors and repository snapshots (seeds)"。
- **bench 怎么测**：`benchmarks.md:25,33-35` 让 git 在第二个 job 时重新 clone。
- **实测**：节点上已有一个 partial clone 时，`git worktree add --detach`（会继承 sparse 配置，`git status` 干净）用 **0.14 s**，Flux 是 0.09 s，差距约 **1.6×**，不是 12×。
- **措辞问题**：`--reference refuses shallow repos` 本身没错，但推不出"每个 job 只能重新 clone"。
- **修法**：git 一侧改用 worktree 或 seed 作为基线，或者把这一行标成"对比无节点缓存的 git"，并删掉 "no equivalent"。

### M3. 查询对比用的 git 仓库没有 commit-graph

- **证据**：`bench/upstream.git/objects/info/` 是空的。`git commit-graph write --reachable --changed-paths` 是 `git maintenance` 和各大 forge 服务端的常规配置。对同一份对象补上 commit-graph 后实测：

| 查询 | 文档中的 git | git + commit-graph | Flux |
|---|---:|---:|---:|
| `log --first-parent -- builtin/blame.c` | 278 ms | **40 ms** | 12 ms |
| `merge-base --all` | 220 ms | **30 ms** | 20–30 ms（**持平**） |
| blame | 275 ms | **80 ms** | 11 ms warm / 约 200 ms 冷启动 |
| landed（`run.py` 中的配方） | 362 ms | **60 ms** | 7 ms |

- **影响**：10–25× 的倍数实际只有 1–7×。冷启动的 blame，Flux 反而比 git 慢。
- **修法**：git 基线加上 commit-graph；表格同时列出 Flux 的冷、热两种数字。真正站得住的卖点是 `benchmarks.md:41` 那句"git 需要 619 MB、约 73 s 的完整 clone，Flux 不需要"，应该把它作为主叙事，而不是靠倍数。

---

## SHOULD

### S1. "开放标准"的说法目前还没有可信度（A 部分）

查询 API 和对象协议现在只有两种形态：Rust 类型（`crates/flux-core/src/api.rs`）和一段文档注释（`crates/flux-core/src/wire.rs:9-17`）。缺失的部分：

- 没有独立的规范文件或 OpenAPI 描述；
- 没有 hash 算法字段，帧格式写死了 20 字节 oid（`wire.rs:13`）；
- rev 语法只是 git 的子集，只支持 `~` 和 `^`（`repo.rs:282-310`），不支持 `A..B`；
- 错误码靠匹配错误字符串推断（`server.rs:42-46`）；
- 没有能力发现机制。

forge 和 git 客户端都没法照着现在的东西实现。建议：

1. 写 `docs/spec/objects-v1.md` 和 `docs/spec/query-v1`，加入版本号和 object-format 字段。
2. 把这些接口和已有的同类能力逐一对照，说明 Flux 补的是哪一块：Gitaly 的 gRPC，GitLab 的 `merge_base` 和 blame API，GitHub compare 接口的 `merge_base_commit`。
3. 讨论能否把 want-only 取对象做成 git protocol v2 的一个 command。只有走到这一步，原文那句 "the git client can learn to talk to them" 才真正兑现。

### S2. 写清楚哪些部分真的能水平扩展

- 缓存层对查询 API 只是原样转发（`cache.rs:184-212`），所有查询最终都落到单机的 SQLite 加本地盘（`design.md:33`）。
- `README.md:77-78` 的 "any server can persist them without coordination"，以及 `design.md:13` 的"读路径可以水平扩展"，在 v0.1 只对**对象**成立。
- README 第 15-16 行 "never transfers anything outside its cone" 与 `--git` 模式不符：该模式会拉取全部 tree（`benchmarks.md:30-31`）。

建议在文字上区分"设计性质"和"v0.1 已实现"。

### S3. 为自动 bisect 补一个便宜的原语（B 部分）

- first-parent depth 已经算好了（`graph.rs:23`），`~N` 用 skip list 也能在 O(log n) 内完成。
- 但 `CommitInfo`（`api.rs:36-46`）只暴露了 generation，也没有"两点之间的 first-parent 距离"接口，bisect 驱动没法在 O(1) 内算出中点。
- 建议在 `CommitInfo` 里加一个 `depth` 字段。改动只有几行，正好对应原文 "automated bisect and revert pipelines"。revert 需要写路径，属于 v0.4 的非目标，这次不要求。

### S4. 差分测试还不能算 fuzzing

- 现状：
  - 只有一个固定种子（`differential.rs:490`）；
  - 生成器刻意避开了冲突、重命名和大块编辑（`differential.rs:63-65`）；
  - `benchmarks.md:68-75` 自己也承认，minimal Myers 的 bug 就是测试漏掉的。
- 建议：
  - 让种子可以通过环境变量设置，CI nightly 跑 N 个种子；
  - 加一种"大块移动"模式；
  - 加一个强推和 ref 删除的用例。`repo.rs:146-153` 的删除路径目前没有任何测试覆盖。
- 不建议现在就搭完整的 hermetic 故障注入框架。

### S5. README 补一句安全说明

- server 没有任何认证，连 `POST /sync` 也不需要（`server.rs:95`）。
- 默认只监听 127.0.0.1（`main.rs:50,65`），默认值是安全的。
- 但 strategy 文档里 MVP 承诺的"继承源站 token 的仓库级权限"（`research/03…:130`）并没有实现。

只需在 README 写一行：镜像的私有仓库对能访问该端口的所有人可读。

### S6. 把 sparse 与 promisor 组合的问题反馈给 git 上游

`design.md:127` 发现：`read-tree` 加 `sparse-checkout reapply` 会触发 promisor 去拉 cone 外的 blob。这正是原文所说 "sparse and shallow features… compose well" 的反例。整理出最小复现发到 git 邮件列表，比在 Flux 里绕开它对生态更有价值。

---

## NICE

- **N1.** `run.py:91` 的全量 checkout 没有带 `--git`，git 一侧却产出了完整仓库。我补测了带 `--git` 的版本，用时同为约 0.55 s，**数字不受影响**，只是建议补上以保证对比对等。
- **N2.** git 一侧的 landed 只跑了一次（`run.py:165-167`），与文档中"取 3 次中位数"的说法不符。
- **N3.** README 的 "generates random histories" 实际只有一个种子；cache 的 "stateless" 更准确的说法是 "disposable"（它会在磁盘上持久化对象）。
- **N4.** 服务端没有按客户端限流，只有缓存到上游的信号量（`cache.rs:30-32`）和批量上限（`wire.rs:24`）。v0.1 只监听本机，暂时不需要。

**我刻意没有升级的问题**：缓存层用 `put_raw` 写入对象，不校验 hash（`cache.rs:103`），只有客户端校验（`client.rs:220-227`）。端到端只在一处校验恰恰是正确的设计；如果在缓存层再加一道，就是原文说的 "overly defensive about trusting another internal service"。

---

## Flux 做对了的地方

1. **"不要扫 log"被写进了代码和流程。** `landed` 接口用 skip list 做二分（`graph.rs:211`），`design.md` §7 不变量 6 和 `CONTRIBUTING.md:7-8` 都明文禁止线性扫描 commit log。这正是原文里"工程师对 Claude 说 no"那个例子的书面版本。`CONTRIBUTING.md:12` 写的 "Reviews focus on intent and invariants first" 也和原文一致。
2. **把 git 当作 oracle。** 每个查询都有差分测试，bench 脚本还会断言结果一致（`run.py:147,153,170`）。文档主动披露了 blame 只有 98.8% 一致，以及"benchmark 抓到的 bug"。这种披露的习惯，正是修好 M1–M3 所需要的。
3. **定位符合原文给的建议。** 作为加速层不要求迁移，不自带 review、search、CI；`design.md:20` 明确说明，只要源站还能被直接 clone，目录 ACL 就不是安全边界，没有为了贴合原文而夸大。
4. **协商协议真的换了。** 对象协议是 want-only 的，对象不可变，每一层 RPC 都相同，`GET /v1/objects/{oid}` 带 `immutable` 头（`server.rs:157-163`），客户端做端到端校验。`--git` checkout 把 shallow、cone sparse、promisor 和 alternates 组合在一起后，`git status` 能立即返回干净结果。这是对原文"协商协议"和"sparse/shallow 组合"两条建议最具体的实践。
