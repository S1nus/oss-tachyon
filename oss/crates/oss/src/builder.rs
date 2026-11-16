//! The builder thread (design §2.4, §3): brings the span store and the epoch
//! evidence up to the archive from its own cursor. The follower's tip `watch`
//! only wakes it, so a missed wake-up costs nothing.
//!
//! `meta` keys written here:
//! - `builder_height`: the last archived height built (spans, and the previous
//!   epoch's evidence when the block opens an epoch).
//! - `evidence_ready_through`: the newest epoch sealed, lowered below the
//!   epoch a reorg reaches.
//! - `rewind_from`: cleared, compare-and-clear, once applied.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use oss_core::anchor;
use rusqlite::{Connection, params};
use tokio::{runtime::Handle, sync::watch};
use udon::field::Field;
use zcash_tachyon::{Anchor, QrDiscriminant, constants::EPOCH_SIZE, pasta::Fp};

use crate::{archive, epoch_chunks, epochs, span_store, store};

pub const BUILDER_HEIGHT: &str = "builder_height";
pub const READY_THROUGH: &str = "evidence_ready_through";

/// Retry after an error even if no block arrives.
const RETRY: Duration = Duration::from_secs(1);

/// Builds on every wake-up until the follower drops its tip channel. Errors
/// (e.g. a reorg racing a read) are logged and retried.
pub fn run(conn: &Connection, mut tip: watch::Receiver<u32>, runtime: &Handle) {
    loop {
        tip.mark_unchanged();
        if let Err(e) = catch_up(conn, &|| tip.has_changed().is_err()) {
            eprintln!("builder: {e:#}");
        }
        let closed = runtime.block_on(async {
            tokio::select! {
                changed = tip.changed() => changed.is_err(),
                () = tokio::time::sleep(RETRY) => false,
            }
        });
        if closed {
            return;
        }
    }
}

/// Builds every archived height above the cursor, in order, applying any
/// pending rewind before each. Returns at the archive tip, or once `stop()`.
pub fn catch_up(conn: &Connection, stop: &dyn Fn() -> bool) -> Result<()> {
    // The open epoch's chunks stay in memory; a rewind discards them.
    let mut chunks: Option<oss_core::ChunkBuilder> = None;
    while !stop() {
        if rewind(conn)? {
            chunks = None;
        }
        let next = match store::meta::<u32>(conn, BUILDER_HEIGHT)? {
            Some(built) => built + 1,
            None => match archive::first_height(conn)? {
                Some(first) => first,
                None => return Ok(()),
            },
        };
        let Some(block) = archive::block(conn, next)? else {
            return Ok(());
        };
        span_store::connect(conn, &block)?;
        if block.epoch > 0 && block.epoch_start_anchor.is_some() {
            seal(conn, block.epoch - 1, block.entry()?)?;
        }
        // Chunks and the cursor move together: a replay must never fold the
        // same stamps into an epoch twice.
        // Empty blocks cannot change a chunk once the epoch is open; only the
        // first block of an epoch still needs a look (to start it or to mark a
        // mid-epoch join for the archive fallback).
        let prepared = if block.stamps.is_empty()
            && chunks
                .as_ref()
                .is_some_and(|builder| builder.epoch() == block.epoch)
        {
            None
        } else {
            epoch_chunks::prepare(conn, &mut chunks, &block)
                .with_context(|| format!("chunks at {}", block.height))?
        };
        // Chunks and the cursor commit together when the block changed a chunk;
        // otherwise the plain cursor write keeps empty blocks as cheap as before.
        match prepared.as_ref().filter(|prepared| !prepared.is_empty()) {
            Some(prepared) => {
                let tx = conn
                    .unchecked_transaction()
                    .with_context(|| format!("begin at {}", block.height))?;
                prepared
                    .write(&tx)
                    .with_context(|| format!("write chunks at {}", block.height))?;
                set(&tx, BUILDER_HEIGHT, next)
                    .with_context(|| format!("cursor at {}", block.height))?;
                tx.commit()
                    .with_context(|| format!("commit at {}", block.height))?;
            }
            None => set(conn, BUILDER_HEIGHT, next)?,
        }
    }
    Ok(())
}

/// Applies the follower's pending `rewind_from` in one transaction: drops the
/// spans it invalidates, moves the cursor back to replay them, and lowers
/// `evidence_ready_through` below the epoch whose closing block it reaches.
fn rewind(conn: &Connection) -> Result<bool> {
    let Some(from) = store::meta::<String>(conn, "rewind_from")? else {
        return Ok(false);
    };
    let height: u32 = from.parse()?;
    let activation: u32 =
        store::meta(conn, "activation_height")?.context("no activation height")?;
    let epoch = height.saturating_sub(activation) / EPOCH_SIZE;
    let tx = conn.unchecked_transaction()?;
    let replay = span_store::rewind(&tx, height)?;
    // A chunk can straddle the fork point: drop the epoch's chunks and let its
    // seal rebuild from the archive. The span rewind still decides the replay.
    epoch_chunks::invalidate(&tx, epoch)?;
    let lower = "UPDATE meta SET value = MIN(CAST(value AS INTEGER), ?2) WHERE key = ?1";
    tx.execute(lower, params![BUILDER_HEIGHT, replay.saturating_sub(1)])?;
    match epoch.checked_sub(1) {
        Some(ready) => tx.execute(lower, params![READY_THROUGH, ready])?,
        None => tx.execute("DELETE FROM meta WHERE key = ?1", [READY_THROUGH])?,
    };
    // A lower `rewind_from` written since the read survives for the next pass.
    tx.execute(
        "DELETE FROM meta WHERE key = 'rewind_from' AND value = ?1",
        [&from],
    )?;
    tx.commit()?;
    Ok(true)
}

/// Seals `epoch` under its canonical closing hash and marks it ready. The
/// chunks accumulated while the epoch was open are routed here; an epoch that
/// predates `epoch_chunks` (the upgrade's first seal) is rebuilt from the
/// archive instead. A no-op without a closing hash.
fn seal(conn: &Connection, epoch: u32, next: Anchor) -> Result<()> {
    let Some(closing) = archive::closing_hash(conn, epoch)? else {
        return Ok(());
    };
    if epochs::evidence(conn, epoch, &closing)?.is_some() {
        set(conn, READY_THROUGH, epoch)?;
        return Ok(());
    }
    // R0 is normally created when the epoch's first stamped block is folded in,
    // but an epoch with no stamps never reaches that path.
    conn.execute(
        "INSERT OR IGNORE INTO epochs(epoch, discriminant) VALUES (?1, ?2)",
        params![epoch, QrDiscriminant::random(&mut rand::rng()).0.to_bytes()],
    )?;
    let first = archive::first_block(conn, epoch)?.context("a closed epoch has blocks")?;
    let start = first.entry()?;
    let predecessor = match epoch {
        0 => Anchor(Fp::ZERO),
        _ => anchor(&first.anchor_before)?,
    };
    let r0 = epochs::discriminant(conn, epoch)?.context("missing discriminant")?;
    // Minimal bucket depth on both paths: the persisted chunks exist so a later
    // leaf accumulator can predict a depth and re-route when it is exceeded,
    // not to force a deeper (smaller-anonymity) tree today.
    let (buckets, members) = match epoch_chunks::load(conn, epoch)? {
        Some(builder) => {
            let chunks: Vec<oss_core::Intake> = builder.chunks().to_vec();
            let members: usize = chunks.iter().map(|chunk| chunk.members.len()).sum();
            // Option B: use the predicted leaves only when they still match the
            // minimal profile set; otherwise route the stored chunks.
            let sealed = if chunks.is_empty() {
                None
            } else {
                match epoch_chunks::seal_leaves(
                    conn,
                    epoch,
                    &chunks,
                    r0,
                    start,
                    next,
                    predecessor,
                ) {
                    Ok(buckets) => buckets,
                    Err(error) => {
                        eprintln!("seal leaves {epoch}: {error:#}");
                        None
                    }
                }
            };
            match sealed {
                Some(buckets) => (buckets, members),
                None => (builder.seal(start, next, predecessor)?, members),
            }
        }
        None => {
            let blocks = archive::epoch_blocks(conn, epoch)?;
            ensure!(
                blocks.first().is_some_and(|b| b.epoch == epoch),
                "archive does not hold epoch {epoch}"
            );
            (
                oss_core::build_epoch(&blocks, predecessor, r0)?,
                usize::try_from(epoch_chunks::member_count(conn, epoch)?)?,
            )
        }
    };
    epochs::write_evidence(conn, epoch, &closing, &buckets)?;
    // The next epoch predicts its leaf depth from this one's member count.
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('last_sealed_members', ?1)",
        [members.to_string()],
    )?;
    epoch_chunks::sealed(conn, epoch)?;
    set(conn, READY_THROUGH, epoch)
}

fn set(conn: &Connection, key: &str, value: u32) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}
