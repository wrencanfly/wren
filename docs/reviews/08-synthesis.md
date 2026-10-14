# 综合结论：按原文意图，Flux 当前是否合理

来源：reviews 04（定位与原文意图）、05（协议与 git v2 对比）、06（agent 规模下会先撞上的墙）、07（反方审查）。
四份报告都由 AI agent 撰写，凡是没有实测的结论都标了出处。

## 一句话

方向对了一半。服务端查询 RPC 是 git 真正缺少的东西，和原文的诉求一致，这部分应该保留并加强。对象协议和 checkout 是在重做微软 GVFS 和 git partial clone 已经做过的事，而且做得更差。"Agent-scale source control" 这个产品定位，与原文"让 git 和 forge 自己演进"的建议是冲突的。

## 原文核对（04）

- **原文写了：**
  - 只适用于超大 monorepo 和成群的 agent；
  - 对绝大多数仓库，git 的设计仍然正确；
  - 这一类问题 6–12 个月内会更普遍；
  - 希望与 GitHub、GitLab 和开源社区合作；
  - 服务端 RPC，可以修改协商协议。
- **原文没写：** "不开源"；"完全重新设计服务端"（原文前提是"不偏离 git 的整体模型"）；"为人类定的规则都要推倒"。
- **常被漏掉的两条：** 让 git 的 sparse 和 shallow 好好组合；标准化 RPC，让 git 客户端能学会调用它们。

## 保留

| 项 | 理由 |
|---|---|
| 查询 API：`landed`、`bisect`、path log、blame、merge-base | git v2 没有对应物，可以做成 v2 的新命令（有 `object-info` 先例）；`landed` 在 git 里没有原语 |
| 以 git 为 oracle 的差分测试 | 唯一能直接交给 forge 和其他实现方的资产 |
| MCP | 精确问答只用几十个 token；工具定义约 1k token，GitHub 官方 MCP 是 4–5 万 |
| pinned 查询不可变 | 方向正确，但必须加语义版本（见下） |

## 修改

**发布前必须做（P0）**
1. **改名。** FluxCD（CNCF 毕业项目）的 CLI 也叫 `flux`，用户群同样是平台团队；crates.io 上的 `flux` 和 `flux-core` 已被占用。
2. **重新定位。** 改为："git 服务端查询 RPC 的参考实现 + 单一信任域内的只读历史查询服务"，并声明"与 Anthropic 无关，也未获背书"。
3. **把隐瞒的代价写进文档。**
   - 对象存储是 loose zlib，4.7 GB；同样的对象打成 git pack 只有 313 MiB，约 15 倍。删掉 README 里的 "Lean server"。
   - 增量 checkout 的传输字节数是 git thin pack 的 3–42 倍，因为没有 delta。
   - benchmarks.md 把优势归因于"省掉 have/want 协商"，这不成立：git 冷 clone 本来就不发 `have`。
   - 部署前提：单一信任域；force-push 删掉的内容仍然可以读到；只能单机。
4. **协议 v1 冻结前的修正**（05 §5）
   - 对象路由带上仓库（`/v1/repos/{repo}/objects`），鉴权按仓库区分；
   - pinned 查询由路径结构保证（`.../at/{oid}/blame`），不再靠猜字符串；
   - 每个查询带语义版本（例如 `blame@fp-myers-1`），并纳入缓存 key；
   - 机器可读的错误码；
   - 私有仓库的响应不标 `public`。
5. **让 MCP 省 token、可缓存**（06）
   - 加行范围、`max_bytes` 加截断说明、游标分页。目前一次 `flux_refs` 约 2.5 万 token，一次 blame 比文件本身还长。
   - 把 `HEAD` 先解析成完整 id 再查询。现在默认路径的查询全部穿透缓存。
6. **新鲜度**：webhook 触发同步；每个响应带同步水位；支持"至少已看到某个 commit"的参数。现在 agent 刚 push 完去问 `landed`，可能拿到"不包含"。

**之后（P1）**
- 冷 blame 修复（research/04：先修冷路径，再换 imara-diff），预期从 622 ms 降到约 60 ms；
- cache 层 singleflight（合并相同的并发请求）；
- 给 agent 用的 grep 和 `log -S`；
- delta 帧或 pack 存储；一次取回整个 cone 的 manifest 端点，消除逐层往返；
- 补 benchmark：linux.git 规模、forge API 对照组、真实 RTT、传输字节数。

## 新增方向

- **`culprit`**（06 P1）：用已有的 `landed`、`bisect` 和 sparse checkout，组成合入后测试的定位与回滚管线。这是 Flux 唯一能切进团队早期之墙的地方，现有零件已经够了一大半。
- **上游路线**（04、05）：
  - 发出 `docs/upstream/git-sparse-promisor.md`；
  - 写一份 git protocol v2 扩展提案：`fetch` 的 `missing-ok`、`query` 命令族、给原生 git 客户端宣告 promisor 缓存层；
  - 第一个适配对象选 Gitea 或 Forgejo。

## 砍掉或推迟

- v0.3 FUSE 虚拟 checkout；
- v0.4 "Flux 作为权威源 + 服务端提交"：这就是在造公开版 Scion，正是原文说"不会真的有用"的那条路；
- Postgres/S3 后端，推迟到有真实部署方再做；
- 把 spec 从"标准"降级为"不稳定的 API 参考"，等 P0 修正完再谈冻结；
- 明确不做：code review、CI 调度、完整 merge queue、代理 forge 的 PR 和 checks API。
