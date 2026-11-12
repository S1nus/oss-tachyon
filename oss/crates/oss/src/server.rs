//! JSON-RPC 2.0 over `POST /` (design §4.1, §4.2), and the process around it:
//! the follower task, the builder thread and the server.

use std::{
    fmt,
    fs::OpenOptions,
    future::{Future, IntoFuture},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use oss_core::{
    AnchorSpansResult, BlockParams, BucketParams, BucketProfile, BucketResult, EpochEvidence,
    EpochParams, EpochResult, ErrorCode, EvidenceStatus, InfoResult, MAX_SPAN_FROMS, NodeStatus,
    NotReadyData, ProofSystem, SpentData, Tip, anchor, anchor_hex, pcd_to_hex,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};
use tower_http::compression::CompressionLayer;
use udon::field::Field;
use zcash_tachyon::constants::EPOCH_SIZE;

use crate::{
    archive, builder,
    config::Config,
    epochs,
    follower::{Follower, Node},
    span_store, store, unspent_service,
};

/// The revs pinned in `oss/Cargo.toml`. Another rev is another PCD application.
const TACHYON_REV: &str = "088f4e1df789fc507742a9a08d27a07604c6a247";
const RAGU_REV: &str = "839cf50d1ff31c1d8b89c1f90f808f76422cec17";
const MAX_REQUEST: usize = 1 << 20;
/// The follower polls the node every second.
const RETRY_AFTER_MS: u64 = 1000;
/// Zakura's finalization depth.
const FINALITY: u32 = 1000;

/// An RPC error with a design §4.2 code. Any other error answers -32603.
#[derive(Debug)]
pub enum Failure {
    InvalidParams(String),
    NotReady(String),
    Spent(SpentData),
    Unavailable(String),
    MethodNotFound(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidParams(message) => write!(f, "invalid params: {message}"),
            Self::NotReady(reason) => write!(f, "not ready: {reason}"),
            Self::Spent(SpentData {
                epoch,
                height,
                txid,
            }) => write!(
                f,
                "spent in epoch {epoch}, block {height}, transaction {txid}"
            ),
            Self::Unavailable(message) => write!(f, "unavailable: {message}"),
            Self::MethodNotFound(method) => write!(f, "method not found: {method}"),
        }
    }
}

impl std::error::Error for Failure {}

fn error_object(error: &anyhow::Error) -> Value {
    let (code, data) = match error.downcast_ref::<Failure>() {
        Some(Failure::InvalidParams(message)) => (ErrorCode::InvalidParams.code(), json!(message)),
        Some(Failure::NotReady(reason)) => (
            ErrorCode::NotReady.code(),
            json!(NotReadyData {
                retry_after_ms: RETRY_AFTER_MS,
                reason: reason.clone(),
            }),
        ),
        Some(Failure::Spent(data)) => (ErrorCode::Spent.code(), json!(data)),
        Some(Failure::Unavailable(_)) => (ErrorCode::Unavailable.code(), Value::Null),
        Some(Failure::MethodNotFound(_)) => (-32601, Value::Null),
        None => {
            eprintln!("rpc: {error:#}");
            (-32603, Value::Null)
        }
    };
    let mut object = json!({"code": code, "message": format!("{error:#}")});
    if !data.is_null() {
        object["data"] = data;
    }
    object
}

/// What every request shares.
pub(crate) struct Shared {
    pub(crate) db: PathBuf,
    /// The Basic-auth password (the cookie token), if auth is on.
    pub(crate) cookie: Option<String>,
    pub(crate) jobs: Arc<Semaphore>,
    /// The node's height, polled every second.
    pub(crate) node_height: Arc<AtomicU32>,
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", post(rpc))
        .layer(DefaultBodyLimit::max(MAX_REQUEST))
        .layer(CompressionLayer::new())
        .with_state(shared)
}

async fn rpc(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    request: Result<Json<Request>, JsonRejection>,
) -> Response {
    if !authorized(shared.cookie.as_deref(), &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => return rejection.into_response(),
    };
    let mut response = json!({"jsonrpc": "2.0", "id": request.id});
    match call(&shared, &request.method, request.params).await {
        Ok(result) => response["result"] = result,
        Err(error) => response["error"] = error_object(&error),
    }
    Json(response).into_response()
}

/// Zakura's check: the password of `Authorization: Basic …` is the cookie.
fn authorized(cookie: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(cookie) = cookie else {
        return true;
    };
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_whitespace().nth(1))
        .and_then(|encoded| STANDARD.decode(encoded).ok())
        .and_then(|decoded| String::from_utf8(decoded).ok())
        .is_some_and(|credentials| {
            let password = credentials.split_once(':').map_or("", |(_, p)| p);
            // Constant time in the password's content.
            password.len() == cookie.len()
                && password
                    .bytes()
                    .zip(cookie.bytes())
                    .fold(0, |diff, (a, b)| diff | (a ^ b))
                    == 0
        })
}

async fn call(shared: &Shared, method: &str, params: Value) -> Result<Value> {
    match method {
        "oss_info" => {
            let node = shared.node_height.load(Ordering::Relaxed);
            read(shared, move |conn| json(info(conn, node)?)).await
        }
        "oss_block" => {
            let BlockParams { height } = parse(params)?;
            read(shared, move |conn| block(conn, height)).await
        }
        "oss_epoch" => {
            let EpochParams { epoch: e } = parse(params)?;
            read(shared, move |conn| json(epoch(conn, e)?)).await
        }
        "oss_bucket" => {
            let params: BucketParams = parse(params)?;
            read(shared, move |conn| json(bucket(conn, params)?)).await
        }
        "oss_anchorspans" => {
            let params: oss_core::AnchorSpansParams = parse(params)?;
            read(shared, move |conn| json(anchor_spans(conn, &params.from)?)).await
        }
        "oss_unspent" => {
            let params: oss_core::UnspentParams = parse(params)?;
            let job = read(shared, move |conn| unspent_service::check(conn, &params)).await?;
            // The slot moves into the proving thread: if the client hangs up,
            // axum drops this future, but the proof runs on and keeps its slot.
            let permit = shared.jobs.clone().acquire_owned().await?;
            read(shared, move |conn| {
                let _permit = permit;
                json(unspent_service::prove(conn, &job)?)
            })
            .await
        }
        _ => Err(Failure::MethodNotFound(method.into()).into()),
    }
}

/// Runs `f` on a blocking thread with a fresh read-only connection.
async fn read<T: Send + 'static>(
    shared: &Shared,
    f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    let db = shared.db.clone();
    tokio::task::spawn_blocking(move || f(&store::open_read(&db)?)).await?
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T> {
    serde_json::from_value(params).map_err(|e| Failure::InvalidParams(e.to_string()).into())
}

fn json(value: impl Serialize) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

fn info(conn: &Connection, node_height: u32) -> Result<InfoResult> {
    let tx = conn.unchecked_transaction()?;
    let tip = archive::tip(&tx)?.ok_or_else(|| Failure::NotReady("the archive is empty".into()))?;
    let entry =
        archive::entry_anchor(&tx, tip.epoch)?.context("the tip epoch has a first block")?;
    Ok(InfoResult {
        genesis_hash: store::meta(&tx, "genesis_hash")?.context("no genesis hash")?,
        activation_height: tip.activation_height,
        epoch_length: EPOCH_SIZE,
        proof_system: ProofSystem {
            tachyon: TACHYON_REV.into(),
            ragu: RAGU_REV.into(),
            mock: true,
        },
        archive_from_height: archive::first_height(&tx)?.context("a non-empty archive")?,
        evidence_ready_through: store::meta(&tx, builder::READY_THROUGH)?,
        node: NodeStatus {
            height: node_height,
            lag_blocks: node_height.saturating_sub(tip.height),
        },
        tip: Tip {
            height: tip.height,
            hash: tip.hash,
            epoch: tip.epoch,
            anchor: tip.anchor_after,
            entry_anchor: anchor_hex(entry),
        },
    })
}

/// The stored JSON with `finalized` recomputed from the current tip.
fn block(conn: &Connection, height: u32) -> Result<Value> {
    let tx = conn.unchecked_transaction()?;
    let (Some(tip), Some(json)) = (archive::tip(&tx)?, archive::block_json(&tx, height)?) else {
        bail!(Failure::Unavailable(format!(
            "block {height} is not archived"
        )));
    };
    let mut block: Value = serde_json::from_str(&json)?;
    block["finalized"] = json!(tip.height.saturating_sub(height) >= FINALITY);
    Ok(block)
}

fn epoch(conn: &Connection, epoch: u32) -> Result<EpochResult> {
    let tx = conn.unchecked_transaction()?;
    below_archive(&tx, epoch)?;
    let closing_hash = archive::closing_hash(&tx, epoch)?;
    let status = |status| EpochEvidence {
        status,
        discriminant: None,
        profiles: None,
    };
    let evidence = match &closing_hash {
        None => status(EvidenceStatus::Open),
        Some(closing) => match epochs::evidence(&tx, epoch, closing)? {
            None => status(EvidenceStatus::Building),
            Some(profiles) => {
                let r0 = epochs::discriminant(&tx, epoch)?.context("a sealed epoch has R0")?;
                EpochEvidence {
                    status: EvidenceStatus::Ready,
                    discriminant: Some(hex::encode(r0.0.to_bytes())),
                    profiles: Some(profiles),
                }
            }
        },
    };
    Ok(EpochResult {
        epoch,
        closing_hash,
        evidence,
    })
}

fn bucket(
    conn: &Connection,
    BucketParams { epoch, profile }: BucketParams,
) -> Result<BucketResult> {
    let tx = conn.unchecked_transaction()?;
    let (closing, _) = sealed(&tx, epoch)?;
    let pcd = epochs::bucket(&tx, epoch, &closing, profile)?.ok_or_else(|| {
        Failure::Unavailable(format!(
            "epoch {epoch} has no bucket at depth {} bits {}",
            profile.depth, profile.bits
        ))
    })?;
    Ok(BucketResult {
        pcd: pcd_to_hex(&pcd)?,
    })
}

fn anchor_spans(conn: &Connection, from: &[String]) -> Result<AnchorSpansResult> {
    if !(1..=MAX_SPAN_FROMS).contains(&from.len()) {
        bail!(Failure::InvalidParams(format!(
            "expected 1 to {MAX_SPAN_FROMS} anchors, got {}",
            from.len()
        )));
    }
    let anchors = from
        .iter()
        .enumerate()
        .map(|(i, a)| {
            anchor(a).map_err(|_| Failure::InvalidParams(format!("from[{i}] is not an anchor")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some((epoch, (target, chunks, start))) = span_store::spans_from(conn, &anchors)? else {
        let built = store::meta::<u32>(conn, builder::BUILDER_HEIGHT)?;
        let tip = archive::tip(conn)?.map(|b| b.height);
        if tip.is_some_and(|tip| built.is_none_or(|built| built < tip)) {
            bail!(Failure::NotReady(
                "the span builder is behind the archive".into()
            ));
        }
        bail!(Failure::Unavailable(
            "the anchors are not all in the spans of one current or previous epoch".into()
        ));
    };
    Ok(AnchorSpansResult {
        epoch,
        target: anchor_hex(target),
        chunks: chunks.iter().map(pcd_to_hex).collect::<Result<_>>()?,
        start: start
            .into_iter()
            .map(|s| s.map(u32::try_from).transpose())
            .collect::<Result<_, _>>()?,
    })
}

/// `Unavailable` for an epoch older than the archive; `NotReady` while empty.
fn below_archive(conn: &Connection, epoch: u32) -> Result<()> {
    let first = archive::first_height(conn)?
        .map(|height| archive::block(conn, height))
        .transpose()?
        .flatten()
        .ok_or_else(|| Failure::NotReady("the archive is empty".into()))?;
    if epoch < first.epoch {
        bail!(Failure::Unavailable(format!(
            "epoch {epoch} is older than the archive, which starts at epoch {}",
            first.epoch
        )));
    }
    Ok(())
}

/// The canonical closing hash and profiles of a sealed epoch: `Unavailable`
/// below the archive, `NotReady` while it is open or its evidence building.
pub(crate) fn sealed(conn: &Connection, epoch: u32) -> Result<(String, Vec<BucketProfile>)> {
    below_archive(conn, epoch)?;
    let closing = archive::closing_hash(conn, epoch)?
        .ok_or_else(|| Failure::NotReady(format!("epoch {epoch} is open")))?;
    let profiles = epochs::evidence(conn, epoch, &closing)?
        .ok_or_else(|| Failure::NotReady(format!("epoch {epoch}'s evidence is building")))?;
    Ok((closing, profiles))
}

/// A started OSS: archive identity checked, builder thread running. Call
/// [`Oss::serve`] to run the follower and the RPC server.
pub struct Oss {
    follower: Follower,
    builder: std::thread::JoinHandle<()>,
    poller: JoinHandle<()>,
    shared: Arc<Shared>,
}

pub async fn start(config: &Config) -> Result<Oss> {
    ensure!(
        config.max_unspent_jobs > 0,
        "--max-unspent-jobs must be positive"
    );
    let node = || Node::new(&config.node_url, config.node_cookie.clone());
    let backfill = config
        .backfill_url
        .as_deref()
        .map(|url| Node::new(url, None));
    let follower = Follower::new(
        store::open(&config.db)?,
        node()?,
        backfill.transpose()?,
        config.start_height,
    )
    .await?;
    let cookie = config
        .cookie_file
        .as_deref()
        .map(write_cookie)
        .transpose()?;
    let poller_node = node()?;
    let node_height = Arc::new(AtomicU32::new(poller_node.height().await.unwrap_or(0)));
    let poller = tokio::spawn(poll_height(poller_node, node_height.clone()));
    let (conn, tip, runtime) = (
        store::open(&config.db)?,
        follower.subscribe(),
        tokio::runtime::Handle::current(),
    );
    let builder = std::thread::Builder::new()
        .name("builder".into())
        .spawn(move || builder::run(&conn, tip, &runtime))?;
    Ok(Oss {
        follower,
        builder,
        poller,
        shared: Arc::new(Shared {
            db: config.db.clone(),
            cookie,
            jobs: Arc::new(Semaphore::new(config.max_unspent_jobs)),
            node_height,
        }),
    })
}

impl Oss {
    /// Serves on `listener` until `shutdown` resolves, or until the follower
    /// fails ([`crate::follower::Pruned`]), which is returned. Then stops the
    /// builder after its current block or seal.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let Self {
            follower,
            builder,
            poller,
            shared,
        } = self;
        let server = axum::serve(listener, router(shared)).with_graceful_shutdown(shutdown);
        let result = tokio::select! {
            result = follower.run() => result,
            result = server.into_future() => result.map_err(Into::into),
        };
        poller.abort();
        // The follower is dropped, closing its tip channel: the builder returns.
        tokio::task::spawn_blocking(move || builder.join())
            .await?
            .map_err(|_| anyhow!("the builder thread panicked"))?;
        result
    }
}

async fn poll_height(node: Node, height: Arc<AtomicU32>) {
    loop {
        if let Ok(h) = node.height().await {
            height.store(h, Ordering::Relaxed);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Writes a fresh `__cookie__:<token>` file, owner-only, as Zakura does, and
/// returns the token.
pub(crate) fn write_cookie(path: &Path) -> Result<String> {
    let token = hex::encode(rand::random::<[u8; 32]>());
    // A new file, so an old one's permissions are not reused.
    let _ = std::fs::remove_file(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot write the cookie file {}", path.display()))?;
    write!(file, "__cookie__:{token}")?;
    Ok(token)
}
