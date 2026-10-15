//! git's smart HTTP protocol, served from Wren's own data (M5).
//!
//! Stock git talks to Wren directly: `git clone`, `git fetch` (including
//! partial clones and their lazy fetches) through protocol v2's `ls-refs`
//! and `fetch`, and `git push` through `receive-pack`. This is the
//! "antique client" path of Scion's topology: git needs no Wren client.
//!
//! * Refs come from the metadata store; packs are generated on the fly from
//!   the object store, without deltas (D4), never stored, never repacked.
//! * A push's pack is indexed by `git index-pack --strict` (which checks
//!   every object), becomes a segment, its commits are ingested as
//!   changesets, and each ref moves by compare-and-swap through
//!   [`Repo::update_ref_as`], so protection, policy and the audit log apply
//!   to pushes exactly as to the write API.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context, Result};

use crate::object::{self, Kind};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::write::RefUpdate;

pub const AGENT: &str = "wren/0.1";

// ---- pkt-lines ------------------------------------------------------------------

pub fn pkt(out: &mut Vec<u8>, data: &[u8]) {
    out.extend(format!("{:04x}", data.len() + 4).as_bytes());
    out.extend(data);
}

pub fn pkt_str(out: &mut Vec<u8>, s: &str) {
    pkt(out, s.as_bytes())
}

pub fn flush(out: &mut Vec<u8>) {
    out.extend(b"0000");
}

pub fn delim(out: &mut Vec<u8>) {
    out.extend(b"0001");
}

#[derive(Debug, PartialEq, Eq)]
pub enum Pkt<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    End,
}

pub struct PktReader<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> PktReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        PktReader { buf, pos: 0 }
    }

    pub fn next_pkt(&mut self) -> Result<Option<Pkt<'a>>> {
        if self.pos == self.buf.len() {
            return Ok(None);
        }
        let len = self
            .buf
            .get(self.pos..self.pos + 4)
            .context("truncated pkt-line")?;
        let len = usize::from_str_radix(std::str::from_utf8(len)?, 16)?;
        self.pos += 4;
        Ok(Some(match len {
            0 => Pkt::Flush,
            1 => Pkt::Delim,
            2 => Pkt::End,
            n if n < 4 => bail!("bad pkt-line length {n}"),
            n => {
                let d = self
                    .buf
                    .get(self.pos..self.pos + n - 4)
                    .context("truncated pkt-line")?;
                self.pos += n - 4;
                Pkt::Data(d)
            }
        }))
    }

    /// The rest of the input, after the pkt-lines read so far.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
}

fn text(d: &[u8]) -> &str {
    std::str::from_utf8(d).unwrap_or("").trim_end_matches('\n')
}

/// Data in side-band channel `band` (1 data, 2 progress, 3 error).
fn sideband(out: &mut Vec<u8>, band: u8, data: &[u8]) {
    for chunk in data.chunks(65515) {
        let mut d = Vec::with_capacity(chunk.len() + 1);
        d.push(band);
        d.extend(chunk);
        pkt(out, &d);
    }
}

// ---- upload-pack (protocol v2) -------------------------------------------------------

/// The protocol v2 capability advertisement (`GET info/refs` with
/// `Git-Protocol: version=2`).
pub fn upload_pack_advertisement() -> Vec<u8> {
    let mut out = Vec::new();
    for line in [
        "version 2\n".to_string(),
        format!("agent={AGENT}\n"),
        "ls-refs=unborn\n".into(),
        // `shallow`: a shallow repository (a `--git` checkout or mount)
        // must see it before it lazily fetches; its `shallow` lines are
        // accepted, `deepen` requests are refused.
        "fetch=shallow filter\n".into(),
        "object-format=sha1\n".into(),
    ] {
        pkt_str(&mut out, &line);
    }
    flush(&mut out);
    out
}

/// Answer one protocol v2 request (`POST git-upload-pack`).
pub fn upload_pack(repo: &Repo, body: &[u8]) -> Result<Vec<u8>> {
    let mut r = PktReader::new(body);
    let mut command = None;
    // Capability lines, up to the delimiter.
    loop {
        match r.next_pkt()? {
            Some(Pkt::Data(d)) => {
                let t = text(d);
                if let Some(c) = t.strip_prefix("command=") {
                    command = Some(c.to_string());
                } else if let Some(f) = t.strip_prefix("object-format=") {
                    ensure!(f == "sha1", "unsupported object format {f}");
                }
            }
            Some(Pkt::Delim) | Some(Pkt::Flush) | None => break,
            Some(Pkt::End) => break,
        }
    }
    let mut args = Vec::new();
    while let Some(Pkt::Data(d)) = r.next_pkt()? {
        args.push(text(d).to_string());
    }
    match command.as_deref() {
        Some("ls-refs") => ls_refs(repo, &args),
        Some("fetch") => fetch(repo, &args),
        Some(c) => bail!("unsupported command {c:?}"),
        None => bail!("no command"),
    }
}

fn ls_refs(repo: &Repo, args: &[String]) -> Result<Vec<u8>> {
    let peel = args.iter().any(|a| a == "peel");
    let symrefs = args.iter().any(|a| a == "symrefs");
    let unborn = args.iter().any(|a| a == "unborn");
    let prefixes: Vec<&str> = args
        .iter()
        .filter_map(|a| a.strip_prefix("ref-prefix "))
        .collect();
    let wanted = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let refs = repo.meta.refs()?;
    let mut out = Vec::new();
    if wanted("HEAD") {
        let head = repo.meta.get_kv("HEAD")?;
        let target = head
            .as_deref()
            .and_then(|h| refs.iter().find(|r| r.name == h));
        match (target, &head) {
            (Some(t), Some(h)) => {
                let mut line = format!("{} HEAD", t.target);
                if symrefs {
                    line.push_str(&format!(" symref-target:{h}"));
                }
                pkt_str(&mut out, &format!("{line}\n"));
            }
            (None, Some(h)) if unborn && symrefs => {
                pkt_str(&mut out, &format!("unborn HEAD symref-target:{h}\n"));
            }
            _ => {}
        }
    }
    for r in &refs {
        if !wanted(&r.name) {
            continue;
        }
        let mut line = format!("{} {}", r.target, r.name);
        if peel && r.peeled != r.target {
            line.push_str(&format!(" peeled:{}", r.peeled));
        }
        pkt_str(&mut out, &format!("{line}\n"));
    }
    flush(&mut out);
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    None,
    BlobNone,
    /// `blob:limit=<n>`: blobs of at most n bytes.
    BlobLimit(usize),
}

fn parse_filter(spec: &str) -> Result<Filter> {
    if spec == "blob:none" {
        return Ok(Filter::BlobNone);
    }
    if let Some(n) = spec.strip_prefix("blob:limit=") {
        let (num, mult) = match n.chars().last() {
            Some('k') => (&n[..n.len() - 1], 1 << 10),
            Some('m') => (&n[..n.len() - 1], 1 << 20),
            Some('g') => (&n[..n.len() - 1], 1 << 30),
            _ => (n, 1),
        };
        return Ok(Filter::BlobLimit(num.parse::<usize>()? * mult));
    }
    bail!("unsupported filter {spec:?} (supported: blob:none, blob:limit=<n>)")
}

fn fetch(repo: &Repo, args: &[String]) -> Result<Vec<u8>> {
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut done = false;
    let mut filter = Filter::None;
    let mut include_tag = false;
    let mut thin = false;
    for a in args {
        if let Some(o) = a.strip_prefix("want ") {
            wants.push(o.parse::<Oid>()?);
        } else if let Some(o) = a.strip_prefix("have ") {
            haves.push(o.parse::<Oid>()?);
        } else if a == "done" {
            done = true;
        } else if let Some(f) = a.strip_prefix("filter ") {
            filter = parse_filter(f)?;
        } else if a == "include-tag" {
            include_tag = true;
        } else if a == "thin-pack" {
            thin = true;
        } else if a.starts_with("deepen") {
            bail!("shallow fetches are not supported; use a partial clone (--filter=blob:none)");
        }
        // `shallow <oid>`: the client's own shallow boundary (sent by a
        // shallow repository's lazy fetches). It changes nothing for what
        // is sent without `deepen`.
        // ofs-delta, no-progress, wait-for-done: nothing to do.
    }
    let mut out = Vec::new();
    // Haves Wren knows are common; the pack is built against them.
    let mut common = Vec::new();
    for h in &haves {
        if repo.meta.has_changeset(h)? {
            common.push(*h);
        }
    }
    if !done {
        pkt_str(&mut out, "acknowledgments\n");
        if common.is_empty() {
            pkt_str(&mut out, "NAK\n");
        }
        for c in &common {
            pkt_str(&mut out, &format!("ACK {c}\n"));
        }
        // Without anything in common, let the client offer more haves,
        // unless it offered none (it has nothing).
        if common.is_empty() && !haves.is_empty() {
            flush(&mut out);
            return Ok(out);
        }
        pkt_str(&mut out, "ready\n");
        delim(&mut out);
    }
    let (objects, known) = objects_to_send(repo, &wants, &common, filter, include_tag)?;
    let pack = pack_bytes(repo, &objects, &known, thin)?;
    pkt_str(&mut out, "packfile\n");
    sideband(&mut out, 1, &pack);
    flush(&mut out);
    Ok(out)
}

/// Commits reachable from `wants` but not from `haves`, and the "edge":
/// commits reachable from `haves` that are parents of commits sent.
///
/// Paint-down by generation number: commits come off a max-heap, so every
/// descendant of a commit is processed before it and its mark (reachable
/// from a have, or not) is final when it is popped. The walk stops as soon
/// as nothing in the heap is still interesting.
fn commit_walk(repo: &Repo, wants: &[Oid], haves: &[Oid]) -> Result<(Vec<Oid>, Vec<Oid>)> {
    struct Walk {
        heap: BinaryHeap<(u32, Oid)>,
        /// true: reachable from a have.
        known: HashMap<Oid, bool>,
        interesting: usize,
    }
    impl Walk {
        fn push(&mut self, repo: &Repo, c: Oid, from_have: bool) -> Result<()> {
            match self.known.get_mut(&c) {
                Some(k) => {
                    if from_have && !*k {
                        // Still queued (see above): no longer interesting.
                        *k = true;
                        self.interesting -= 1;
                    }
                }
                None => {
                    self.known.insert(c, from_have);
                    self.heap.push((repo.node(&c)?.generation, c));
                    if !from_have {
                        self.interesting += 1;
                    }
                }
            }
            Ok(())
        }
    }
    let mut w = Walk {
        heap: BinaryHeap::new(),
        known: HashMap::new(),
        interesting: 0,
    };
    for h in haves {
        w.push(repo, *h, true)?;
    }
    for c in wants {
        if repo.meta.has_changeset(c)? {
            w.push(repo, *c, false)?;
        }
    }
    let mut send = Vec::new();
    while w.interesting > 0 {
        let Some((_, c)) = w.heap.pop() else { break };
        let from_have = w.known[&c];
        if !from_have {
            w.interesting -= 1;
            send.push(c);
        }
        for p in repo.node(&c)?.parents.clone() {
            w.push(repo, p, from_have)?;
        }
    }
    let mut edge = HashSet::new();
    for c in &send {
        for p in &repo.node(c)?.parents {
            if w.known.get(p) == Some(&true) {
                edge.insert(*p);
            }
        }
    }
    Ok((send, edge.into_iter().collect()))
}

/// Every object a fetch of `wants` (against `haves`) must send, and the
/// objects the client is known to have (under the edge commits' trees).
fn objects_to_send(
    repo: &Repo,
    wants: &[Oid],
    haves: &[Oid],
    filter: Filter,
    include_tag: bool,
) -> Result<(Vec<Oid>, HashSet<Oid>)> {
    let store = &repo.store;
    let mut out = Vec::new();
    let mut sent: HashSet<Oid> = HashSet::new();
    // Wants that are not commits: tags, and the trees and blobs a partial
    // clone fetches lazily (sent whatever the filter).
    let mut commit_wants = Vec::new();
    for w in wants {
        if repo.meta.has_changeset(w)? {
            commit_wants.push(*w);
            continue;
        }
        let (kind, data) = store
            .get(w)?
            .with_context(|| format!("want {w}: no such object"))?;
        match kind {
            Kind::Tag => {
                if sent.insert(*w) {
                    out.push(*w);
                }
                let mut target = object::Tag::parse(&data)?;
                while target.kind == Kind::Tag {
                    if sent.insert(target.object) {
                        out.push(target.object);
                    }
                    target = object::Tag::parse(&store.get_kind(&target.object, Kind::Tag)?)?;
                }
                if target.kind == Kind::Commit {
                    commit_wants.push(target.object);
                }
            }
            Kind::Tree => walk_tree(
                store,
                *w,
                Filter::None,
                &HashSet::new(),
                &mut sent,
                &mut out,
            )?,
            _ => {
                if sent.insert(*w) {
                    out.push(*w);
                }
            }
        }
    }
    let (commits, edge) = commit_walk(repo, &commit_wants, haves)?;
    // What the client has: everything under the edge commits' trees.
    let mut known: HashSet<Oid> = HashSet::new();
    for e in &edge {
        let tree = repo.tree_of(e)?;
        let mut stack = vec![tree];
        while let Some(t) = stack.pop() {
            if !known.insert(t) {
                continue;
            }
            for entry in store.read_tree(&t)? {
                if entry.is_tree() {
                    stack.push(entry.oid);
                } else {
                    known.insert(entry.oid);
                }
            }
        }
    }
    for c in &commits {
        if sent.insert(*c) {
            out.push(*c);
        }
        let tree = repo.tree_of(c)?;
        walk_tree(store, tree, filter, &known, &mut sent, &mut out)?;
    }
    if include_tag {
        let sent_commits: HashSet<&Oid> = commits.iter().collect();
        for r in repo.meta.refs()? {
            if r.name.starts_with("refs/tags/")
                && r.target != r.peeled
                && sent_commits.contains(&r.peeled)
                && sent.insert(r.target)
            {
                out.push(r.target);
            }
        }
    }
    Ok((out, known))
}

fn walk_tree(
    store: &crate::store::ObjectStore,
    root: Oid,
    filter: Filter,
    known: &HashSet<Oid>,
    sent: &mut HashSet<Oid>,
    out: &mut Vec<Oid>,
) -> Result<()> {
    let mut stack = vec![root];
    while let Some(t) = stack.pop() {
        if known.contains(&t) || !sent.insert(t) {
            continue;
        }
        out.push(t);
        for e in store.read_tree(&t)? {
            if e.is_tree() {
                stack.push(e.oid);
            } else if e.is_gitlink() || known.contains(&e.oid) || sent.contains(&e.oid) {
                continue;
            } else {
                let include = match filter {
                    Filter::None => true,
                    Filter::BlobNone => false,
                    Filter::BlobLimit(n) => store.get(&e.oid)?.is_some_and(|(_, d)| d.len() <= n),
                };
                if include {
                    sent.insert(e.oid);
                    out.push(e.oid);
                }
            }
        }
    }
    Ok(())
}

/// A pack of `oids`, in memory. An object stored as a delta (the deltas
/// git computed, kept in Wren's segments) is sent as that delta when its
/// base is sent too or the client has it (`known`, with `thin-pack`);
/// everything else is sent whole.
fn pack_bytes(repo: &Repo, oids: &[Oid], known: &HashSet<Oid>, thin: bool) -> Result<Vec<u8>> {
    let sending: HashSet<&Oid> = oids.iter().collect();
    let mut w = crate::pack::PackWriter::new(u32::try_from(oids.len())?);
    for o in oids {
        if let Some((base, size, z)) = repo.store.packs().stored_delta(o)? {
            if sending.contains(&base) || (thin && known.contains(&base)) {
                w.ref_delta(&base, size, &z);
                continue;
            }
        }
        let (k, d) = repo
            .store
            .get(o)?
            .with_context(|| format!("object {o} is missing from the store"))?;
        w.whole(k, &d)?;
    }
    w.finish()
}

// ---- receive-pack (push) --------------------------------------------------------------

/// The ref advertisement for `git push` (`GET info/refs?service=git-receive-pack`).
pub fn receive_pack_advertisement(repo: &Repo) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    pkt_str(&mut out, "# service=git-receive-pack\n");
    flush(&mut out);
    let caps = format!(
        "report-status delete-refs side-band-64k quiet ofs-delta no-thin \
         object-format=sha1 agent={AGENT}"
    );
    let refs = repo.meta.refs()?;
    if refs.is_empty() {
        pkt(
            &mut out,
            format!("{} capabilities^{{}}\0{caps}\n", Oid::ZERO).as_bytes(),
        );
    }
    for (i, r) in refs.iter().enumerate() {
        if i == 0 {
            pkt(
                &mut out,
                format!("{} {}\0{caps}\n", r.target, r.name).as_bytes(),
            );
        } else {
            pkt_str(&mut out, &format!("{} {}\n", r.target, r.name));
        }
    }
    flush(&mut out);
    Ok(out)
}

/// Apply a push (`POST git-receive-pack`) and return the status report.
pub fn receive_pack(repo: &Repo, body: &[u8]) -> Result<Vec<u8>> {
    let mut r = PktReader::new(body);
    let mut commands: Vec<(Option<Oid>, Option<Oid>, String)> = Vec::new();
    let mut caps: Vec<String> = Vec::new();
    while let Some(Pkt::Data(d)) = r.next_pkt()? {
        let (line, c) = match d.iter().position(|&b| b == 0) {
            Some(i) => (&d[..i], Some(&d[i + 1..])),
            None => (d, None),
        };
        if let Some(c) = c {
            caps = text(c).split(' ').map(str::to_string).collect();
        }
        let t = text(line);
        let mut it = t.splitn(3, ' ');
        let (Some(old), Some(new), Some(name)) = (it.next(), it.next(), it.next()) else {
            bail!("bad push command {t:?}");
        };
        let opt = |s: &str| -> Result<Option<Oid>> {
            let o: Oid = s.parse()?;
            Ok((o != Oid::ZERO).then_some(o))
        };
        commands.push((opt(old)?, opt(new)?, name.to_string()));
    }
    let pack = r.rest();
    let unpack = if pack.is_empty() {
        Ok(())
    } else {
        add_pushed_pack(repo, pack)
    };
    let mut report = Vec::new();
    match &unpack {
        Ok(()) => pkt_str(&mut report, "unpack ok\n"),
        Err(e) => pkt_str(
            &mut report,
            &format!("unpack {}\n", one_line(&format!("{e:#}"))),
        ),
    }
    for (old, new, name) in &commands {
        let status = match &unpack {
            Err(_) => Err("unpacker error".to_string()),
            Ok(()) => apply_command(repo, name, *old, *new),
        };
        match status {
            Ok(()) => pkt_str(&mut report, &format!("ok {name}\n")),
            Err(reason) => pkt_str(&mut report, &format!("ng {name} {}\n", one_line(&reason))),
        }
    }
    flush(&mut report);
    if caps.iter().any(|c| c == "side-band-64k") {
        let mut out = Vec::new();
        sideband(&mut out, 1, &report);
        flush(&mut out);
        Ok(out)
    } else {
        Ok(report)
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// Index a pushed pack and add it to the store as a segment. `git
/// index-pack --strict` checks every object; `--fix-thin` completes a thin
/// pack (git sends deltas against objects the server has) with the bases
/// from Wren's store, through a scratch repository whose alternates point
/// at it.
fn add_pushed_pack(repo: &Repo, pack: &[u8]) -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("incoming-")
        .tempdir_in(repo.store.root())?;
    let scratch = dir.path().join("repo.git");
    let init = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&scratch)
        .output()?;
    ensure!(init.status.success(), "git init failed");
    std::fs::write(
        scratch.join("objects/info/alternates"),
        format!("{}\n", repo.store.root().canonicalize()?.display()),
    )?;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(&scratch)
        .args(["index-pack", "--stdin", "--fix-thin", "--strict"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git index-pack")?;
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(pack)?;
    }
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "invalid pack: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    // "pack\t<hash>" (or "keep\t<hash>").
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hash = stdout.trim().rsplit('\t').next().unwrap_or("").to_string();
    ensure!(hash.len() == 40, "unexpected index-pack output {stdout:?}");
    let named = scratch.join(format!("objects/pack/pack-{hash}.pack"));
    let _ = std::fs::remove_file(named.with_extension("keep"));
    repo.store.add_pack(&named)?;
    Ok(())
}

fn apply_command(
    repo: &Repo,
    name: &str,
    old: Option<Oid>,
    new: Option<Oid>,
) -> Result<(), String> {
    let run = || -> Result<Result<(), String>> {
        if let Some(n) = new {
            repo.ingest_pushed(&n)?;
        }
        Ok(match repo.update_ref_as(name, old, new, "push")? {
            RefUpdate::Updated => Ok(()),
            RefUpdate::Stale { .. } => Err("fetch first".into()),
            RefUpdate::Rejected { reason } => Err(reason),
        })
    };
    match run() {
        Ok(r) => r,
        Err(e) => Err(format!("{e:#}")),
    }
}
