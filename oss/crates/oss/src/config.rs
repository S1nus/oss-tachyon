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
    /// Most `oss_unspent` proving jobs at once; more requests wait.
    #[arg(long, default_value_t = 4)]
    pub max_unspent_jobs: usize,
}
