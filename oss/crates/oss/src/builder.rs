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

use anyhow::{Context, Result};
use oss_core::anchor;
use rusqlite::{Connection, params};
use tokio::{runtime::Handle, sync::watch};
use udon::field::Field;
use zcash_tachyon::{Anchor, constants::EPOCH_SIZE, pasta::Fp};

use crate::{archive, epochs, span_store, store};

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
    while !stop() {
        rewind(conn)?;
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
            seal(conn, block.epoch - 1)?;
        }
        set(conn, BUILDER_HEIGHT, next)?;
    }
    Ok(())
}

/// Applies the follower's pending `rewind_from` in one transaction: drops the
/// spans it invalidates, moves the cursor back to replay them, and lowers
/// `evidence_ready_through` below the epoch whose closing block it reaches.
fn rewind(conn: &Connection) -> Result<()> {
    let Some(from) = store::meta::<String>(conn, "rewind_from")? else {
        return Ok(());
    };
    let height: u32 = from.parse()?;
    let activation: u32 =
        store::meta(conn, "activation_height")?.context("no activation height")?;
    let epoch = height.saturating_sub(activation) / EPOCH_SIZE;
    let tx = conn.unchecked_transaction()?;
    let replay = span_store::rewind(&tx, height)?;
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
    Ok(())
}

/// Seals `epoch` under its canonical closing hash and marks it ready. A no-op
/// for an epoch the archive did not see in full (it starts at a checkpoint).
fn seal(conn: &Connection, epoch: u32) -> Result<()> {
    let Some(closing) = archive::closing_hash(conn, epoch)? else {
        return Ok(());
    };
    let blocks = archive::epoch_blocks(conn, epoch)?;
    let first = blocks.first().context("a closed epoch has blocks")?;
    let predecessor = match epoch {
        0 => Anchor(Fp::ZERO),
        _ => anchor(&first.anchor_before)?,
    };
    // About 16 s for a full epoch; `seal_epoch` holds no transaction meanwhile.
    epochs::seal_epoch(conn, epoch, &blocks, predecessor, &closing)?;
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
