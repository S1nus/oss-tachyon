//! The builder (design §2.4, §3): two threads, each bringing its part of the
//! store up to the archive from its own cursor. The follower's tip `watch` only
//! wakes them, so a missed wake-up costs nothing.
//!
//! - The span thread ([`catch_up`]) applies rewinds and connects every block's
//!   stamps to the anchor spans.
//! - The sealer ([`seal_closed`]) seals each closed epoch's QR buckets. An
//!   epoch close takes seconds to minutes, so it runs apart: spans for the new
//!   epoch's blocks keep being served meanwhile.
//!
//! `meta` keys written here:
//! - `builder_height`: the last archived height whose spans are built.
//! - `evidence_ready_through`: the newest epoch sealed, with every older one.
//!   The sealer raises it one epoch at a time; a rewind lowers it below the
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

/// One of the builder's passes: [`catch_up`] or [`seal_closed`].
pub type Pass = fn(&Connection, &dyn Fn() -> bool) -> Result<()>;

/// Runs `pass` on every wake-up until the follower drops its tip channel.
/// Errors (e.g. a reorg racing a read) are logged and retried.
pub fn run(
    name: &str,
    pass: Pass,
    conn: &Connection,
    mut tip: watch::Receiver<u32>,
    runtime: &Handle,
) {
    loop {
        tip.mark_unchanged();
        if let Err(e) = pass(conn, &|| tip.has_changed().is_err()) {
            eprintln!("{name}: {e:#}");
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

/// Builds the spans of every archived height above the cursor, in order,
/// applying any pending rewind before each. Returns at the archive tip, or
/// once `stop()`. Sealing is [`seal_closed`]'s.
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

/// Seals every closed epoch above `evidence_ready_through`, oldest first, and
/// marks each ready. Starts at the archive's first epoch if none is ready.
/// Returns once the next epoch is still open, once `stop()`, or when a reorg
/// moved the chain under a seal (the next pass retries).
pub fn seal_closed(conn: &Connection, stop: &dyn Fn() -> bool) -> Result<()> {
    while !stop() {
        let ready = store::meta::<u32>(conn, READY_THROUGH)?;
        let epoch = match ready {
            Some(ready) => ready + 1,
            None => match archive::first_epoch(conn)? {
                Some(first) => first,
                None => return Ok(()),
            },
        };
        let Some(closing) = archive::closing_hash(conn, epoch)? else {
            return Ok(());
        };
        seal(conn, epoch, &closing)?;
        if !mark_ready(conn, epoch, &closing, ready)? {
            return Ok(());
        }
    }
    Ok(())
}

/// Sets `evidence_ready_through` to `epoch` if `closing` is still its
/// canonical closing hash, the cursor is still `ready` (the value the sealer
/// read), and `epoch` is the one after it (or the archive's first epoch). One
/// statement, so a reorg or rewind that lands while the epoch seals cannot be
/// overwritten: the sealer rereads and retries instead.
pub(crate) fn mark_ready(
    conn: &Connection,
    epoch: u32,
    closing: &str,
    ready: Option<u32>,
) -> Result<bool> {
    let changed = conn.execute(
        "INSERT INTO meta(key, value) SELECT ?1, ?2
         WHERE (SELECT hash FROM blocks WHERE epoch = ?2
                  AND height = (SELECT MIN(height) FROM blocks WHERE epoch = ?2 + 1) - 1) = ?3
           AND (SELECT CAST(value AS INTEGER) FROM meta WHERE key = ?1) IS ?4
           AND ?2 = COALESCE(?4 + 1, (SELECT epoch FROM blocks ORDER BY height LIMIT 1))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![READY_THROUGH, epoch, closing, ready],
    )?;
    Ok(changed == 1)
}

/// Seals `epoch` under `closing`, its closing hash.
fn seal(conn: &Connection, epoch: u32, closing: &str) -> Result<()> {
    let blocks = archive::epoch_blocks(conn, epoch)?;
    let first = blocks.first().context("a closed epoch has blocks")?;
    let predecessor = match epoch {
        0 => Anchor(Fp::ZERO),
        _ => anchor(&first.anchor_before)?,
    };
    // About 16 s for a full epoch; `seal_epoch` holds no transaction meanwhile.
    epochs::seal_epoch(conn, epoch, &blocks, predecessor, closing)
}

fn set(conn: &Connection, key: &str, value: u32) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}
