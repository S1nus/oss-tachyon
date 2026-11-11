//! Anchor-span chunks of the current and previous epoch (design §3, §2.4).
//! The builder thread is the only writer: it `connect`s every canonical block
//! in height order and `rewind`s on a reorg.
//!
//! Besides its chunks (`chunk` 0, 1, …), each epoch has a start row
//! (`chunk = -1`, empty `members` and `pcd`). It holds the anchor the epoch's
//! spans start at, so an epoch with no stamps yet still answers its entry
//! anchor, and its `end_height` is the first block connected in the epoch.

use anyhow::{Context, Result, ensure};
use oss_core::{
    Pcd, SpanBuilder, SpanChunk, Spans, TachyonBlock, anchor, anchor_hex, max_polynomial_members,
    pcd_from_bytes, pcd_to_bytes,
};
use rusqlite::{Connection, OptionalExtension, params};
use zcash_tachyon::{Anchor, stamp::proof::pool::AnchorSpan};

/// The `chunk` of an epoch's start row.
const START: i64 = -1;

/// Pushes the block's stamps into its epoch's chunks, starting the epoch at
/// the block's entry anchor if it is the first block seen in it, and drops
/// epochs older than the previous one. Connecting a block again is a no-op.
pub fn connect(conn: &Connection, block: &TachyonBlock) -> Result<()> {
    connect_with(conn, block, max_polynomial_members())
}

fn connect_with(conn: &Connection, block: &TachyonBlock, capacity: usize) -> Result<()> {
    let after = anchor(&block.anchor_after)?;
    let target = target(conn, block.epoch)?;
    if block.stamps.is_empty()
        && let Some(target) = target
    {
        ensure!(
            target == after,
            "block {} does not continue epoch {}'s spans",
            block.height,
            block.epoch
        );
        return Ok(());
    }
    let mut builder = match target {
        Some(_) => load(conn, block.epoch, capacity)?.context("epoch spans vanished")?,
        None => SpanBuilder::new(block.epoch, block.entry()?, capacity),
    };
    let sizes: Vec<_> = builder.chunks().iter().map(|c| c.members.len()).collect();
    // Prove outside any transaction: the follower must not wait on the lock.
    builder.push_block(block)?;
    ensure!(
        builder.target() == after,
        "block {} does not continue epoch {}'s spans at {}",
        block.height,
        block.epoch,
        anchor_hex(builder.target())
    );
    let tx = conn.unchecked_transaction()?;
    if target.is_none() {
        tx.execute("DELETE FROM spans WHERE epoch + 1 < ?1", [block.epoch])?;
        tx.execute(
            "INSERT INTO spans(epoch, chunk, start_anchor, end_height, members, pcd)
             VALUES (?1, ?2, ?3, ?4, x'', x'')",
            params![block.epoch, START, bytes(builder.start()), block.height],
        )?;
    }
    for (i, chunk) in builder.chunks().iter().enumerate() {
        if sizes.get(i) == Some(&chunk.members.len()) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO spans(epoch, chunk, start_anchor, end_height, members, pcd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                block.epoch,
                i64::try_from(i)?,
                bytes(chunk.start),
                block.height,
                chunk
                    .members
                    .iter()
                    .flat_map(|a| bytes(*a))
                    .collect::<Vec<_>>(),
                pcd_to_bytes(&chunk.pcd)?
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Drops every row a reorg at `height` invalidates and returns the first
/// height to `connect` again: `height` if nothing was dropped or nothing
/// survives, else the end height of the last surviving row. That block may
/// hold stamps of a dropped chunk; `connect` skips the ones still kept.
///
/// Opens no transaction: run it in the caller's, so the caller's cursor moves
/// in the same commit. Run apart, a crash between them replays too little.
pub fn rewind(conn: &Connection, height: u32) -> Result<u32> {
    let dropped = conn.execute("DELETE FROM spans WHERE end_height >= ?1", [height])?;
    let last: Option<u32> =
        conn.query_row("SELECT MAX(end_height) FROM spans", [], |r| r.get(0))?;
    Ok(last.filter(|_| dropped > 0).unwrap_or(height))
}

/// The epoch and [`Spans`] for `from`, from one epoch read in one
/// transaction; see [`SpanBuilder::spans_from`]. `None` if some anchor is not
/// one the span builder has reached, or the anchors are in different epochs.
pub fn spans_from(conn: &Connection, from: &[Anchor]) -> Result<Option<(u32, Spans)>> {
    let tx = conn.unchecked_transaction()?;
    let mut stmt = tx.prepare("SELECT DISTINCT epoch FROM spans ORDER BY epoch DESC")?;
    let epochs = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<u32>>>()?;
    for epoch in epochs {
        let builder =
            load(&tx, epoch, max_polynomial_members())?.context("epoch spans vanished")?;
        if let Some(spans) = builder.spans_from(from) {
            return Ok(Some((epoch, spans)));
        }
    }
    Ok(None)
}

/// The anchor the epoch's spans end at, or `None` if the epoch is not started.
fn target(conn: &Connection, epoch: u32) -> Result<Option<Anchor>> {
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT start_anchor, members FROM spans WHERE epoch = ?1 ORDER BY chunk DESC LIMIT 1",
            [epoch],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(start, members)| match members.len() {
        0 => read_anchor(&start),
        n => read_anchor(&members[n - 32..]),
    })
    .transpose()
}

fn load(conn: &Connection, epoch: u32, capacity: usize) -> Result<Option<SpanBuilder>> {
    let mut stmt = conn.prepare(
        "SELECT chunk, start_anchor, members, pcd FROM spans WHERE epoch = ?1 ORDER BY chunk",
    )?;
    let mut rows = stmt.query([epoch])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    ensure!(
        row.get::<_, i64>(0)? == START,
        "epoch {epoch} spans have no start row"
    );
    let epoch_start = read_anchor(&row.get::<_, Vec<u8>>(1)?)?;
    let mut chunks: Vec<SpanChunk> = Vec::new();
    while let Some(row) = rows.next()? {
        let start = read_anchor(&row.get::<_, Vec<u8>>(1)?)?;
        let members = row
            .get::<_, Vec<u8>>(2)?
            .chunks(32)
            .map(read_anchor)
            .collect::<Result<Vec<_>>>()?;
        let pcd: Pcd<AnchorSpan> = pcd_from_bytes(&row.get::<_, Vec<u8>>(3)?)?;
        ensure!(
            row.get::<_, i64>(0)? == i64::try_from(chunks.len())?
                && start == chunks.last().map_or(epoch_start, SpanChunk::end)
                && Some(&pcd.data().1) == members.last()
                && pcd.data().0 == start,
            "epoch {epoch} span chunks are not contiguous"
        );
        chunks.push(SpanChunk {
            start,
            members,
            pcd,
        });
    }
    Ok(Some(SpanBuilder::resume(
        epoch,
        epoch_start,
        capacity,
        chunks,
    )))
}

fn bytes(anchor: Anchor) -> [u8; 32] {
    let mut bytes = [0; 32];
    anchor
        .write(&mut bytes[..])
        .expect("fixed-size anchor fits its encoding");
    bytes
}

fn read_anchor(bytes: &[u8]) -> Result<Anchor> {
    ensure!(bytes.len() == 32, "stored anchor is not 32 bytes");
    Ok(Anchor::read(bytes)?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use oss_core::{evidence::testing::*, verify_pcd};

    use super::*;
    use crate::store;

    struct Db(PathBuf, Connection);

    impl Db {
        fn open(name: &str) -> Result<Self> {
            let path = std::env::temp_dir()
                .join(format!("oss-spans-{name}-{}.sqlite", std::process::id()));
            let conn = store::open(&path)?;
            Ok(Self(path, conn))
        }

        /// `(chunk, end_height)` of every row, by epoch.
        fn rows(&self) -> Result<Vec<(u32, i64, u32)>> {
            let mut stmt = self
                .1
                .prepare("SELECT epoch, chunk, end_height FROM spans ORDER BY epoch, chunk")?;
            Ok(stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?)
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    /// `(epoch, target, chunk count, start)`.
    type Shape = (u32, Anchor, usize, Vec<Option<usize>>);

    /// [`spans_from`]'s shape, after checking that every chunk verifies and
    /// that they chain to `target`.
    fn read(conn: &Connection, from: &[Anchor]) -> Result<Option<Shape>> {
        let Some((epoch, (target, chunks, start))) = spans_from(conn, from)? else {
            return Ok(None);
        };
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].data().1, pair[1].data().0);
        }
        if let Some(last) = chunks.last() {
            assert_eq!(last.data().1, target);
        }
        for chunk in &chunks {
            assert!(verify_pcd(chunk)?);
        }
        Ok(Some((epoch, target, chunks.len(), start)))
    }

    #[test]
    fn connect_rewind_and_rebuild_a_split_block() -> Result<()> {
        let db = Db::open("rewind")?;
        let conn = &db.1;
        let mut blocks = vec![genesis()]; // height 8
        populate_epoch(&mut blocks, 1)?; // 9: one stamp
        populate_epoch(&mut blocks, 65)?; // 10: two stamps
        mine(&mut blocks)?; // 11: none
        populate_epoch(&mut blocks, 1)?; // 12: one stamp
        let entry = blocks[0].entry()?;
        let tip = anchor(&blocks[4].anchor_after)?;
        let mid = anchor(&blocks[2].anchor_after)?;

        connect_with(conn, &blocks[0], 2)?;
        assert_eq!(read(conn, &[entry])?, Some((0, entry, 0, vec![None])));
        for block in &blocks[1..] {
            connect_with(conn, block, 2)?;
        }
        // Capacity 2 splits block 10 across chunks 0 and 1.
        assert_eq!(db.rows()?, [(0, -1, 8), (0, 0, 10), (0, 1, 12)]);
        assert_eq!(read(conn, &[entry])?, Some((0, tip, 2, vec![Some(0)])));
        assert_eq!(
            read(conn, &[mid, tip])?,
            Some((0, tip, 1, vec![Some(0), None]))
        );

        // Fed again: a no-op. Fed out of order: an error.
        connect_with(conn, &blocks[4], 2)?;
        assert!(connect_with(conn, &blocks[2], 2).is_err());
        assert_eq!(rewind(conn, 13)?, 13, "nothing to drop");
        assert_eq!(db.rows()?, [(0, -1, 8), (0, 0, 10), (0, 1, 12)]);

        // A reorg at 12 drops chunk 1, which also held block 10's second stamp.
        assert_eq!(rewind(conn, 12)?, 10);
        assert_eq!(db.rows()?, [(0, -1, 8), (0, 0, 10)]);
        let mut fork = blocks[..4].to_vec();
        populate_epoch(&mut fork, 3)?;
        let new_tip = anchor(&fork[4].anchor_after)?;
        assert_ne!(new_tip, tip);
        for block in &fork[2..] {
            connect_with(conn, block, 2)?;
        }
        assert_eq!(db.rows()?, [(0, -1, 8), (0, 0, 10), (0, 1, 12)]);
        assert_eq!(read(conn, &[tip])?, None);
        assert_eq!(
            read(conn, &[entry, mid])?,
            Some((0, new_tip, 2, vec![Some(0), Some(1)]))
        );

        // Below every row: nothing survives, replay from the reorg height.
        assert_eq!(rewind(conn, 8)?, 8);
        assert!(db.rows()?.is_empty());
        assert_eq!(read(conn, &[entry])?, None);
        Ok(())
    }

    #[test]
    fn keeps_two_epochs_and_rewinds_across_their_boundary() -> Result<()> {
        let db = Db::open("epochs")?;
        let conn = &db.1;
        let mut blocks = vec![genesis()];
        populate_epoch(&mut blocks, 1)?;
        mine_to_epoch(&mut blocks, 1)?;
        let first1 = blocks.len() - 1;
        populate_epoch(&mut blocks, 1)?;
        mine_to_epoch(&mut blocks, 2)?;
        let first2 = blocks.len() - 1;
        let entry = |i: usize| blocks[i].entry();
        let after = |i: usize| anchor(&blocks[i].anchor_after);

        for block in &blocks[..=first1] {
            connect(conn, block)?;
        }
        // Epoch 1 has no stamp yet, so its entry is its target.
        assert_eq!(
            read(conn, &[entry(first1)?])?,
            Some((1, entry(first1)?, 0, vec![None]))
        );
        assert_eq!(
            read(conn, &[entry(0)?])?,
            Some((0, after(1)?, 1, vec![Some(0)]))
        );

        for block in &blocks[first1 + 1..] {
            connect(conn, block)?;
        }
        let tip1 = after(first1 + 1)?;
        assert_eq!(read(conn, &[entry(0)?])?, None, "epoch 0 is pruned");
        assert_eq!(
            read(conn, &[entry(first1)?])?,
            Some((1, tip1, 1, vec![Some(0)]))
        );
        assert_eq!(
            read(conn, &[entry(first2)?])?,
            Some((2, entry(first2)?, 0, vec![None]))
        );
        assert_eq!(read(conn, &[entry(first1)?, entry(first2)?])?, None);
        let rows = db.rows()?;

        // A reorg of epoch 2's first block replays from epoch 1's last stamp.
        let height = blocks[first2].height;
        let replay = rewind(conn, height)?;
        assert_eq!(replay, blocks[first1 + 1].height);
        assert_eq!(read(conn, &[entry(first2)?])?, None);
        for block in blocks.iter().filter(|b| b.height >= replay) {
            connect(conn, block)?;
        }
        assert_eq!(db.rows()?, rows);
        assert_eq!(
            read(conn, &[entry(first2)?])?,
            Some((2, entry(first2)?, 0, vec![None]))
        );
        Ok(())
    }
}
