# Review 05 · 协议是否体现了"重新设计 git 服务端"

> 视角：按用户对 Scion 原文的理解，Anthropic 公开设计，是希望开源社区、GitHub 和 GitLab 重新思考 git 的服务端：用高效的 RPC 查询机制和更灵活的协商协议，取代几十年前为人类设计的规则。本文把 `docs/spec/flux-protocol-v1.md`（第二稿）和参考实现（`crates/flux/src/{server,cache,client,checkout}.rs`、`crates/flux-core/src/{wire,api}.rs`），逐项对照 git 当前的 protocol v2。
> 分级：**MUST** 指 forge 或 git 上游评估时会被一票否决的问题，**SHOULD** 指应该做，**NICE** 指可以做。
> git 参照版本：git/git `6de20f6`（2026-10-06）。行号对应该版本的 `Documentation/*.adoc` 和源码，sparse clone 在 `scratchpad/research/git/`。
> 实测：在 `scratchpad/exp05/` 做了两组实验，一是**线上字节数**（Flux 帧和 git pack 对比），二是**对 github.com 的真实 WAN want-only 拉取**。实验没有改动 Flux 的任何代码和文档。复现命令见附录 B。

---

## 0. 结论

1. **"高效的 RPC 查询"基本做到了，而且是 git 协议里真正缺的东西。** `landed`、`log -- path`、`blame`、`bisect`、`merge-base` 都由服务端借助索引或派生数据回答，客户端不需要本地历史。git protocol v2 只有 `ls-refs`、`fetch`、`object-info`（仅 size 和 type，默认关闭）这几个与"查询"相关的命令。这一部分是 Flux 对原文诉求最实在的回应。
2. **"更灵活的协商"没有做到，Flux 只是把 git 的一种固定模式换成了另一种固定模式。**
   - want-only 的核心语义 git v2 **今天就已经支持**：只发 `want`、不发 `have`、直接 `done`，再配合 `filter tree:0`，服务端返回的正好就是所点名的那些对象。
   - git 自己的 promisor 懒拉取就是这么实现的（`promisor-remote.c:47-50` 使用 `fetch.negotiationAlgorithm=noop`）。我对 github.com 实测过（§3.1）：点名一个子树，`tree:0` 返回 1 个对象，`blob:none` 返回 124 个。
   - Flux 真正的新东西是另外三点：**响应可以按对象寻址和缓存**；**缺失对象不会让整个请求失败**；**有 GET 形式，可以走 CDN**。
   - 同时 Flux 去掉了 git 已有的选项：不能用 `have` 换增量、没有 delta、没有服务端闭包或深度控制。这算不上"更灵活"。
3. **代价已经可以量化，它大于基准测试呈现的样子。** 在 git/git 上实测线上字节数：
   - 冷启动全量 checkout：Flux 比 git pack 多 25%。
   - sparse（`Documentation`）：多 16%。
   - **暖节点增量**（同节点从 HEAD~15 换到 HEAD，只改了代码）：Flux 发 **570 KB**，git thin pack 只要 **13.6 KB**，相差 **42 倍**。
   - 跨 50 个合并（含翻译文件）：4.61 MB 对 1.35 MB，相差 3.4 倍。
   - `docs/benchmarks.md` 只在 localhost 上测了时间，**没有记录线上字节数，也没有任何真实网络延迟下的数据。**
4. **按现在的形态，forge 或 git 上游都无法直接采纳。** 一票否决项有四个：
   - 对象命名空间是整个 server 共用的，token 不分仓库（§2.1、§3.2）。这正是 GitHub 2024 年 "Cross Fork Object Reference" 问题的同一种形态。
   - pinned 答案的不可变承诺和算法实现绑死了，修一次 blame 的 bug 就要升协议大版本。
   - 错误没有机器可读的错误码。
   - 新鲜度和一致性语义只有一个 `last_sync` 时间戳。
5. **建议换个定位，走双轨：**
   - (a) 现有 HTTP/JSON API 定位成"forge REST profile"。GET 加 `immutable` 的可缓存性是它相对 git 的真正优势，应当保留。
   - (b) 另写一份《Proposed git protocol v2 extensions》，只提 git 确实缺的部分：`query` 命令族、`fetch` 的 `missing-ok` 特性、原始对象 URI、`object-info` 新属性，再加用 `promisor-remote` 宣告 Flux 缓存层。
   - (c) 在 Flux server 上实现 `git-upload-pack` v2 前端，作为参考实现。

   优先级清单见 §5。

---

## 1. 原文诉求与 Flux 的对应

按 `docs/research/03-lessons-and-public-strategy.md` §4 的整理，原文对 git 生态提了四点建议。

| 原文建议 | Flux 的对应物 | 评价 |
|---|---|---|
| git server 不必与客户端同构 | 服务端用 changeset、派生数据、loose 对象存储，mirror 只保留 refs | ✅ 做到了，但它是挂在 forge 前面的旁路，不是 forge 本身 |
| forge 提供全面的服务端 RPC，最好跨 forge 标准化 | `/api/v1/repos/{repo}/...` 下 14 个端点，配有 git oracle 差分测试 | ✅ 方向对。可标准化的程度见 §4 |
| 协商协议不必是今天的样子 | want-only `POST /v1/objects` | ⚠️ 语义上 git v2 已经能表达（§2.1）；真正的新意在可缓存性，而灵活性反而降低了 |
| sparse 与 shallow 能良好组合 | `--git` 模式写 sparse index，只拉边界 tree，用 `shallow` 加 promisor | ✅ 做得好，还有一份待发给上游的问题报告（`docs/upstream/git-sparse-promisor.md`） |

---

## 2. 逐项对照 git protocol v2

### 2.0 总表

| Flux 设计 | git v2 是否已有 | 能否表达为 v2 capability 或 command | 是否根本在 git 模型之外 |
|---|---|---|---|
| want-only 对象拉取（不发 have，不算闭包） | **基本已有**：`fetch` 只带 `want` 加 `done`，配 `filter tree:0` | 已经能表达。只缺"缺失对象不报错"，可以加 `fetch` 特性 `missing-ok` | 否 |
| 逐对象 loose 帧 | 否，v2 只传 pack | 可以，做成新命令 `object-get`，或者用 packfile-uris 的原始对象 URI（git 自己在 future work 里列过） | 否。loose 编码本来就是 git 的格式 |
| 每层缓存 RPC 相同，响应不可变、可缓存 | 部分：v2 无状态，可以轮询负载均衡；有 bundle-uri、packfile-uris、promisor-remote 卸载流量 | 对象部分可以借 `promisor-remote` 宣告缓存层。"响应可缓存"无法在 smart HTTP 的 POST 里表达 | **HTTP 缓存语义在 git 协议之外**，只能定义新的 HTTP 绑定 |
| 服务端查询 RPC（log、blame、merge-base、landed、bisect、tree、file） | 否。只有 `object-info`（size、type）和 `ls-refs` | 可以。v2 本来就按"命令可扩展"设计（v2:19-23、162-168），`object-info` 加 `remote-object-info` 是现成的先例 | first-parent 语义、`landed`、`bisect-step` 是 git 里没有的概念，但只要输入输出都是 oid，就不和 git 模型冲突 |
| pinned 查询的不可变性 | 否 | 可以在能力值里声明某个命令是"纯函数"，但缓存仍然需要 HTTP GET 绑定 | 部分在外：git 协议没有缓存语义 |
| 能力声明 | **git 更强**：`key=value`，命令可以带特性列表，客户端在请求中回显能力 | — | — |
| refs 与修订解析 | `ls-refs`（带 `ref-prefix`、`symrefs`、`peel`、`unborn`）、`want-ref` | Flux 的 `refs` 是 `ls-refs` 的弱化版；`resolve` 接近 `want-ref` 加 rev-parse | 否 |

### 2.1 want-only 对象拉取

**git 现状**

- `want` 可以是任意 oid：`gitprotocol-v2.adoc:256-259` 写明 "Wants can be anything and are not limited to advertised objects"。v2 的 `parse_want` 只检查对象存不存在，不检查可达性（`upload-pack.c:1437-1468`）。
- 请求里没有 `have` 时，服务端跳过 ACK 阶段，直接生成 pack（`upload-pack.c:1765-1771`，注释原文是 "Request had 'want's but no 'have's so we can immediately go to construct and send a pack"）。
- 闭包可以关掉：`filter tree:0` 不包含任何 tree 或 blob，"unless included explicitly"（`rev-list-options.adoc:1003-1007`）。显式给出的对象不受过滤器影响（同文件 :993-996）。
- git 的 promisor 懒拉取已经用的是这种模式：`promisor-remote.c:47-50` 用 `fetch.negotiationAlgorithm=noop` 加 `--filter=blob:none --stdin`。`partial-clone.adoc:176-177` 写的是 "does not perform any packfile negotiation"。
- 但懒拉取默认用 `blob:none`，所以要一个 tree 时会**连同整棵子树**一起拉下来。`partial-clone.adoc:180-182` 自己承认 "fetches all objects referred to by the requested objects, even though they are not necessary"。

**实测（github.com，真实 WAN）**

| 请求 | 返回对象数 | pack 字节数 |
|---|---:|---:|
| `want <git.git HEAD:t 的 tree oid>`，`filter tree:0`，noop 协商 | **1** | 36,995 |
| 同上，`filter blob:none` | 124（整个子树的 tree） | 80,339 |
| 4,965 个 blob 的 oid，noop 协商，`blob:none` | 4,965 | 14.1 MB，**用时 5.5 s** |

**结论**

- "客户端只说要什么、不说有什么"，在 git v2 里是**一种请求写法**，不是新协议。GitHub 今天就支持。
- spec §3.6 和 §9 把它当作 Flux 的核心创新，`docs/benchmarks.md` 也把 checkout 的优势归因于 "no have/want negotiation"。这两处说法都需要收窄。
- 还要注意：基准里 git 一侧是**冷 clone，本来就不发送任何 `have`**，所以那 2.3 到 2.6 倍的差距不可能来自"省掉协商"。更可能的来源是进程启动、ref 广告（1,019 个 ref）、pack 生成（delta 搜索）、客户端 `index-pack` 和 checkout。这需要用 `GIT_TRACE2_PERF` 拆开看（§5 P2-4）。

**Flux 相对 git 真正不同的地方**

1. **缺失对象容错**：Flux 用帧状态 `0x01` 表示缺失，其余帧照常返回（spec §3.2）。git 的 `parse_want` 遇到未知 oid 会直接 `die("not our ref")`，整个请求失败。
2. **按对象寻址**：每个对象的响应都是一个独立的不可变单元，可以在任意一层缓存（spec §3.5）。git 的 pack 是一次性的整体响应。
3. **续传**：失败后只重新请求缺的那些（`client.rs` 里的 `ensure`，最多 4 轮）。git 的 packfile-uris 说可以提供 "some measure of resumability"，但仍是未来工作（`packfile-uri.adoc:6-7`、`72-76`）。

这三点都能以扩展的形式加进 git v2，见 §5 P2-1。

### 2.2 逐对象 loose 帧

- git v2 只有 pack，没有 loose 对象这种传输格式。但 git 社区自己在 `packfile-uri.adoc:78-82` 的 future work 里写过 "Different file formats referenced by URIs (e.g. raw object)"。所以"原始对象 URI"有现成的切入点。
- Flux 选择 loose 帧，好处有三个：缓存层和客户端都不需要重新编码；任意一层都能逐个校验对象；单对象可以走 `GET /v1/objects/{oid}` 和 CDN。
- **代价是没有任何 delta**。这一点 §3 用实测量化了。
- 另外 loose 编码的 zlib 级别没有规定（spec §3.1），参考实现用的是 `Compression::fast()`（`object.rs:208`）。同一个 oid 在两台 server 上的字节可能不同。这对"响应不可变"的 HTTP 缓存语义（ETag、CDN 去重）不太友好，但不影响正确性。

### 2.3 每层缓存 RPC 相同，响应不可变、可缓存

**git 里相近的机制**

- **无状态和代理**：v2 "stateless by default… permits simple round-robin load-balancing"（`gitprotocol-v2.adoc:170-176`），并且 "the http remote helper can simply act as a proxy"（:28-29）。
- **CDN 卸载**：`bundle-uri`（v2:602-801）用来卸载 clone，`packfile-uris`（v2:366-377、486-498）用来卸载 pack 的一部分。服务端还可以通过 `uploadpack.packObjectsHook`（v2:631-637）缓存 pack-objects 的结果。据 GitLab 公开文档，Gitaly 的 pack-objects cache 就是这么做的。
- **宣告更近的远端**：`promisor-remote=<pr-info>` 可以给客户端宣告一个"连接更好"的远端，还能带上 `token` 字段（v2:803-900）。按 v2:890-900 的说法，让服务端省略这些对象、由客户端去更近的远端懒拉取，目前还是 future work。

**git 没有的**

- **HTTP 层的不可变缓存**：smart HTTP 的数据请求都是 `POST $GIT_URL/git-upload-pack`，HTTP 中间层无法缓存。
- **"每层同一套 RPC、客户端不感知拓扑"的分层树**：git 也没有这个概念。

**评价**：这是 Flux 相对 git 最有价值的设计之一。要表达成 git 的形式，需要新的 HTTP 绑定，单靠一个新的 v2 capability 不够。对象部分可以先用 `promisor-remote` 把 Flux 缓存层宣告给原生 git 客户端。这样还能顺带修复 `design.md` §10 写到的"懒拉取直连源站，绕过了缓存层"。

**实现层面的问题**

| 问题 | 位置 |
|---|---|
| 缓存层宣告的 `features` 是写死的，不检查上游 | `cache.rs` 的 `capabilities`（spec #18） |
| 没有 singleflight，同一对象的并发未命中会各自回源 | `design.md` §10 |
| 查询缓存的 key 是原始的 `path?query`，参数换个顺序就算不同的 key | spec #19 |
| 缓存层**完全不做授权**：任何持有缓存层 token 的客户端，都能读到缓存里所有仓库的不可变响应 | — |

最后一条是 §4 的一票否决项之一。

### 2.4 服务端查询 RPC

- git v2 里和查询有关的只有两个命令：
  - `object-info`：只返回 size 和 type（v2:556-600），而且**默认不宣告**（`serve.c:95-106`，以及 `transfer.advertiseObjectInfo` "Defaults to false"）。
  - `ls-refs`。
- 客户端的 `git cat-file --batch-command` 已经有 `remote-object-info`（`git-cat-file.adoc:172-177`）。这说明上游接受"在 v2 里加服务端查询命令，再加客户端 plumbing"这条路，只是目前只做到了 size 和 type。
- Flux 的端点和 git plumbing 的对应如下：

| Flux 端点 | 对应的 git 命令 | 能否直接做成 v2 命令 |
|---|---|---|
| `merge-base` | `git merge-base --all` | 能 |
| `is-ancestor` | `git merge-base --is-ancestor` | 能 |
| `log?path=` | `git rev-list --first-parent <oid> -- <path>` | 能，需要定义 path 的字节编码 |
| `blame` | `git blame --first-parent --porcelain` | 能，但答案依赖 diff 算法（§2.5） |
| `landed` | 没有对应的单条命令 | 能。这是新概念，但输入输出都是 oid |
| `bisect` | 没有对应的单条命令（`git bisect` 是有状态的） | 能。Flux 的无状态单步设计正好适合 v2 的无状态模型（v2:170-176） |
| `tree`、`file` | `ls-tree`、`cat-file` | 和对象拉取重叠，对 git 意义不大 |
| `diff?patch=true` | `git diff` | 输出格式没有规定（spec #16），不建议标准化 |

- **评价**：查询能力本身完全可以表达成 v2 命令，而且 v2 的设计就是为这类扩展准备的。真正的障碍在语义层面：
  - 所有历史类查询都固定为 first-parent，没有全图版本；
  - 不跟踪重命名；
  - blame 与 git 只有约 98.8% 一致。

  这三点都需要在命令名或能力值里显式标出来，比如 `blame` 命令要声明 `first-parent no-renames diff=myers-minimal`。否则 forge 实现时没有标准可以对齐。

### 2.5 pinned 查询的不可变性

- **设计本身是对的**：一个查询的所有修订都是完整 oid，答案就是不可变输入的纯函数，可以标成 `Cache-Control: immutable`（spec §7.1-7.3）。git 协议里没有对应的东西。
- **但现在的实现有三个结构性问题。**
  1. **pinned 靠 URL 启发式判定。** `server.rs:160` 的 `is_pinned` 对原始字符串做匹配：不做百分号解码，只认小写，参数名只要叫 `rev`、`a` 之类就算数。spec #19 已经承认这一点。根子在于**修订解析和查询混在同一个请求里**。git v2 把两件事分开了：`ls-refs` 和 `want-ref` 负责解析，`fetch` 只接受 oid。如果查询端点只接受完整 oid，pinned 就由结构保证，不再需要启发式（§5 P0-2）。
  2. **不可变承诺和算法实现绑死。** spec §7.3 规定，pinned 答案一旦改变就必须升协议大版本。blame 只要修一次对齐 bug，比如向 git 的 indent heuristic 靠拢，就得发 `/api/v2`。第二稿里已经因为 `mode` 的类型、`exact` 字段和 bisect 下界改过三次 pinned 答案（spec #26）。forge 不可能接受这种耦合。
  3. **授权不在缓存 key 里。** 对外宣布 `public, immutable`，就等于允许任何 CDN 把私有仓库的 blame 缓存下来、发给任何人。对 forge 来说，私有仓库必须用 `private` 或者带签名的 URL。

### 2.6 能力声明与版本

| 方面 | git v2 | Flux v1 |
|---|---|---|
| 能力形态 | `key[=value]`，命令可以带特性列表，例如 `fetch=shallow filter`（v2:115-124、204-206、250-252） | 平铺的字符串列表，不能带参数 |
| 客户端回声 | 请求里携带自己用到的能力（v2:135-142）、`agent`、`session-id`、`object-format` | 没有。服务端不知道客户端支持什么 |
| 限额宣告 | 多由能力值表达 | `MAX_BATCH = 10000`、log 上限 10,000、帧长度上限 4 GiB 都**没有宣告** |
| 哈希算法 | `object-format=sha256` 已经能用 | `object_format` 字段存在，但帧格式写死 20 字节（spec #17） |
| 版本协商 | 客户端在 `Git-Protocol: version=2` 请求头里说明 | 用路径前缀加响应头 `flux-protocol: 1`，客户端没有办法表达自己的版本 |

**评价**：这一项 Flux 比 git 弱。不过第二稿刚把 v1 定为"未冻结"，现在是改的最佳时机。

---

## 3. 强与弱：实测和推算

### 3.1 线上字节数实测（git/git，HEAD=`6de20f6`）

**方法**

- Flux 帧大小 = Σ(25 + zlib level 1 压缩后的 `"<type> <size>\0<content>"`)，对应 `wire.rs` 的帧格式和 `object.rs:208` 的 `Compression::fast()`。我用 Python 的 zlib 重新计算。flate2 的后端可能有几个百分点的出入。
- git 的数字是同一组对象经本地 `git pack-objects --stdout`（默认 window 和 depth）得到的大小。增量场景用 `--revs --thin` 加 `X1 ^X0`，其中 X0、X1 是只挂了 T0、T1 两棵树的合成 commit。
- 另外测了"对 Flux 帧流再做 gzip"的效果，对应启用 HTTP 压缩。

| 场景 | 对象数 | 原始大小 | Flux 帧 | Flux 帧加 gzip | git pack | Flux / git |
|---|---:|---:|---:|---:|---:|---:|
| 冷节点全量 checkout（HEAD 整棵树） | 4,962 | 50.4 MB | 16.68 MB | 16.56 MB | 13.37 MB | **1.25×** |
| 冷节点 sparse，cone=`Documentation`（含根目录文件和边界 tree） | 1,551 | — | 5.36 MB | 5.32 MB | 4.64 MB | **1.16×** |
| 暖节点增量，HEAD~15 到 HEAD（15 个第一父合并，58 个文件，+520/−158 行，纯代码） | 68（58 个 blob） | 1.45 MB | **570 KB** | 570 KB | thin pack **13.6 KB** | **≈42×** |
| 暖节点增量，HEAD~50 到 HEAD（230 个文件，含 12 个 `po/*.po` 大改） | 256（230 个 blob） | 14.6 MB | 4.61 MB | 4.61 MB | thin pack 1.35 MB | **3.4×** |

**解读**

- **冷启动差距不大**，只有 16% 到 25%。pack 的 delta 只能在同一批对象之间找，git 在这里优势有限。
- **暖节点增量差距很大。** 这恰好是 Flux 声称最擅长的场景：CI 和 agent 节点反复 checkout 新提交。
  - Flux 客户端会跳过本地已有的 oid（`client.rs` 的 `ensure`），所以不会重发相同的对象。
  - 但每个**改过的文件**都要整份重发。git 的 thin pack 则用旧版本作为 delta 的基底。
  - git.git 的典型合并是在大 C 文件里改几行，这时差距达到 42 倍。
- **HTTP gzip 几乎没用**，不到 1%。loose 对象本身已经是 zlib 压缩的，跨对象的冗余只能靠 delta 消除。
- **"客户端已经通过其他途径拿到对象"的情况，Flux 完全看不到。** 例如节点上已有一个普通 git clone 的 pack，或者 CI 缓存里有 bundle。Flux 的 `ensure` 只查 Flux 自己的 loose 存储。git 可以用 `have` 或 bundle-uri 的 tips 利用这些对象（v2:708-713）。

### 3.2 往返次数（推算，未实测）

- Flux checkout 每层 tree 需要一个顺序往返（spec §3.6 末尾承认 "one round trip per tree level"）。git.git 的 tree 最深 7 层（每层 tree 数：31、118、47、16、6、5、1）。
- 全量 checkout 的顺序往返大约 11 次：`resolve` 1 次、commit 1 次、tree 8 层、blob 1 次，`--git` 模式再加 `repo_info` 1 次。
- 换成 RTT 50 ms 的链路，**仅等待延迟就有约 0.55 s**。而 localhost 上测得 Flux 全量 checkout 一共才 650 ms。
- git 的 `clone --depth=1` 只需要 3 次往返左右：`info/refs`、`ls-refs`、`fetch`。
- 推论：真实网络下，2.3 倍的优势会明显缩小。在目录很深的 monorepo（10 到 20 层）上甚至可能反转。
- 这些都没有测过。`docs/benchmarks.md` 的 Caveats 只说了 "cache tiers were not measured under real network latency"，没有点出往返次数这个结构性因素。

### 3.3 其他弱点

| 弱点 | 证据 |
|---|---|
| **没有流式传输**：server 把整批帧拼进一个 `Vec` 再发；cache 在 `fetch_upstream` 里 `resp.bytes().await` 读完整个上游响应；client 收完全部帧才开始校验 | `server.rs:267-283`，`cache.rs:148`、`:321`，`client.rs:218` |
| **只有 HTTP/1.1**：reqwest 没开 `http2` 特性，axum 默认也不开；靠 16 条并行连接弥补 | `crates/flux/Cargo.toml:15`，`client.rs:16` |
| **没有进度和保活**：长时间的 blame 或大批量拉取没有心跳。git 有 sideband 2 和 `sideband-all` 的 keepalive | v2:356-364、512-522 |
| **单个对象不能超过 4 GiB**，超大对象会让整批请求失败 | spec #24 |
| **只支持 SHA-1**，帧格式写死 20 字节 | spec §1.5、#17 |
| **没有分页**：log 只有 `limit`，超过 10,000 会被静默截断 | spec §4.5、#20 |
| **不支持范围**，例如 `A..B` | spec §5.4 |
| **非 UTF-8 内容有损**：路径、消息、blame 内容转成 JSON 时有损，非 UTF-8 路径无法寻址 | spec #15 |
| **鉴权**：单一 bearer token，覆盖整个 server | spec §2.1、§11 |

### 3.4 强项（有数据支撑）

- **不需要 clone 就能查询历史。** git 一侧光准备工作就要 5.5 s 和 117 MiB（blobless bare clone，`file://`）。Flux 的暖 blame 14 ms，`landed` 7 ms（git 要 90 ms），`log -- path` 15 ms（git 要 50 ms）。数据来自 `docs/benchmarks.md`。
- **精确的对象集合。** 边界 tree 只拉一层，cone 外的 blob 一个都不拉。这一点 git 用 `tree:0` 也能做到，但 git 客户端的懒拉取路径默认是 `blob:none`，会拉整棵子树。
- **单对象 GET 可以无限期缓存**，CDN 友好。
- **缺失对象容错，失败可以续传。**
- **缓存层不需要被信任**：客户端逐个对象校验哈希，spec §3.4。
- **规范写得认真**：带 git oracle 的差分测试，open issues 编号稳定。这在同类项目里很少见。

### 3.5 没有测量过的东西

1. 真实网络下的延迟和带宽，包括 RTT 以及 cache 层在跨区域部署时的表现。
2. **线上字节数**：`checkout::Report` 里有 `bytes_fetched`，但 `bench/run.py` 没有记录。§3.1 是我补测的。
3. **服务端 CPU**。"git 服务端要为每个客户端生成 pack"是 Flux 的立论前提（`docs/research/01` 第 38 行），却从来没有测过 upload-pack 和 Flux server 每个请求的 CPU 开销。
4. 并发负载下缓存的命中率，以及没有 singleflight 时的回源放大。
5. 仓库规模的影响。git.git 相对 Scion 面对的场景太小，这一点 benchmarks.md 自己也承认。
6. 增量场景：暖节点切换到新提交。现有基准里完全没有这个场景，而 §3.1 显示 Flux 在这里最弱。

---

## 4. forge 或 git 上游能否采纳

### 4.1 缺口清单

| 维度 | 现状 | forge 的最低要求 | 级别 |
|---|---|---|---|
| **对象授权** | `/v1/objects` 的命名空间是整个 server，token 不区分仓库（spec §3.2、#10） | 对象路由必须带仓库，server 要验证 oid 属于该仓库或从该仓库可达。GitHub 2024 年的 CFOR 事件就是 fork 网络共享对象池导致的跨仓库对象读取，Flux 现在的设计是同一种形态 | **MUST** |
| **缓存层授权** | 缓存层用自己的 token，把上游返回的不可变响应发给任何客户端 | 缓存层必须能独立验证"这个客户端能不能读这个仓库"，例如用带仓库 claim 的签名 token；私有仓库的响应不能标 `public` | **MUST** |
| **答案语义的版本** | 改变 pinned 答案就必须升大版本 | 每个查询有独立的语义版本，并且这个版本进入缓存 key | **MUST** |
| **错误模型** | 只有 `{"error":"文本"}`；框架层报错是 text/plain 或空 body；分类不清的一律是 500（spec #5、#23） | 机器可读的 `code`、`retryable`，以及 `429` 加 `Retry-After` | **MUST** |
| **一致性和新鲜度** | `last_sync` 只出现在 `/repos` 列表里；push 之后读不到自己的写入；`sync` 任何人都能调用 | 每个依赖分支的响应都带快照标识；支持"至少同步到某个 oid"；用 webhook 驱动同步 | SHOULD |
| **扩展模型** | 平铺的 feature 列表 | 特性带参数，有注册表，客户端宣告自己的能力 | SHOULD |
| **限流和成本** | 没有。冷 blame 要 622 ms，属于高成本请求 | 按 token 或仓库配额，返回成本提示 | SHOULD |
| **多仓库和命名** | `{repo}` 只能是一个路径段 | `owner/name`、fork 网络、改名后的重定向 | SHOULD |
| **写路径** | 没有。依赖 `git push` 加轮询 | 至少规定 push 之后如何触发同步，以及 `landed` 的可见性窗口 | SHOULD |
| **分页、范围、二进制路径** | 都没有 | cursor、`A..B`、base64 编码的字节路径 | SHOULD |
| **SHA-256** | 没有定义 | GitLab 等平台已经在推进 SHA-256 仓库，至少要定义好帧格式 | SHOULD |
| **HTTP 层一致性测试** | `differential.rs` 在进程内调用 `flux-core`（spec #13） | 一套能对任意 HTTP 实现运行的测试，供 forge 自测 | SHOULD |

### 4.2 是否应该改成"git protocol v2 扩展提案加参考实现"

**建议改，但走双轨，不要二选一。**

- **v2 扩展和 HTTP/JSON API 的分工：**
  - 只做 v2 扩展会丢掉 Flux 最大的优点。git smart HTTP 是 POST 加 pkt-line，CDN 和 HTTP 缓存都用不上。
  - 只做 HTTP/JSON API，在 git 上游看来就是"又一个 forge REST API"，客户端也学不会调用它。原文希望的是 git 客户端学会调用服务端 RPC。
- **具体分三条线：**
  1. **Flux HTTP Profile（现有 spec 的演进版）**：给 forge 和 CDN 用，强调 GET、不可变、可缓存。
  2. **git protocol v2 扩展提案**：只提 git 确实缺的东西，见 §5 P2-1。这份提案要先承认，want-only 用 `want` 加 `done` 加 `tree:0` 已经能表达，否则在 git 邮件列表上第一封回复就会指出这一点。
  3. **参考实现**：Flux server 同时提供 `git-upload-pack` v2，支持 `ls-refs`、`fetch` 和新命令。原生 git 客户端通过 `promisor-remote` 被引到 Flux 缓存层。这样可以证明两条线讲的是同一套语义。
- 现在 spec §9 只是把这条路线"顺带一提"，标注为 "Nothing here is implemented"，`design.md` §12 把它放在"持续"一栏。按原文的说法，这类问题 6 到 12 个月内会变得普遍，窗口期很短。应该把它提到路线图的最前面。

---

## 5. 优先级改动清单

### P0：在和任何 forge 接触之前必须完成（趁 v1 还没冻结，直接在 v1 里改）

1. **对象路由带上仓库**（MUST）
   - 新增 `POST /v1/repos/{repo}/objects` 和 `GET /v1/repos/{repo}/objects/{oid}`，删除全局的 `/v1/objects`。
   - server 维护一个对象到仓库的成员索引。ingest 时已经逐个吸收对象，可以顺手写入。
   - 不属于该仓库的 oid 一律返回 `missing`，不返回 403，以免泄露对象是否存在。
   - capabilities 增加 `"object-scope": "repo"`。
2. **把修订解析和查询分开，pinned 由结构保证**（MUST）
   - 新增一组只接受完整 oid 的端点，例如 `GET /api/v1/repos/{repo}/at/{oid}/blame?path=`、`.../at/{oid}/log?path=&limit=`、`.../merge-base?a=<oid>&b=<oid>`。参数不是 40 位小写十六进制的直接返回 400。
   - 这组端点**一律**标为 immutable。现有接受分支名的端点保留作"便利层"，永远不缓存。
   - 删掉 `is_pinned` 的字符串启发式。这样也解决了 spec #19。
3. **给答案加语义版本**（MUST）
   - capabilities 增加 `"semantics": {"blame": "fp-myers-1", "log": "fp-1", "bisect": "fp-depth-2", "changes": "no-renames-1"}`。
   - pinned 的 URL 带上版本，例如 `/api/v1/repos/{repo}/at/{oid}/blame@fp-myers-1?path=`；不带版本时 302 重定向到当前版本。
   - 以后修 blame，只需要发布 `fp-myers-2`。旧版本的 URL 可以下线，返回 410，但永远不会悄悄变成另一个答案。spec §7.3 也随之改写。
4. **规定机器可读的错误模型**（MUST）
   - 统一格式：`{"error":{"code":"unknown_revision","message":"…","retryable":false}}`，同时保留原来的 `error` 字符串一个版本以便兼容。
   - 定义一张错误码表：`unknown_repo`、`unknown_revision`、`ambiguous_revision`、`invalid_revision_syntax`、`path_not_found`、`object_too_large`、`batch_too_large`、`upstream_unavailable`、`no_upstream`、`rate_limited`。
   - 框架层的 400、415、422、404、405 都套上同一种 ErrorBody（spec #5）。`sync` 在没有 upstream 时返回 409，上游 fetch 失败返回 502（spec #23）。
5. **私有响应不标 `public`**（MUST）
   - 开启鉴权时，pinned 响应改为 `Cache-Control: private, max-age=31536000, immutable`。Flux 缓存层依据另外一个响应头 `Flux-Cacheable: immutable` 判断能否落盘，不再看 `Cache-Control` 里有没有 `public`。
   - 缓存层的 key 加上仓库和授权域。

### P1：协议效率，回应"更灵活的协商"

1. **基于基底的 delta 帧**（SHOULD，回应 §3.1 的 42 倍差距）
   - `POST /v1/repos/{repo}/objects` 的请求体增加可选字段 `"bases": {"<want-oid>": "<base-oid>"}`。客户端只为单个对象提议基底，通常是同一路径下的旧版本 oid，不需要描述自己拥有的全部对象，want-only 的精神不变。
   - 新增帧状态 `0x02 = delta`：在 `len` 之后跟 20 字节的 base oid，再跟 zlib 压缩的 git delta 数据，格式和 pack 里的 `OBJ_REF_DELTA` 相同。服务端可以忽略提议，照常返回完整对象（`0x00`）。
   - delta 帧本身也是不可变的，可以提供 `GET /v1/repos/{repo}/deltas/{base}/{oid}`，让 CDN 缓存。
   - capabilities 宣告 `"delta-frames": {"max_chain": 1}`。
   - 客户端在 checkout 新提交时，用旧 checkout 的 tree 为每个改动的路径找到基底。
2. **一次性取得清单，消除逐层往返**（SHOULD，回应 §3.2）
   - 新增 `GET /api/v1/repos/{repo}/at/{oid}/manifest?cone=a/b&cone=c&trees=boundary`，返回 cone 内所有条目的 `(path, mode, oid)`，以及边界 tree 的 oid。
   - 这个请求是 pinned 的，可以缓存。客户端拿到 oid 后，一轮并行请求就能拉齐 tree 和 blob。
   - 顺序往返从"tree 深度加 3 次"降到 3 次左右。由于 cone 参数会变，响应体可能较大，需要定义分页（`next` cursor）。
3. **打包后的批量响应作为可选项**（NICE）
   - `POST .../objects` 支持 `Accept: application/x-git-packfile`，返回同一组对象的 pack（只在批次内部做 delta，不能缓存）。
   - 默认仍然返回 `application/x-flux-objects`。这样客户端可以按场景在"可缓存"和"省带宽"之间选择，这才是"协商"的本来含义。
4. **能力声明改成带参数的对象**（SHOULD）
   ```json
   "features": {
     "objects": {"max_batch": 10000, "max_object_size": 4294967295, "formats": ["loose", "delta", "pack"]},
     "query": {"ops": ["log", "blame", "merge-base", "is-ancestor", "landed", "bisect-step", "manifest"], "max_log": 10000},
     "object-format": ["sha1"]
   }
   ```
   客户端通过请求头 `Flux-Client: features=delta,manifest; agent=flux/0.2` 宣告自己的能力，对应 git v2 的能力回声和 `agent`。
5. **帧格式 v1.1：长度可变的 oid 和 64 位长度**（SHOULD）
   - oid 的长度由 `object_format` 决定（20 或 32 字节），`len` 改为 u64。
   - 增加状态 `0x03 = too_large`，修复 spec #24 里"一个大对象拖垮整批"的问题。
   - 用 `Content-Type: application/x-flux-objects; v=2` 区分新旧帧格式。
6. **流式传输和 HTTP/2**（SHOULD）
   - server 和 cache 用 `Body::from_stream` 逐帧发送，cache 边收边转发，client 边收边校验。
   - 开启 reqwest 的 `http2` 特性和 axum 的 `http2` 特性。
   - 长请求加可选的 NDJSON 进度流，或者 keepalive。
7. **新鲜度语义**（SHOULD）
   - 每个依赖分支的响应都带上 `Flux-Synced-At: <unix>` 和 `Flux-Refs-Snapshot: <seq>`。
   - 新增 `GET .../resolve?rev=main&min_synced_at=<unix>`：server 的同步还没跟上时，返回 `409 stale` 加 `Retry-After`。
   - `POST .../sync?want=<oid>` 只在 oid 还不认识的时候触发同步，降低 DoS 面。
   - 增加 webhook 接收端点。
8. **log 分页**（SHOULD）：响应里加 `"next": "<oid>"` 和 `"truncated": true`（spec #20）。

### P2：采纳路线

1. **起草《Proposed git protocol v2 extensions》**，放在 `docs/upstream/`，内容如下：
   - **(a) want-only 写法说明**：承认 `fetch` 加 `want` 加 `done` 加 `filter tree:0`，再用 noop 协商，已经能做到精确的对象集合，并附上 §2.1 的实测数据。
   - **(b) `fetch` 新特性 `missing-ok`**：在能力里宣告为 `fetch=… missing-ok`，响应增加一个 `missing-objects` 段，取代 `die("not our ref")`。
   - **(c) `object-uris`**：作为 packfile-uris 的扩展，响应段格式为 `object-uri <oid> <uri>`，把 `packfile-uri.adoc:82` 的 "raw object" 落成具体设计，让 CDN 可以逐对象分发。
   - **(d) `object-info` 增加属性**：例如 `generation`、`depth`。这是最小、最容易被接受的第一步。
   - **(e) `query` 命令族**：在能力里宣告为 `query=merge-base is-ancestor log-first-parent blame-first-parent landed bisect-step`。所有参数都只接受 oid。响应用 pkt-line。能力值中声明 `semantics=<ver>` 和 `pure`。
   - **(f) 用 `promisor-remote` 宣告 Flux 缓存层**：带上 `url` 和 `token` 字段，并推动 v2:890-900 写的 future work，也就是由服务端省略这些对象、让客户端去更近的远端拿。
2. **参考实现**：`flux server` 增加 `/{repo}.git/git-upload-pack`（v2），支持 `ls-refs`、`fetch`（只支持 `tree:0` 和 `blob:none`）以及上面的 (b)、(d)、(e)。e2e 测试用原生 git 的 `cat-file --batch-command` 里的 `remote-object-info` 和 promisor 懒拉取来验证。
3. **对外发布 HTTP 层一致性测试**：把 `differential.rs` 改成通过 HTTP 调用，打包成 `flux-conformance`，可以指向任意实现，覆盖 §7 的缓存规则、错误码和 `no-store`。
4. **补测**：
   - 在 `bench/run.py` 里记录 `bytes_fetched` 和请求数；
   - 新增"暖节点增量 checkout"场景；
   - 用 `tc netem` 或 macOS 的 `dnctl` 注入 20、50、150 ms 的 RTT；
   - 用 `GIT_TRACE2_PERF` 拆分 git 一侧的耗时，核实"no negotiation"的归因；
   - 统计 server 和 upload-pack 每个请求的 CPU 时间。
5. **修正文案**：README 的 "Want-only protocol"、benchmarks.md 的 "There is no have/want negotiation"、spec §3.6 和 §9，统一改成"git v2 已经能做到精确拉取；Flux 的差异在于逐对象可寻址、可缓存、缺失容错"。

---

## 附录 A：引用

| 来源 | 位置 | 内容 |
|---|---|---|
| `gitprotocol-v2.adoc` | 19-29 | v2 的目标：可扩展，为无状态 RPC 和 HTTP 代理设计 |
| 同上 | 104-142 | 能力声明和命令请求的语法 |
| 同上 | 170-176 | 默认无状态，可以轮询负载均衡 |
| 同上 | 197-239 | `ls-refs` |
| 同上 | 256-271 | `want` 可以是任意 oid，以及 `have`、`done` |
| 同上 | 331-385 | `filter`、`want-ref`、`sideband-all`、`packfile-uris`、`wait-for-done` |
| 同上 | 534-541 | `object-format` |
| 同上 | 556-600 | `object-info` |
| 同上 | 602-801 | `bundle-uri` |
| 同上 | 803-900 | `promisor-remote`（含 `token` 字段和 future work） |
| `upload-pack.c` | 1437-1468 | v2 的 `parse_want`：只检查对象是否存在，未知对象直接 `die` |
| 同上 | 1735-1789 | `upload_pack_v2`：没有 `have` 时直接生成 pack |
| `promisor-remote.c` | 47-50 | 懒拉取使用 noop 协商加 `blob:none` |
| `serve.c` | 95-106、146-189 | `object-info` 默认不宣告；v2 的能力表 |
| `technical/partial-clone.adoc` | 171-182、250-259 | 懒拉取不做协商，但会拉闭包；逐个对象调用 fetch-pack |
| `technical/packfile-uri.adoc` | 1-9、78-82 | CDN 卸载；future work 中的 "raw object" |
| `rev-list-options.adoc` | 993-1011 | 显式给出的对象不受过滤；`tree:<depth>` |
| `git-cat-file.adoc` | 172-177 | `remote-object-info` |
| `config/transfer.adoc` | 125-127 | `transfer.advertiseObjectInfo` 默认 false |
| `gitprotocol-http.adoc` | 66-92 | 标准 HTTP 鉴权 |

Flux 侧：spec §1.3、§2.1、§3.1-3.7、§7.1-7.3、§9、§13.2；`server.rs:160`（`is_pinned`）、`:217`（capabilities）、`:267-283`（整批放进内存）；`cache.rs:148`、`:259-330`；`client.rs:15-16`、`:218`；`checkout.rs:84-141`；`object.rs:208`；`crates/flux/Cargo.toml:15`。

## 附录 B：复现

```sh
S=<scratchpad>; cd $S
cp -R research/gitgit exp05/repo.git                # git/git 的 blobless bare 镜像
# want-only 拉取 HEAD 和 HEAD~50 的全部 blob（noop 协商，真实 WAN）
git -C exp05/repo.git -c fetch.negotiationAlgorithm=noop fetch origin --no-tags \
  --no-write-fetch-head --recurse-submodules=no --filter=blob:none --stdin < exp05/blobs.txt
python3 -I exp05scripts/measure.py $PWD/exp05/repo.git $PWD/exp05/commits     # HEAD~50..HEAD
python3 -I exp05scripts/measure.py $PWD/exp05/repo.git $PWD/exp05/commits15   # HEAD~15..HEAD
# thin pack 基线：用合成 commit 把旧树设为可作 delta 基底的边界
X0=$(git commit-tree -m x0 $C0^{tree}); X1=$(git commit-tree -m x1 -p $X0 $C1^{tree})
printf "%s\n^%s\n" $X1 $X0 | git pack-objects --revs --thin --stdout -q | wc -c
# 精确拉取单个 tree：tree:0 返回 1 个对象，blob:none 返回 124 个
echo <HEAD:t 的 oid> | git -c fetch.negotiationAlgorithm=noop fetch origin --filter=tree:0 --stdin ...
```
