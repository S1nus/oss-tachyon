//! Command-line flags of `tachyon-oss`.

use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

/// The Tachyon Oblivious Synchronization Service: follows a Zakura node,
/// archives Tachyon blocks, builds epoch evidence and anchor spans, and serves
/// them over JSON-RPC.
#[derive(Clone, Debug, Parser)]
#[command(name = "tachyon-oss", version)]
pub struct Config {
    /// Zakura's JSON-RPC URL.
    #[arg(long)]
    pub node_url: String,
    /// Zakura's RPC cookie file (`__cookie__:<token>`).
    #[arg(long)]
    pub node_cookie: Option<PathBuf>,
    /// The SQLite database, created if missing.
    #[arg(long)]
    pub db: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8240")]
    pub listen: SocketAddr,
    /// Write a fresh `__cookie__:<token>` here on start and require it as HTTP
    /// Basic auth, as Zakura does.
    #[arg(long)]
    pub cookie_file: Option<PathBuf>,
    /// Start an empty archive at this epoch boundary instead of activation.
    #[arg(long)]
    pub start_height: Option<u32>,
    /// An archive node to fetch block bodies the node has pruned.
    #[arg(long)]
    pub backfill_url: Option<String>,
    /// Worker threads for one `oss_unspent` batch: proof steps of different
    /// requests run in parallel up to this many at once.
    #[arg(long, default_value_t = 4)]
    pub max_unspent_jobs: usize,
    /// Most `oss_unspent` requests proven in one batch; more wait for the next.
    #[arg(long, default_value_t = 64)]
    pub unspent_batch_max: usize,
    /// After the first request of a batch, wait this long for others to join
    /// it. 0 batches only what already queued while the last batch ran.
    #[arg(long, default_value_t = 0)]
    pub unspent_batch_window_ms: u64,
}
