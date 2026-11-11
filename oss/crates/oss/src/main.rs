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
    // Ctrl-C or SIGTERM (systemd stop) shuts down gracefully. Returns an error,
    // and exits non-zero, if the follower stops on a pruned gap.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    oss.serve(listener, async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    })
    .await
}
