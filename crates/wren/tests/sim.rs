//! Simulation of the tiers with fault injection (turmoil).
//!
//! The real server, region cache and node daemons (their axum routers, the
//! cache logic, the stores) run in one process on turmoil's simulated
//! network, with simulated clients:
//!
//! * readers walk random commits' trees through a node, verifying every
//!   object, and compare pinned query answers served by the tiers with the
//!   server's own;
//! * writers land commits on `main` through the server (the control plane),
//!   retrying on network errors;
//! * a fault schedule, drawn from the seed, partitions links, adds latency
//!   and crashes and restarts the region cache.
//!
//! The network and the fault schedule are a function of the seed. Storage
//! work runs on real threads (the stores are real directories), so thread
//! interleavings are not bit-for-bit reproducible; the invariants must hold
//! for every interleaving:
//!
//! 1. every object a client receives verifies against its id;
//! 2. an answer served by a tier equals the server's, and carries the same
//!    semantics version;
//! 3. `main` behaves as a linearizable compare-and-swap register: every
//!    landed commit is on its final first-parent chain exactly once, and
//!    each client sees `main` only move forward along that chain;
//! 4. the server never has more requests in flight from the region cache
//!    than the region's upstream limit.
//!
//! `WREN_SIM_SEEDS=N` runs N seeds (default 24), starting after
//! `WREN_SIM_SEED_BASE` (default 0); `WREN_SIM_SEED=S` runs one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::http::{header, Method, Request, StatusCode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use rand::{Rng, SeedableRng};
use wren_core::api::QUERY_SEMANTICS;
use wren_core::object::{self, Kind};
use wren_core::wire::{FrameDecoder, ObjectsRequest, Payload};
use wren_core::{ObjectStore, Oid, Repo};
use wren_vcs::cache::{CacheConfig, CacheState, Tier};
use wren_vcs::server::{self, ServerState};
use wren_vcs::transport::{self as tp, BoxFuture, HttpResponse, Transport};

// ---- simulated network glue ---------------------------------------------------

struct SimListener(turmoil::net::TcpListener);

impl axum::serve::Listener for SimListener {
    type Io = turmoil::net::TcpStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok(x) => return x,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

async fn listen(port: u16) -> SimListener {
    SimListener(
        turmoil::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .unwrap(),
    )
}

/// HTTP/1 over turmoil's TCP, one connection per request.
struct SimTransport;

impl Transport for SimTransport {
    fn send(&self, req: Request<Bytes>) -> BoxFuture<anyhow::Result<HttpResponse>> {
        Box::pin(async move {
            tokio::time::timeout(Duration::from_secs(20), send(req))
                .await
                .map_err(|_| anyhow::anyhow!("timed out"))?
        })
    }
}

async fn send(req: Request<Bytes>) -> anyhow::Result<HttpResponse> {
    let host = req.uri().host().unwrap_or_default().to_string();
    let port = req.uri().port_u16().unwrap_or(80);
    let stream = turmoil::net::TcpStream::connect((host.as_str(), port)).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let (mut parts, body) = req.into_parts();
    parts.uri = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .parse()?;
    parts.headers.insert(header::HOST, host.parse()?);
    let resp = sender
        .send_request(Request::from_parts(parts, Full::new(body)))
        .await?;
    let (p, b) = resp.into_parts();
    let body = b.collect().await?.to_bytes();
    Ok(HttpResponse {
        status: p.status,
        headers: p.headers,
        body,
    })
}

/// A request from a simulated client, retried (with backoff) on network
/// errors and 5xx answers until it succeeds or `tries` run out.
async fn call(
    method: Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Bytes,
    tries: usize,
) -> anyhow::Result<HttpResponse> {
    let mut last = None;
    for i in 0..tries {
        let mut req = tp::request(method.clone(), url, None, None, body.clone())?;
        for (k, v) in headers {
            req.headers_mut()
                .insert(header::HeaderName::from_bytes(k.as_bytes())?, v.parse()?);
        }
        match SimTransport.send(req).await {
            Ok(r) if !r.status.is_server_error() => return Ok(r),
            Ok(r) => {
                last = Some(anyhow::anyhow!(
                    "{}: {}",
                    r.status,
                    String::from_utf8_lossy(&r.body)
                ))
            }
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(50 * (1 + i as u64).min(20))).await;
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no tries")))
}

// ---- the repository every seed starts from --------------------------------------

struct Template {
    _dir: tempfile::TempDir,
    root: PathBuf,
    commits: Vec<Oid>,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@wren.dev"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let up = root.join("work");
        std::fs::create_dir_all(&up).unwrap();
        git(&up, &["init", "-q", "-b", "main"]);
        for i in 0..12 {
            for d in ["a", "b/c", "d"] {
                std::fs::create_dir_all(up.join(d)).unwrap();
                for f in 0..4 {
                    if (i + f) % 3 == 0 || i == 0 {
                        std::fs::write(
                            up.join(format!("{d}/f{f}.txt")),
                            format!("{d} {f} at {i}\n").repeat(40 + f * 10),
                        )
                        .unwrap();
                    }
                }
            }
            git(&up, &["add", "-A"]);
            git(&up, &["commit", "-q", "-m", &format!("c{i}")]);
        }
        let host = root.join("host.git");
        git(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                up.to_str().unwrap(),
                host.to_str().unwrap(),
            ],
        );
        let data = root.join("data");
        let store = Arc::new(ObjectStore::open(data.join("objects")).unwrap());
        let repo = Repo::open(
            data.join("repos/demo"),
            "demo",
            store,
            Some(host.to_str().unwrap()),
        )
        .unwrap();
        repo.sync().unwrap();
        wren_core::snapshot::publish(&repo, "HEAD", &[], &data.join("snapshots")).unwrap();
        let commits = git(&up, &["rev-list", "main"])
            .lines()
            .map(|l| l.parse().unwrap())
            .collect();
        Template {
            _dir: dir,
            root,
            commits,
        }
    })
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type().unwrap().is_dir() {
            copy_dir(&src, &dst);
        } else if e.file_type().unwrap().is_file() {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

// ---- one run --------------------------------------------------------------------

#[derive(Default)]
struct Shared {
    violations: Mutex<Vec<String>>,
    /// Requests to the server from the region cache: in flight now, most
    /// at once, total.
    from_region: AtomicUsize,
    from_region_max: AtomicUsize,
    from_region_total: AtomicU64,
    from_region_paths: Mutex<HashMap<String, usize>>,
    /// Requests the region cache received (from nodes).
    to_region_total: AtomicU64,
    /// Commits writers were told were landed.
    landed: Mutex<Vec<Oid>>,
    /// Values of main each reader observed, in order.
    observed: Mutex<HashMap<usize, Vec<Oid>>>,
    objects_verified: AtomicU64,
    clients_done: AtomicUsize,
}

impl Shared {
    fn violation(&self, v: String) {
        self.violations.lock().unwrap().push(v);
    }
}

#[derive(Clone, Copy)]
struct Opts {
    nodes: usize,
    readers_per_node: usize,
    writers: usize,
    rounds: usize,
    faults: bool,
    snapshot: bool,
    /// Readers read the tip only (as a fleet starting on main does).
    head_only: bool,
}

const REGION_LIMIT: usize = 4;
const NODE_LIMIT: usize = 2;
const DIRECT: (&str, &str) = ("x-sim-direct", "1");

struct Outcome {
    shared: Arc<Shared>,
    final_main: Oid,
    main_chain: Vec<Oid>,
    initial_main: Oid,
}

fn run(seed: u64, opts: Opts) -> Outcome {
    let tpl = template();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    copy_dir(&tpl.root.join("data"), &root.join("data"));
    // Lands push to the host, so each run gets its own.
    let host = root.join("host.git");
    copy_dir(&tpl.root.join("host.git"), &host);
    // The mirror's alternates point at the template's store; repoint them.
    std::fs::write(
        root.join("data/repos/demo/mirror.git/objects/info/alternates"),
        format!("{}\n", root.join("data/objects").display()),
    )
    .unwrap();
    std::fs::write(
        root.join("data/repos/demo/mirror.git/config"),
        std::fs::read_to_string(root.join("data/repos/demo/mirror.git/config"))
            .unwrap()
            .replace(
                tpl.root.join("host.git").to_str().unwrap(),
                host.to_str().unwrap(),
            ),
    )
    .unwrap();
    let initial_main: Oid = git(&host, &["rev-parse", "main"]).trim().parse().unwrap();

    let shared = Arc::new(Shared::default());
    let mut sim = turmoil::Builder::new()
        .rng_seed(seed)
        .tick_duration(Duration::from_millis(5))
        .simulation_duration(Duration::from_secs(3600))
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(Duration::from_millis(if opts.faults { 40 } else { 5 }))
        .enable_random_order()
        .build();

    // Server.
    {
        let data = root.join("data");
        let shared = shared.clone();
        sim.host("server", move || {
            let (data, shared) = (data.clone(), shared.clone());
            async move {
                let store = Arc::new(ObjectStore::open(data.join("objects"))?);
                let repo = Repo::open(data.join("repos/demo"), "demo", store.clone(), None)?;
                let mut repos = std::collections::BTreeMap::new();
                repos.insert("demo".to_string(), Arc::new(repo));
                let mut st = ServerState::new(store, repos, None, Some("w".into()));
                st.snapshots.dir = Some(data.join("snapshots"));
                let app = server::router(Arc::new(st)).layer(axum::middleware::from_fn(
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        let shared = shared.clone();
                        async move {
                            let from_region = !req.headers().contains_key(DIRECT.0);
                            if from_region {
                                let n = shared.from_region.fetch_add(1, Ordering::SeqCst) + 1;
                                shared.from_region_max.fetch_max(n, Ordering::SeqCst);
                                shared.from_region_total.fetch_add(1, Ordering::SeqCst);
                                let p = req.uri().path().to_string();
                                *shared
                                    .from_region_paths
                                    .lock()
                                    .unwrap()
                                    .entry(p)
                                    .or_default() += 1;
                            }
                            let resp = next.run(req).await;
                            if from_region {
                                shared.from_region.fetch_sub(1, Ordering::SeqCst);
                            }
                            resp
                        }
                    },
                ));
                axum::serve(listen(7070).await, app).await?;
                Ok(())
            }
        });
    }
    // Region cache (restarted with its directory intact after a crash).
    {
        let dir = root.join("region");
        let shared = shared.clone();
        sim.host("region", move || {
            let (dir, shared) = (dir.clone(), shared.clone());
            async move {
                let st = Arc::new(CacheState::new(
                    CacheConfig {
                        tier: Tier::Region,
                        store_dir: dir.join("objects"),
                        dir: dir.clone(),
                        upstream: "http://server:7070".into(),
                        token: None,
                        upstream_token: None,
                        upstream_concurrency: REGION_LIMIT,
                        memory: 64 << 10,
                        budget: None,
                    },
                    Arc::new(SimTransport),
                )?);
                st.check_upstream(false, Duration::from_secs(600)).await?;
                let app = wren_vcs::cache::router(st).layer(axum::middleware::from_fn(
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        shared.to_region_total.fetch_add(1, Ordering::SeqCst);
                        next.run(req)
                    },
                ));
                axum::serve(listen(7071).await, app).await?;
                Ok(())
            }
        });
    }
    // Nodes, with small budgets so that eviction happens.
    for n in 0..opts.nodes {
        let dir = root.join(format!("node{n}"));
        let snapshot = opts.snapshot;
        sim.host(format!("node{n}"), move || {
            let dir = dir.clone();
            async move {
                let st = Arc::new(CacheState::new(
                    CacheConfig {
                        tier: Tier::Node,
                        store_dir: dir.join("objects"),
                        dir: dir.join("daemon"),
                        upstream: "http://region:7071".into(),
                        token: None,
                        upstream_token: None,
                        upstream_concurrency: NODE_LIMIT,
                        memory: 16 << 10,
                        budget: Some((48 << 10, Duration::from_secs(3600))),
                    },
                    Arc::new(SimTransport),
                )?);
                st.check_upstream(false, Duration::from_secs(600)).await?;
                if snapshot {
                    // Retried: the region may be down or partitioned.
                    let mut ok = false;
                    for _ in 0..200 {
                        if st.seed_from_snapshot("demo").await.is_ok() {
                            ok = true;
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    if !ok {
                        return Err("could not seed from the snapshot".into());
                    }
                }
                axum::serve(listen(7072).await, wren_vcs::cache::router(st)).await?;
                Ok(())
            }
        });
    }
    // Readers.
    for n in 0..opts.nodes {
        for r in 0..opts.readers_per_node {
            let id = n * opts.readers_per_node + r;
            let shared = shared.clone();
            let commits = tpl.commits.clone();
            let rounds = opts.rounds;
            let head_only = opts.head_only;
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed ^ ((id as u64 + 1) * 0x9e37));
            sim.client(format!("reader{id}"), async move {
                for _ in 0..rounds {
                    // rev-list order: the tip first.
                    let c = if head_only {
                        commits[0]
                    } else {
                        commits[rng.random_range(0..commits.len())]
                    };
                    read_commit(&shared, &format!("http://node{n}:7072"), c).await?;
                    compare_query(&shared, &format!("http://node{n}:7072"), c).await?;
                    observe_main(&shared, id).await?;
                }
                shared.clients_done.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
        }
    }
    // Writers.
    for w in 0..opts.writers {
        let shared = shared.clone();
        let rounds = opts.rounds;
        sim.client(format!("writer{w}"), async move {
            for k in 0..rounds {
                land(&shared, w, k).await?;
            }
            shared.clients_done.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
    }

    // Faults, from the seed. Steps are 5 ms of simulated time.
    let mut frng = rand::rngs::StdRng::seed_from_u64(seed.wrapping_mul(0x2545F4914F6CDD1D));
    let mut events: Vec<(u64, Fault)> = Vec::new();
    if opts.faults {
        for _ in 0..frng.random_range(2..6) {
            let at = frng.random_range(20..1500);
            let len = frng.random_range(20..400);
            let link = match frng.random_range(0..3) {
                0 => ("region".to_string(), "server".to_string()),
                1 => (
                    format!("node{}", frng.random_range(0..opts.nodes)),
                    "region".to_string(),
                ),
                _ => (
                    format!("writer{}", frng.random_range(0..opts.writers.max(1))),
                    "server".to_string(),
                ),
            };
            if link.0.starts_with("writer") && opts.writers == 0 {
                continue;
            }
            events.push((at, Fault::Partition(link.0.clone(), link.1.clone())));
            events.push((at + len, Fault::Repair(link.0, link.1)));
        }
        if frng.random_bool(0.7) {
            let at = frng.random_range(50..1500);
            events.push((at, Fault::Crash("region".into())));
            events.push((
                at + frng.random_range(10..300),
                Fault::Bounce("region".into()),
            ));
        }
        if frng.random_bool(0.5) {
            let n = frng.random_range(0..opts.nodes);
            let at = frng.random_range(50..1500);
            events.push((at, Fault::Crash(format!("node{n}"))));
            events.push((
                at + frng.random_range(10..300),
                Fault::Bounce(format!("node{n}")),
            ));
        }
    }
    events.sort_by_key(|e| e.0);
    let mut step = 0u64;
    let mut next = 0;
    loop {
        while next < events.len() && events[next].0 <= step {
            match &events[next].1 {
                Fault::Partition(a, b) => sim.partition(a.as_str(), b.as_str()),
                Fault::Repair(a, b) => sim.repair(a.as_str(), b.as_str()),
                Fault::Crash(h) => sim.crash(h.as_str()),
                Fault::Bounce(h) => sim.bounce(h.as_str()),
            }
            next += 1;
        }
        match sim.step() {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => {
                let v = shared.violations.lock().unwrap();
                panic!("seed {seed}: {e}; violations: {:#?}", &v[..v.len().min(5)])
            }
        }
        step += 1;
    }

    let final_main: Oid = git(&host, &["rev-parse", "main"]).trim().parse().unwrap();
    let main_chain = git(&host, &["rev-list", "--first-parent", "main"])
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    drop(tmp);
    Outcome {
        shared,
        final_main,
        main_chain,
        initial_main,
    }
}

enum Fault {
    Partition(String, String),
    Repair(String, String),
    Crash(String),
    Bounce(String),
}

/// Fetch objects through `base`, verifying each; returns them in order.
async fn fetch(shared: &Shared, base: &str, oids: &[Oid]) -> anyhow::Result<Vec<(Kind, Vec<u8>)>> {
    let body = serde_json::to_vec(&ObjectsRequest {
        oids: oids.to_vec(),
        bases: Vec::new(),
    })?;
    let r = call(
        Method::POST,
        &format!("{base}/v1/objects"),
        &[("content-type", "application/json")],
        body.into(),
        400,
    )
    .await?;
    anyhow::ensure!(r.status == StatusCode::OK, "objects: {}", r.status);
    let mut dec = FrameDecoder::default();
    let frames = dec.push(&r.body)?;
    dec.finish()?;
    let mut out = Vec::new();
    for (want, (oid, payload)) in oids.iter().zip(frames) {
        if oid != *want {
            shared.violation(format!("asked for {want}, got a frame for {oid}"));
            anyhow::bail!("bad frame order");
        }
        match payload {
            Payload::Loose(raw) => match object::decode_loose_verified(&oid, &raw) {
                Ok(o) => {
                    shared.objects_verified.fetch_add(1, Ordering::Relaxed);
                    out.push(o);
                }
                Err(e) => {
                    shared.violation(format!("{oid} failed verification: {e:#}"));
                    anyhow::bail!("verification");
                }
            },
            other => {
                shared.violation(format!("{oid}: unexpected frame {other:?}"));
                anyhow::bail!("missing object");
            }
        }
    }
    Ok(out)
}

/// Walk commit `c`'s whole tree through a node.
async fn read_commit(shared: &Shared, node: &str, c: Oid) -> anyhow::Result<()> {
    let commit = fetch(shared, node, &[c]).await?.remove(0);
    let tree = object::Commit::parse(&commit.1)?.tree;
    let mut level = vec![tree];
    let mut seen = HashSet::new();
    while !level.is_empty() {
        let trees = fetch(shared, node, &level).await?;
        let mut blobs = Vec::new();
        let mut next = Vec::new();
        for (_, data) in trees {
            for e in object::parse_tree(&data)? {
                if !seen.insert(e.oid) {
                    continue;
                }
                if e.is_tree() {
                    next.push(e.oid);
                } else {
                    blobs.push(e.oid);
                }
            }
        }
        if !blobs.is_empty() {
            fetch(shared, node, &blobs).await?;
        }
        level = next;
    }
    Ok(())
}

/// A pinned query through the tiers must equal the server's answer.
async fn compare_query(shared: &Shared, node: &str, c: Oid) -> anyhow::Result<()> {
    let path = format!("/api/v1/repos/demo/log?rev={c}&n=5");
    let tiered = call(
        Method::GET,
        &format!("{node}{path}"),
        &[],
        Bytes::new(),
        400,
    )
    .await?;
    let direct = call(
        Method::GET,
        &format!("http://server:7070{path}"),
        &[DIRECT],
        Bytes::new(),
        400,
    )
    .await?;
    if tiered.status != direct.status || tiered.body != direct.body {
        shared.violation(format!(
            "{path}: tiers answered {} {:?}, the server {} {:?}",
            tiered.status,
            String::from_utf8_lossy(&tiered.body),
            direct.status,
            String::from_utf8_lossy(&direct.body)
        ));
    }
    let sem = |r: &HttpResponse| {
        r.headers
            .get(server::SEMANTICS_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    if sem(&tiered).as_deref() != Some(QUERY_SEMANTICS) {
        shared.violation(format!(
            "{path}: semantics {:?} through the tiers",
            sem(&tiered)
        ));
    }
    Ok(())
}

/// Read main (unpinned, so forwarded to the server) and record it.
async fn observe_main(shared: &Shared, id: usize) -> anyhow::Result<()> {
    let r = call(
        Method::GET,
        "http://server:7070/api/v1/repos/demo/resolve?rev=main",
        &[DIRECT],
        Bytes::new(),
        400,
    )
    .await?;
    let v: serde_json::Value = serde_json::from_slice(&r.body)?;
    let oid: Oid = v["oid"].as_str().unwrap_or_default().parse()?;
    shared
        .observed
        .lock()
        .unwrap()
        .entry(id)
        .or_default()
        .push(oid);
    Ok(())
}

async fn land(shared: &Shared, w: usize, k: usize) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "parents": ["main"],
        "author": {"name": "W", "email": "w@wren.dev", "time": 1_700_000_000 + k, "tz": "+0000"},
        "message": format!("writer {w} change {k}"),
        "changes": [{"path": format!("sim/w{w}.txt"), "content": format!("{k}\n")}],
        "land": {"onto": "main"},
    });
    // A lost response means the land may or may not have happened; the
    // writer tries again (at least once, like any client).
    for _ in 0..400 {
        let r = call(
            Method::POST,
            "http://server:7070/api/v1/repos/demo/changesets",
            &[
                DIRECT,
                ("authorization", "Bearer w"),
                ("content-type", "application/json"),
            ],
            serde_json::to_vec(&body)?.into(),
            400,
        )
        .await?;
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
        if r.status == StatusCode::OK && v["land"]["result"] == "landed" {
            let new: Oid = v["land"]["new"].as_str().unwrap_or_default().parse()?;
            shared.landed.lock().unwrap().push(new);
            return Ok(());
        }
        // Lost the race too many times, or a conflict: try again.
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    anyhow::bail!("writer {w} never landed change {k}")
}

fn check(seed: u64, o: &Outcome) {
    let v = o.shared.violations.lock().unwrap();
    assert!(
        v.is_empty(),
        "seed {seed}: {} violations: {:#?}",
        v.len(),
        &v[..v.len().min(5)]
    );
    // 3. Linearizable main.
    let pos: HashMap<Oid, usize> = o
        .main_chain
        .iter()
        .rev()
        .enumerate()
        .map(|(i, c)| (*c, i))
        .collect();
    assert_eq!(
        pos.len(),
        o.main_chain.len(),
        "seed {seed}: main repeats a commit"
    );
    assert!(
        pos.contains_key(&o.initial_main),
        "seed {seed}: history rewritten"
    );
    for c in o.shared.landed.lock().unwrap().iter() {
        assert!(
            pos.contains_key(c),
            "seed {seed}: landed {c} is not on main {}",
            o.final_main
        );
    }
    for (id, seen) in o.shared.observed.lock().unwrap().iter() {
        let mut last = 0;
        for c in seen {
            let p = *pos
                .get(c)
                .unwrap_or_else(|| panic!("seed {seed}: reader {id} saw {c}, not on main"));
            assert!(p >= last, "seed {seed}: reader {id} saw main go backwards");
            last = p;
        }
    }
    // 4. The region's upstream limit holds at the server.
    let max = o.shared.from_region_max.load(Ordering::SeqCst);
    assert!(
        max <= REGION_LIMIT,
        "seed {seed}: {max} requests from the region in flight at once"
    );
}

fn seeds() -> Vec<u64> {
    if let Ok(s) = std::env::var("WREN_SIM_SEED") {
        return vec![s.parse().unwrap()];
    }
    let var = |k: &str, d: u64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (n, base) = (var("WREN_SIM_SEEDS", 24), var("WREN_SIM_SEED_BASE", 0));
    (base + 1..=base + n).collect()
}

#[test]
fn tiers_under_faults() {
    let opts = Opts {
        nodes: 2,
        readers_per_node: 2,
        writers: 2,
        rounds: 3,
        faults: true,
        snapshot: false,
        head_only: false,
    };
    let seeds = seeds();
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let verified = AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(&seed) = seeds.get(i) else { break };
                let r = std::panic::catch_unwind(|| {
                    let o = run(seed, opts);
                    check(seed, &o);
                    o.shared.objects_verified.load(Ordering::Relaxed)
                });
                match r {
                    Ok(n) => {
                        verified.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let msg = e
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_default();
                        failures.lock().unwrap().push((seed, msg));
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    eprintln!(
        "{} seeds, {} failed, {} objects verified",
        seeds.len(),
        failures.len(),
        verified.load(Ordering::Relaxed)
    );
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Many nodes start cold at once. Without snapshots, the region cache
/// coalesces their identical misses; with them, the nodes mostly fetch one
/// file, which the region downloads once.
#[test]
fn cold_start_of_many_nodes() {
    let nodes: usize = std::env::var("WREN_SIM_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let mut report = Vec::new();
    for snapshot in [false, true] {
        let o = run(
            7,
            Opts {
                nodes,
                readers_per_node: 1,
                writers: 0,
                rounds: 1,
                faults: false,
                snapshot,
                head_only: true,
            },
        );
        check(7, &o);
        let total = o.shared.from_region_total.load(Ordering::SeqCst);
        let max = o.shared.from_region_max.load(Ordering::SeqCst);
        let region = o.shared.to_region_total.load(Ordering::SeqCst);
        report.push((snapshot, region, total, max));
    }
    eprintln!(
        "cold start of {nodes} nodes (snapshot, requests to the region, \
         requests to the server, most in flight at the server): {report:?}"
    );
    // One node walking the tip alone asks the server about ten times (the
    // commit, then trees and blobs level by level, a query, the tier
    // check); fifty at once must cost about the same, not fifty times
    // more. With a snapshot, the objects come in one file.
    let (_, _, without, _) = report[0];
    let (_, _, with, _) = report[1];
    assert!(without <= 20, "{report:?}");
    assert!(with <= 8, "{report:?}");
}
