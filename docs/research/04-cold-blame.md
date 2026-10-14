# 冷 blame 为什么慢，怎么修

对象：git/git 的 `builtin/blame.c`。它有 189 个 first-parent 版本，最终 1334 行。冷 blame 耗时 622 ms，git 是 86 ms。
来源：三份独立调研，分别是开源实现调研、增量数据结构调研、本地 profile。

## 1. 结论

- **diff 只占约 3%**。188 次 diff 合计 9–31 ms，每步约 60 µs。
- **方向没错**。git 在这个文件上也无法提前停止：只 blame `-L 1,40` 时，它照样做 268 次 diff，因为文件里有 2006 年的行。
  - 反向 scoreboard 只在"只看较新的几行"时占优，不值得为此做两套实现。
- **慢的是每一步的常数开销**。git 每步约 0.3 ms，Flux 每步约 3.3 ms。

## 2. 实测分解（完全冷，进程内 553 ms）

| 项 | 冷 | 热 | 原因 |
|---|---:|---:|---|
| `last_change` × 189 | 169 ms | 24 ms | 每步重查 `path_index`（LIMIT 512、混合排序方向），554 次 `on_first_parent_chain` 冷读图节点 |
| 树读取 × 1133 | 150 ms | 70 ms | 每步 `file_blob` 调 3 次，每次解析 22 KB 根树，其中 4/6 是重复读 |
| blob 读取 × 378 | 87 ms | 64 ms | 旧 blob 就是上一步的新 blob，重复读了一半 |
| changeset 整行 × 567 | 51 ms | 4 ms | 只为拿 tree id |
| `put_derived` × 189 | 47 ms | 26 ms | 每步一个 autocommit，共 3.28 MB |
| diff + split | 31 ms | 30 ms | |

## 3. 方案（按性价比排序）

**A. 修冷路径**（约半天，只改 `blame.rs` 和 `Repo::open`；profile 原型实测与原实现逐行一致）
1. **一次取出整条链，并复用上一步的 blob。** 用 `log(last, path, ∞)` 一次拿到完整的变更链。first-parent 链只会收缩，所以结果与原实现等价。链上每一步的旧/新 blob oid 直接取自 changeset 的 change 记录，完全不走树；上一步的新 blob 直接作为下一步的旧 blob。实测冷耗时 266 ms。
2. **只持久化最终结果加检查点。** 检查点每 32 步一个，全部写入放进同一个事务。写入量从 3.28 MB 降到约 100 KB。
3. **并行预取。** 用 rayon 并行解压 blob；并行读取图节点、changeset 行和 commit 元数据。实测冷耗时约 133 ms。
4. **预热页缓存。** 服务启动时顺序读一遍 meta.sqlite，耗时一次性 17–32 ms。实测冷耗时 46 ms。
5. **换 zlib 后端。** flate2 换用 zlib-rs，只需改一个 feature。blob 解压从 149 µs 降到 85 µs，checkout 也会受益。

预期：冷 blame 从 622 ms 降到约 50–70 ms，比 git 的 86 ms 更快。热 blame 从约 200 ms 降到约 35 ms（这里指进程内，不含 HTTP）。

**需要先核对：** 方案 A.1 依赖 change 记录与 `file_blob` 的语义完全一致。目录与文件互换、symlink、gitlink 三种边界情况要对照 `changes.rs` 的 `diff_rec` 逐一确认，并加进差分 fuzz。

**B. diff 换成 imara-diff 的 Myers 加 `postprocess_lines`**（约半天）
- 速度相当，最慢一步从 5.8 ms 降到 0.24 ms。
- 与 git 的 hunk 一致率从 174/188 提到 185/188。在未跨改名的 884 行上，blame 一致率从 98.8% 提到 100%（仅单文件实测）。
- 它的启发式是确定性的，不需要 5 s deadline。`exact=false` 这条路径和 `no-store` 的特殊处理可以删掉，纯函数约束重新干净。

**C. 改为每个文件存一个 linelog**（Mercurial / Sapling，约 2–3 天）
- key 为 `(path, 最后一次 first-parent 改动)`，value 为 linelog 加 revmap。仍是不可变输入的纯函数。
- 存储：189 个版本从 3.28 MB 降到 62 KB。任意历史版本的 annotate 只需 3 µs，不再需要检查点。
- 增量：每次改动在末尾追加指令，成本为 O(hunk 行数)。
- 风险：改动极频繁的文件指令会变长。需要大小上限；超过 10 MB 的文件和二进制文件直接拒绝，与 Mononoke 一致。

**D. ingest 时急切推导**（Mononoke 模式，约 1–2 天，复用 `Derivable`）
- ref 前进时，在后台为新提交改动的文本文件追加 linelog；查询时的惰性推导仍保留作兜底。
- 对新提交而言不再存在冷 blame。
- 前提是先做 C：每步存完整 BlameData 的话，全仓推导的存储量不可接受。

**不推荐的做法：**
- 直接引入 gix-blame：它没有 first-parent 模式，也用不上 Flux 的 path index。
- 反向 scoreboard 加缓存的混合方案（JGit BlameCache）：要维护两套实现，而且在这类文件上没有收益。

## 4. 参考

- git `blame.c`：Bloom filter 提交 0906ac2b5，2014 年性能重写 7e6ac6e439
- [JGit BlameCache](https://gerrit.googlesource.com/jgit/+/e295f7bd9eeccfd039393d58574461fc9d924e8a%5E%21/)
- [Gitiles blame 缓存](https://gerrit.googlesource.com/gitiles/+/1aa01423df03725ce5f98bc12e1e9f709334b922%5E%21)
- [gitoxide PR #1852](https://github.com/GitoxideLabs/gitoxide/pull/1852)
- [Mercurial linelog.py](https://www.mercurial-scm.org/repo/hg/file/tip/mercurial/linelog.py)，以及 `hgext/fastannotate`
- [Sapling linelog（Rust）](https://github.com/facebook/sapling/tree/main/eden/scm/lib/linelog)
- Mononoke `derived_data/blame/derive_v2.rs`、`mononoke_types/src/blame_v2.rs`
- [GitLab #586593](https://gitlab.com/gitlab-org/gitlab/-/issues/586593)：Linux `MAINTAINERS` 的 blame 约需 8 s
- [Interleaved deltas（SCCS weave）](https://en.wikipedia.org/wiki/Interleaved_deltas)
