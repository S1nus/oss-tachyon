//! Test support for this crate and its dependents (the `testing` feature): a
//! fake Zakura JSON-RPC node, ported from devtool `wallet/tests.rs`. Build its
//! chains with `oss_core::evidence::testing`.

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use clap::Parser;
use oss_core::TachyonBlock;
use serde_json::{Value, json};

use crate::{builder, config::Config, server, store};

/// Zakura's error text for a pruned body.
const PRUNED: &str = "Tachyon block data unavailable: body pruned or not retained; use an archive node or previously saved sync data";

pub struct NodeData {
    /// The best chain, from the activation block up.
    pub blocks: Vec<TachyonBlock>,
    /// `gettachyonblock` fails at these heights as Zakura does for a pruned body.
    pub pruned_heights: BTreeSet<u32>,
    /// Every `(method, params)` served, in order.
    pub requests: Vec<(String, Value)>,
    reorgs: u8,
}

/// Serves `getbestblockhash`, `getblockhash`, `getblockcount`,
/// `getblockchaininfo` and `gettachyonblock` over HTTP at `url`.
pub struct FakeNode {
    pub url: String,
    data: Arc<Mutex<NodeData>>,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr); // wakes the blocking accept
        let _ = self.worker.take().map(thread::JoinHandle::join);
    }
}

impl FakeNode {
    pub fn start(blocks: Vec<TachyonBlock>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let data = Arc::new(Mutex::new(NodeData {
            blocks,
            pruned_heights: BTreeSet::new(),
            requests: vec![],
            reorgs: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_data, thread_stop) = (data.clone(), stop.clone());
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(stream) = stream {
                    // A client that hangs up early must not stop the node.
                    let _ = serve(&thread_data, stream);
                }
            }
        });
        Ok(Self {
            url: format!("http://{addr}"),
            data,
            addr,
            stop,
            worker: Some(worker),
        })
    }

    pub fn data(&self) -> MutexGuard<'_, NodeData> {
        self.data.lock().unwrap()
    }

    /// Replaces the chain from height `from` with `branch`, rewriting its
    /// hashes as a competing branch's: `evidence::testing::mine` hashes by
    /// height alone, so the reorg would otherwise be invisible. Sync before
    /// mining past the replaced tip, for the same reason.
    pub fn reorg(&self, from: u32, mut branch: Vec<TachyonBlock>) {
        assert!(branch.first().is_none_or(|b| b.height == from));
        let mut data = self.data();
        data.reorgs += 1;
        data.blocks.retain(|b| b.height < from);
        let mut parent = data.blocks.last().map(|b| b.hash.clone());
        for block in &mut branch {
            block.hash = format!("{:02x}{:062x}", data.reorgs, block.height);
            if let Some(parent) = parent.replace(block.hash.clone()) {
                block.previous_block_hash = parent;
            }
        }
        data.blocks.extend(branch);
    }

    /// Prunes every body below `height`, as Zakura's pruned storage mode does.
    pub fn prune_below(&self, height: u32) {
        let mut data = self.data();
        let first = data.blocks.first().map_or(height, |b| b.height);
        data.pruned_heights.extend(first..height);
    }
}

/// `block` as `gettachyonblock` serves it: with `finalized`, without the
/// devtool-only `transactions`.
pub fn served(block: &TachyonBlock) -> Value {
    let mut value = serde_json::to_value(block).expect("blocks serialize");
    let fields = value.as_object_mut().expect("a block is an object");
    fields.remove("transactions");
    fields.insert("finalized".into(), json!(false));
    value
}

fn serve(data: &Mutex<NodeData>, stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream);
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse()?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body)?;
    let response = serde_json::to_vec(&data.lock().unwrap().answer(&request))?;
    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        response.len()
    )?;
    stream.write_all(&response)?;
    Ok(())
}

impl NodeData {
    fn answer(&mut self, request: &Value) -> Value {
        let (method, params) = (
            request["method"].as_str().unwrap_or_default(),
            &request["params"],
        );
        self.requests.push((method.into(), params.clone()));
        let at = |height: Option<u64>| {
            self.blocks
                .iter()
                .find(|b| Some(u64::from(b.height)) == height)
        };
        let tip = self.blocks.last().expect("the fake chain is never empty");
        let result = match method {
            "getblockchaininfo" => {
                Ok(json!({"chain": "test", "blocks": tip.height, "bestblockhash": tip.hash}))
            }
            "getblockcount" => Ok(json!(tip.height)),
            "getbestblockhash" => Ok(json!(tip.hash)),
            "getblockhash" => match params[0].as_u64() {
                Some(0) => Ok(json!(hex::encode([0; 32]))),
                height => at(height)
                    .map(|b| json!(b.hash))
                    .ok_or((-8, "Block height out of range")),
            },
            "gettachyonblock" => {
                let height = params[0].as_str().and_then(|h| h.parse::<u32>().ok());
                if height.is_some_and(|h| self.pruned_heights.contains(&h)) {
                    Err((-1, PRUNED))
                } else {
                    at(height.map(u64::from))
                        .map(served)
                        .ok_or((-8, "the requested block is not in the best chain"))
                }
            }
            _ => Err((-32601, "Method not found")),
        };
        match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": request["id"],
                "error": {"code": code, "message": message}}),
        }
    }
}

/// A running OSS on its own thread and runtime, from [`spawn_oss`]. Dropping
/// it shuts the OSS down and waits for it.
pub struct OssHandle {
    db: PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

/// Starts the whole OSS (follower, builder, server) against `node_url` with
/// the database at `db`, on a free localhost port, with default flags. Returns
/// its URL once the archive identity is checked. Usable from sync and async
/// tests alike.
pub fn spawn_oss(node_url: &str, db: &Path) -> Result<(String, OssHandle)> {
    let config = Config::try_parse_from(
        ["tachyon-oss", "--node-url", node_url, "--db"]
            .map(OsStr::new)
            .into_iter()
            .chain([db.as_os_str()]),
    )?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let url = format!("http://{}", listener.local_addr()?);
    let (shutdown, stop) = tokio::sync::oneshot::channel::<()>();
    let (started, start) = std::sync::mpsc::channel();
    let thread = thread::spawn(move || {
        tokio::runtime::Runtime::new()?.block_on(async move {
            let oss = match server::start(&config).await {
                Ok(oss) => oss,
                Err(e) => {
                    let _ = started.send(Err(e));
                    return Ok(());
                }
            };
            let _ = started.send(Ok(()));
            let listener = tokio::net::TcpListener::from_std(listener)?;
            oss.serve(listener, async {
                let _ = stop.await;
            })
            .await
        })
    });
    start
        .recv()
        .map_err(|_| anyhow!("the OSS thread failed to start"))??;
    let handle = OssHandle {
        db: db.into(),
        shutdown: Some(shutdown),
        thread: Some(thread),
    };
    Ok((url, handle))
}

impl OssHandle {
    /// Waits until `evidenceReadyThrough >= epoch`.
    pub fn wait_ready(&self, epoch: u32) -> Result<()> {
        self.wait(builder::READY_THROUGH, epoch)
    }

    /// Waits until the builder has built `height`: it is archived and its
    /// spans (and any evidence it closes) are served.
    pub fn wait_built(&self, height: u32) -> Result<()> {
        self.wait(builder::BUILDER_HEIGHT, height)
    }

    fn wait(&self, key: &str, at_least: u32) -> Result<()> {
        let conn = store::open_read(&self.db)?;
        for _ in 0..3000 {
            if store::meta::<u32>(&conn, key)?.is_some_and(|v| v >= at_least) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
        bail!("timed out waiting for {key} >= {at_least}")
    }
}

impl Drop for OssHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.take().map(|s| s.send(()));
        if let Some(Ok(Err(e))) = self.thread.take().map(thread::JoinHandle::join) {
            eprintln!("oss: {e:#}");
        }
    }
}

/// A database path in the temp dir, removed with its WAL files on drop.
pub struct TempDb(pub PathBuf);

impl TempDb {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "oss-test-{}-{}.sqlite",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let db = Self(std::env::temp_dir().join(name));
        db.remove();
        db
    }

    fn remove(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

impl Default for TempDb {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}
