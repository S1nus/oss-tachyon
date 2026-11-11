use clap::Parser;
use oss::{config::Config, server};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let oss = server::start(&config).await?;
    eprintln!(
        "tachyon-oss: {} on http://{}",
        config.db.display(),
        listener.local_addr()?
    );
    // Returns an error, and exits non-zero, if the follower stops on a pruned gap.
    oss.serve(listener, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
