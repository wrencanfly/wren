# wren-nfsserve

An NFSv3 server library, vendored from [nfsserve](https://github.com/huggingface/nfsserve)
0.11.0 (BSD-3-Clause, Copyright (c) 2023 XetData, (c) 2025 Hugging Face; see
`LICENSE`). `wren mount` uses it on macOS, where the system's own NFS client
mounts a localhost server without kernel extensions or root.

Changes from upstream (marked `Wren:` in the source):

* `READDIR` passes the client's cookie to the filesystem
  (`NFSFileSystem::readdir_simple` takes `start_after`). Upstream dropped it,
  so listing a directory larger than one reply started over from the top.
* `DirEntry::attr` is optional: `READDIRPLUS` may send entries without
  attributes, and the client asks with `GETATTR` when it needs them. A file
  whose attributes are expensive (its size is in a blob not fetched yet) is
  then not fetched just because its directory was listed.
* `LINK` answers `NFS3ERR_NOTSUPP` (upstream: procedure unavailable), and
  `COMMIT` succeeds (every `WRITE` is already `FILE_SYNC`).
