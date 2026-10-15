//! Upstream access. Wren sits in front of an existing git host: it keeps a
//! bare mirror fetched with stock git, and reads objects out of it with
//! `git cat-file --batch`. Nothing else in Wren talks git protocols.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::object::Kind;
use crate::oid::Oid;

pub struct GitMirror {
    path: PathBuf,
}

/// Outcome of a conditional push to the upstream.
#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    Done,
    /// The upstream ref no longer has the expected value.
    Stale,
    /// The upstream refused the update (hook, branch protection, ...).
    Rejected(String),
}

pub struct UpstreamRef {
    pub name: String,
    pub oid: Oid,
    pub kind: Kind,
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("--git-dir").arg(dir);
    c.env("GIT_TERMINAL_PROMPT", "0");
    c
}

fn run(mut cmd: Command) -> Result<Vec<u8>> {
    let out = cmd
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

impl GitMirror {
    /// Open the mirror at `path`, creating it (without fetching) if needed.
    /// `store` is Wren's object store. The mirror borrows it through git
    /// alternates, so after each fetch the mirror's own objects can be moved
    /// into the store and deleted: the mirror keeps refs, not a second copy
    /// of the repository, and never accumulates packfiles.
    pub fn open(path: impl Into<PathBuf>, url: &str, store: &Path) -> Result<GitMirror> {
        let path = path.into();
        if !path.join("HEAD").exists() {
            std::fs::create_dir_all(&path)?;
            run({
                let mut c = Command::new("git");
                c.args(["init", "--bare", "-q"]).arg(&path);
                c
            })?;
            run({
                let mut c = git(&path);
                c.args(["remote", "add", "origin", url]);
                c
            })?;
            // Only branches and tags: forge-specific refs (refs/pull/*, …) are
            // not part of the history Wren serves.
            run({
                let mut c = git(&path);
                c.args([
                    "config",
                    "remote.origin.fetch",
                    "+refs/heads/*:refs/heads/*",
                ]);
                c
            })?;
            run({
                let mut c = git(&path);
                c.args([
                    "config",
                    "--add",
                    "remote.origin.fetch",
                    "+refs/tags/*:refs/tags/*",
                ]);
                c
            })?;
            // Keep fetched objects in a pack (never loose) so they can be
            // absorbed in one pass; never repack or gc a mirror we empty.
            for (k, v) in [
                ("fetch.unpackLimit", "1"),
                ("gc.auto", "0"),
                ("maintenance.auto", "false"),
            ] {
                run({
                    let mut c = git(&path);
                    c.args(["config", k, v]);
                    c
                })?;
            }
        }
        let alternates = path.join("objects/info/alternates");
        std::fs::create_dir_all(alternates.parent().unwrap())?;
        std::fs::write(
            &alternates,
            format!("{}\n", store.canonicalize()?.display()),
        )?;
        Ok(GitMirror { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn fetch(&self) -> Result<()> {
        run({
            let mut c = git(&self.path);
            c.args(["fetch", "--prune", "--quiet", "origin"]);
            c
        })?;
        Ok(())
    }

    /// Push `new` (or a deletion) to `name` on the upstream, but only if the
    /// upstream still has `expected` there (`--force-with-lease`; `None`
    /// means "must not exist"). receive-pack checks the old value inside its
    /// ref transaction, so this is a compare-and-swap on the host. The
    /// objects come from Wren's store through the mirror's alternates.
    pub fn push_lease(&self, name: &str, expected: Option<Oid>, new: Option<Oid>) -> Result<Push> {
        let lease = format!(
            "--force-with-lease={name}:{}",
            expected.map(|o| o.to_hex()).unwrap_or_default()
        );
        let refspec = match new {
            Some(n) => format!("{n}:{name}"),
            None => format!(":{name}"),
        };
        let out = git(&self.path)
            .args(["push", "--porcelain", "--quiet", &lease, "origin", &refspec])
            .stderr(Stdio::piped())
            .output()
            .context("running git push")?;
        if out.status.success() {
            return Ok(Push::Done);
        }
        // Porcelain lines: "<flag>\t<from>:<to>\t<summary> (<reason>)".
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        match stdout.lines().find(|l| l.starts_with('!')) {
            // "stale info": the client-side lease check failed. "incorrect
            // old value provided" / "failed to lock": the host's own ref
            // transaction lost a race. All mean "the ref moved".
            Some(l)
                if [
                    "stale info",
                    "fetch first",
                    "incorrect old value",
                    "failed to lock",
                ]
                .iter()
                .any(|m| l.contains(m)) =>
            {
                Ok(Push::Stale)
            }
            Some(l) => Ok(Push::Rejected(
                l.rsplit('\t').next().unwrap_or(l).trim().to_string(),
            )),
            None => bail!("git push to upstream failed: {}", stderr.trim()),
        }
    }

    /// The upstream's default branch (e.g. `refs/heads/main`), if it has one.
    pub fn default_branch(&self) -> Result<Option<String>> {
        let out = run({
            let mut c = git(&self.path);
            c.args(["ls-remote", "--symref", "origin", "HEAD"]);
            c
        })?;
        for line in String::from_utf8_lossy(&out).lines() {
            if let Some(rest) = line.strip_prefix("ref: ") {
                if let Some((target, _)) = rest.split_once('\t') {
                    return Ok(Some(target.to_string()));
                }
            }
        }
        Ok(None)
    }

    /// Point the mirror's refs at `refs` (name, target), so the next fetch
    /// tells the host what is already here. Used when servers do not share
    /// the mirror: another server may have synced or landed since.
    pub fn set_refs(&self, refs: &[(String, Oid)]) -> Result<()> {
        let mut child = git(&self.path)
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        {
            let mut stdin = BufWriter::new(child.stdin.take().unwrap());
            for (name, oid) in refs {
                writeln!(stdin, "update {name} {oid}")?;
            }
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "git update-ref failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    pub fn list_refs(&self) -> Result<Vec<UpstreamRef>> {
        let out = run({
            let mut c = git(&self.path);
            c.args([
                "for-each-ref",
                "--format=%(objectname) %(objecttype) %(refname)",
                "refs/heads",
                "refs/tags",
            ]);
            c
        })?;
        let mut refs = Vec::new();
        for line in String::from_utf8_lossy(&out).lines() {
            let mut it = line.splitn(3, ' ');
            let (Some(oid), Some(kind), Some(name)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            refs.push(UpstreamRef {
                name: name.to_string(),
                oid: oid.parse()?,
                kind: Kind::parse(kind.as_bytes())?,
            });
        }
        Ok(refs)
    }

    /// Commits reachable from `tips` but not from `exclude`, parents first.
    pub fn rev_list(&self, tips: &[Oid], exclude: &[Oid]) -> Result<Vec<Oid>> {
        if tips.is_empty() {
            return Ok(Vec::new());
        }
        let mut child = git(&self.path)
            .args(["rev-list", "--topo-order", "--reverse", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        {
            let mut stdin = BufWriter::new(child.stdin.take().unwrap());
            for t in tips {
                writeln!(stdin, "{t}")?;
            }
            for e in exclude {
                writeln!(stdin, "^{e}")?;
            }
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "git rev-list failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.parse())
            .collect()
    }

    pub fn cat_file(&self) -> Result<CatFile> {
        let mut child = git(&self.path)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
        Ok(CatFile {
            child,
            stdin,
            stdout,
        })
    }

    /// Move every object the mirror holds itself into Wren's store, so the
    /// mirror keeps refs only. Fetched packs move in whole as segments (with
    /// git's deltas, nothing re-encoded); loose objects go to `put`. Objects
    /// reachable through alternates are untouched. Returns the number of
    /// objects moved. An interrupted absorb is simply redone: a pack leaves
    /// the mirror only once it is a complete segment.
    pub fn absorb_objects(
        &self,
        store: &crate::store::ObjectStore,
        mut put: impl FnMut(Oid, &[u8]) -> Result<()>,
    ) -> Result<usize> {
        let objects = self.path.join("objects");
        let mut count = 0;
        for fan in std::fs::read_dir(&objects)?.flatten() {
            let name = fan.file_name().to_string_lossy().to_string();
            if name.len() != 2 || !fan.path().is_dir() {
                continue;
            }
            for f in std::fs::read_dir(fan.path())?.flatten() {
                let rest = f.file_name().to_string_lossy().to_string();
                let Ok(oid) = Oid::from_hex(&format!("{name}{rest}")) else {
                    continue;
                };
                put(oid, &std::fs::read(f.path())?)?;
                std::fs::remove_file(f.path())?;
                count += 1;
            }
        }
        let pack_dir = objects.join("pack");
        let packs: Vec<PathBuf> = match std::fs::read_dir(&pack_dir) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "idx"))
                .collect(),
            Err(_) => Vec::new(),
        };
        for idx in packs {
            let pack = idx.with_extension("pack");
            count += crate::pack::Pack::open(&pack)?.count();
            store.add_pack(&pack)?;
            for ext in ["promisor", "keep", "bitmap"] {
                let _ = std::fs::remove_file(idx.with_extension(ext));
            }
        }
        Ok(count)
    }
}

pub struct CatFile {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    pub fn get(&mut self, oid: &Oid) -> Result<(Kind, Vec<u8>)> {
        writeln!(self.stdin, "{oid}")?;
        self.stdin.flush()?;
        let (_, kind, data) =
            read_batch_entry(&mut self.stdout)?.ok_or_else(|| anyhow!("cat-file exited"))?;
        Ok((
            kind,
            data.with_context(|| format!("object {oid} missing from mirror"))?,
        ))
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(clippy::type_complexity)]
fn read_batch_entry(r: &mut impl BufRead) -> Result<Option<(Oid, Kind, Option<Vec<u8>>)>> {
    let mut header = String::new();
    if r.read_line(&mut header)? == 0 {
        return Ok(None);
    }
    let parts: Vec<&str> = header.trim_end().split(' ').collect();
    if parts.len() == 2 && parts[1] == "missing" {
        return Ok(Some((parts[0].parse()?, Kind::Blob, None)));
    }
    if parts.len() != 3 {
        bail!("unexpected cat-file header {header:?}");
    }
    let oid: Oid = parts[0].parse()?;
    let kind = Kind::parse(parts[1].as_bytes())?;
    let size: usize = parts[2].parse()?;
    let mut data = vec![0u8; size + 1];
    r.read_exact(&mut data)?;
    data.pop(); // trailing newline
    Ok(Some((oid, kind, Some(data))))
}
