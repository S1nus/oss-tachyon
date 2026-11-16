//! Per-epoch chunk and leaf accumulation (design §2.4, incremental evidence).
//!
//! The builder seeds each block's stamp intakes and folds them into the epoch's
//! open chunk as the block is archived, so sealing only routes what is stored
//! instead of re-reading the epoch. Chunks use the same capacity and order
//! `Epoch::chunks` would produce, which keeps the seal equivalent.
//!
//! Closed chunks are also routed into per-leaf accumulators at a predicted
//! depth (option B). At seal the leaves are used only if their profiles still
//! match `profiles(all members, r0)`; otherwise the seal re-routes the stored
//! chunks. A wrong prediction costs time, never correctness.
//!
//! A reorg invalidates the whole affected epoch: a chunk can straddle the fork
//! point, and replaying a partial chunk would lose or duplicate members. The
//! epoch is marked for an archive-based seal rather than replaying the epoch
//! just for its evidence; later epochs accumulate normally.

use anyhow::{Context, Result, ensure};
use oss_core::{
    ChunkBuilder, Intake, LeafBuilder, Pcd, TachyonBlock, max_polynomial_members, pcd_from_bytes,
    pcd_to_bytes,
};
use rusqlite::{Connection, OptionalExtension, params};
use udon::field::Field;
use zcash_tachyon::{
    Anchor, QrDiscriminant, QrProfile, Tachygram, constants::EPOCH_SIZE, pasta::Fp,
    stamp::proof::qr::QrBucket,
};

use crate::{archive, store};

/// A change to the epoch's leaf accumulators, applied with its chunk rows.
enum LeafUpdate {
    /// The full leaf set after merging the chunks that just closed.
    Replace {
        merged: usize,
        rows: Vec<(u32, u32, Vec<u8>, Vec<u8>)>,
    },
    /// The prediction could not be maintained; seal this epoch from chunks.
    Skip,
}

/// Rows [`prepare`] produced for one block, written with the builder cursor in
/// one transaction so a replay never folds the same block in twice.
pub struct Prepared {
    epoch: u32,
    rows: Vec<(i64, u32, Vec<u8>, Vec<u8>)>,
    leaves: Option<LeafUpdate>,
}

impl Prepared {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.leaves.is_none()
    }

    pub fn write(&self, conn: &Connection) -> Result<()> {
        for (index, end_height, members, pcd) in &self.rows {
            conn.execute(
                "INSERT OR REPLACE INTO epoch_chunks(epoch, chunk, end_height, members, pcd)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![self.epoch, index, end_height, members, pcd],
            )?;
        }
        write_leaves(conn, self.epoch, self.leaves.as_ref())
    }
}

/// Seeds `block`'s stamps and returns the chunks (and leaves) that changed, or
/// `None` when the epoch is marked for an archive seal. Proving happens here,
/// outside any transaction, so the follower never waits on it.
pub fn prepare(
    conn: &Connection,
    cache: &mut Option<ChunkBuilder>,
    block: &TachyonBlock,
) -> Result<Option<Prepared>> {
    if skipped(conn, block.epoch)? {
        *cache = None;
        return Ok(None);
    }
    let capacity = max_polynomial_members();
    // R0 is fixed when the epoch's first block arrives, not at seal, so chunks
    // can be seeded while the epoch is open. It stays unserved until evidence.
    conn.execute(
        "INSERT OR IGNORE INTO epochs(epoch, discriminant) VALUES (?1, ?2)",
        params![block.epoch, QrDiscriminant::random(&mut rand::rng()).0.to_bytes()],
    )?;
    let discriminant = crate::epochs::discriminant(conn, block.epoch)?
        .context("epoch discriminant just inserted")?;
    // Keep the open epoch in memory: loading and decoding its chunks per block
    // would dominate the builder as the epoch grows.
    let mut builder = match cache.take() {
        Some(builder) if builder.epoch() == block.epoch => builder,
        _ => {
            if !started(conn, block.epoch)? {
                if block.epoch_start_anchor.is_none() {
                    // Joined the epoch after its first block (upgrade or first
                    // start mid-epoch): fill the chunks in from the archive once,
                    // including this block, then continue normally.
                    backfill(conn, block.epoch, block.height)?;
                    *cache = None;
                    return Ok(None);
                }
                conn.execute(
                    "INSERT OR IGNORE INTO epoch_chunks_started(epoch) VALUES (?1)",
                    [block.epoch],
                )?;
            }
            match load(conn, block.epoch)? {
                Some(builder) => builder,
                None => ChunkBuilder::new(block.epoch, discriminant, capacity),
            }
        }
    };
    let sizes: Vec<_> = builder
        .chunks()
        .iter()
        .map(|chunk| chunk.members.len())
        .collect();
    builder.push_block(block)?;
    let mut rows = Vec::new();
    for (index, chunk) in builder.chunks().iter().enumerate() {
        if sizes.get(index) == Some(&chunk.members.len()) {
            continue;
        }
        rows.push((
            i64::try_from(index)?,
            block.height,
            members_bytes(&chunk.members),
            pcd_to_bytes(&chunk.proof)?,
        ));
    }
    // A chunk closed when the push started a new one; route it into the leaves.
    let leaves = if builder.chunks().len() > sizes.len() && !leaves_skipped(conn, block.epoch)? {
        match accumulate(conn, block.epoch, builder.chunks()) {
            Ok((leaves, merged)) => Some(LeafUpdate::Replace {
                merged,
                rows: leaf_rows(&leaves)?,
            }),
            Err(error) => {
                eprintln!("leaves: epoch {} falls back to chunks: {error:#}", block.epoch);
                Some(LeafUpdate::Skip)
            }
        }
    } else {
        None
    };
    *cache = Some(builder);
    Ok(Some(Prepared {
        epoch: block.epoch,
        rows,
        leaves,
    }))
}

/// Seeds every archived block of `epoch` up to `through` and persists the
/// chunks and leaves. Used when the builder joins an epoch after its first
/// block; the epoch's earlier blocks are still in the archive.
pub fn backfill(conn: &Connection, epoch: u32, through: u32) -> Result<()> {
    if skipped(conn, epoch)? || load(conn, epoch)?.is_some() {
        return Ok(());
    }
    let activation: u32 = store::meta(conn, "activation_height")?.context("no activation height")?;
    let first = activation.saturating_add(epoch.saturating_mul(EPOCH_SIZE));
    conn.execute(
        "INSERT OR IGNORE INTO epochs(epoch, discriminant) VALUES (?1, ?2)",
        params![epoch, QrDiscriminant::random(&mut rand::rng()).0.to_bytes()],
    )?;
    let discriminant = crate::epochs::discriminant(conn, epoch)?
        .context("epoch discriminant just inserted")?;
    conn.execute(
        "INSERT OR IGNORE INTO epoch_chunks_started(epoch) VALUES (?1)",
        [epoch],
    )?;
    let mut builder = ChunkBuilder::new(epoch, discriminant, max_polynomial_members());
    for height in first..=through {
        let Some(block) = archive::block(conn, height)? else {
            break;
        };
        ensure!(
            block.epoch == epoch,
            "archive block {height} is not in epoch {epoch}"
        );
        builder.push_block(&block)?;
    }
    let leaves = if leaves_skipped(conn, epoch)? {
        None
    } else {
        match accumulate(conn, epoch, builder.chunks()) {
            Ok((leaves, merged)) => Some(LeafUpdate::Replace {
                merged,
                rows: leaf_rows(&leaves)?,
            }),
            Err(error) => {
                eprintln!("leaves: epoch {epoch} falls back to chunks: {error:#}");
                Some(LeafUpdate::Skip)
            }
        }
    };
    let tx = conn.unchecked_transaction()?;
    for (index, chunk) in builder.chunks().iter().enumerate() {
        tx.execute(
            "INSERT OR REPLACE INTO epoch_chunks(epoch, chunk, end_height, members, pcd)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                epoch,
                i64::try_from(index)?,
                through,
                members_bytes(&chunk.members),
                pcd_to_bytes(&chunk.proof)?
            ],
        )?;
    }
    write_leaves(&tx, epoch, leaves.as_ref())?;
    tx.commit()?;
    Ok(())
}

/// The epoch's persisted chunks, ready to continue or to seal. `None` if the
/// epoch predates this table or a reorg invalidated it.
pub fn load(conn: &Connection, epoch: u32) -> Result<Option<ChunkBuilder>> {
    if skipped(conn, epoch)? {
        return Ok(None);
    }
    let Some(discriminant) = crate::epochs::discriminant(conn, epoch)? else {
        return Ok(None);
    };
    if !started(conn, epoch)? {
        return Ok(None);
    }
    let mut stmt = conn
        .prepare("SELECT chunk, members, pcd FROM epoch_chunks WHERE epoch = ?1 ORDER BY chunk")?;
    let mut rows = stmt.query([epoch])?;
    let mut chunks = Vec::new();
    while let Some(row) = rows.next()? {
        ensure!(
            row.get::<_, i64>(0)? == i64::try_from(chunks.len())?,
            "epoch {epoch} chunks are not contiguous"
        );
        chunks.push(Intake {
            proof: pcd_from_bytes(&row.get::<_, Vec<u8>>(2)?)?,
            members: read_members(&row.get::<_, Vec<u8>>(1)?)?,
        });
    }
    Ok(Some(ChunkBuilder::resume(
        epoch,
        discriminant,
        max_polynomial_members(),
        chunks,
    )?))
}

/// Option B seal: routes the still-open chunk into the stored leaves and seals
/// them, provided the leaf set still matches `profiles` for every member.
/// `None` means the prediction was wrong (or leaves never started) and the
/// caller must route the chunks instead.
pub fn seal_leaves(
    conn: &Connection,
    epoch: u32,
    chunks: &[Intake],
    discriminant: QrDiscriminant,
    start: Anchor,
    next: Anchor,
    predecessor: Anchor,
) -> Result<Option<Vec<(QrProfile, Pcd<QrBucket>, usize)>>> {
    if chunks.is_empty() {
        return Ok(None);
    }
    let Some((mut leaves, merged)) = load_leaves(conn, epoch, discriminant)? else {
        return Ok(None);
    };
    if merged != chunks.len() - 1 {
        return Ok(None);
    }
    let Some(open) = chunks.last() else {
        return Ok(None);
    };
    if leaves.merge_chunk(open).is_err() {
        return Ok(None);
    }
    let members: Vec<Tachygram> = chunks
        .iter()
        .flat_map(|chunk| chunk.members.iter().copied())
        .collect();
    let mut want: Vec<(u32, u32)> = oss_core::profiles(members.clone(), discriminant)?
        .iter()
        .map(|path| path_key(path))
        .collect();
    let mut got: Vec<(u32, u32)> = leaves
        .profiles()
        .iter()
        .map(|profile| (profile.depth, profile.bits))
        .collect();
    want.sort_unstable();
    got.sort_unstable();
    if got != want || leaves.members() != members.len() {
        return Ok(None);
    }
    Ok(Some(leaves.seal(start, next, predecessor)?))
}

/// A reorg invalidated `epoch` and any later one: drop their chunks, leaves and
/// marks, and mark the epoch for an archive-based seal.
pub fn invalidate(conn: &Connection, epoch: u32) -> Result<()> {
    conn.execute("DELETE FROM epoch_chunks WHERE epoch >= ?1", [epoch])?;
    conn.execute("DELETE FROM epoch_chunks_started WHERE epoch >= ?1", [epoch])?;
    conn.execute("DELETE FROM epoch_leaves WHERE epoch >= ?1", [epoch])?;
    conn.execute("DELETE FROM epoch_leaves_progress WHERE epoch >= ?1", [epoch])?;
    conn.execute("DELETE FROM epoch_leaves_skip WHERE epoch >= ?1", [epoch])?;
    conn.execute(
        "INSERT OR REPLACE INTO epoch_chunks_skip(epoch) VALUES (?1)",
        [epoch],
    )?;
    Ok(())
}

/// Drops the epoch's chunks, leaves and marks once evidence is written.
pub fn sealed(conn: &Connection, epoch: u32) -> Result<()> {
    conn.execute("DELETE FROM epoch_chunks WHERE epoch = ?1", [epoch])?;
    conn.execute("DELETE FROM epoch_chunks_started WHERE epoch = ?1", [epoch])?;
    conn.execute("DELETE FROM epoch_chunks_skip WHERE epoch = ?1", [epoch])?;
    clear_leaves(conn, epoch)
}

/// The epoch's archived member count, for the fallback seal.
pub fn member_count(conn: &Connection, epoch: u32) -> Result<u64> {
    let activation: u32 = store::meta(conn, "activation_height")?.context("no activation height")?;
    let start = activation.saturating_add(epoch.saturating_mul(EPOCH_SIZE));
    let end = start.saturating_add(EPOCH_SIZE);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tachygrams WHERE height >= ?1 AND height < ?2",
        params![start, end],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count)?)
}

fn clear_leaves(conn: &Connection, epoch: u32) -> Result<()> {
    conn.execute("DELETE FROM epoch_leaves WHERE epoch = ?1", [epoch])?;
    conn.execute("DELETE FROM epoch_leaves_progress WHERE epoch = ?1", [epoch])?;
    conn.execute("DELETE FROM epoch_leaves_skip WHERE epoch = ?1", [epoch])?;
    Ok(())
}

/// Merges every closed chunk that the stored leaves have not seen yet.
fn accumulate(
    conn: &Connection,
    epoch: u32,
    chunks: &[Intake],
) -> Result<(LeafBuilder, usize)> {
    let discriminant = crate::epochs::discriminant(conn, epoch)?.context("missing discriminant")?;
    let closed = chunks.len().saturating_sub(1);
    let (mut leaves, merged) = match load_leaves(conn, epoch, discriminant)? {
        Some((leaves, merged)) => (leaves, merged),
        None => (
            LeafBuilder::new(epoch, predicted_depth(conn, epoch)?, discriminant),
            0,
        ),
    };
    ensure!(
        merged <= closed,
        "epoch {epoch} leaf progress is ahead of its chunks"
    );
    for chunk in &chunks[merged..closed] {
        leaves.merge_chunk(chunk)?;
    }
    Ok((leaves, closed))
}

fn load_leaves(
    conn: &Connection,
    epoch: u32,
    discriminant: QrDiscriminant,
) -> Result<Option<(LeafBuilder, usize)>> {
    let merged: Option<i64> = conn
        .query_row(
            "SELECT merged FROM epoch_leaves_progress WHERE epoch = ?1",
            [epoch],
            |row| row.get(0),
        )
        .optional()?;
    let Some(merged) = merged else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT depth, bits, members, pcd FROM epoch_leaves WHERE epoch = ?1 ORDER BY depth, bits",
    )?;
    let mut rows = stmt.query([epoch])?;
    let mut entries = Vec::new();
    let mut depth: Option<u32> = None;
    while let Some(row) = rows.next()? {
        let leaf_depth: u32 = row.get(0)?;
        if let Some(previous) = depth {
            ensure!(
                previous == leaf_depth,
                "epoch {epoch} leaves have mixed depths"
            );
        }
        depth = Some(leaf_depth);
        entries.push((
            QrProfile {
                depth: leaf_depth,
                bits: row.get(1)?,
            },
            Intake {
                proof: pcd_from_bytes(&row.get::<_, Vec<u8>>(3)?)?,
                members: read_members(&row.get::<_, Vec<u8>>(2)?)?,
            },
        ));
    }
    let depth = match depth {
        Some(depth) => depth,
        None => predicted_depth(conn, epoch)?,
    };
    Ok(Some((
        LeafBuilder::resume(epoch, depth, discriminant, entries)?,
        usize::try_from(merged)?,
    )))
}

fn leaf_rows(leaves: &LeafBuilder) -> Result<Vec<(u32, u32, Vec<u8>, Vec<u8>)>> {
    leaves
        .entries()
        .into_iter()
        .map(|(profile, intake)| {
            Ok((
                profile.depth,
                profile.bits,
                members_bytes(&intake.members),
                pcd_to_bytes(&intake.proof)?,
            ))
        })
        .collect()
}

fn write_leaves(conn: &Connection, epoch: u32, update: Option<&LeafUpdate>) -> Result<()> {
    match update {
        None => Ok(()),
        Some(LeafUpdate::Replace { merged, rows }) => {
            conn.execute("DELETE FROM epoch_leaves WHERE epoch = ?1", [epoch])?;
            for (depth, bits, members, pcd) in rows {
                conn.execute(
                    "INSERT OR REPLACE INTO epoch_leaves(epoch, depth, bits, members, pcd)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![epoch, depth, bits, members, pcd],
                )?;
            }
            conn.execute(
                "INSERT OR REPLACE INTO epoch_leaves_progress(epoch, merged) VALUES (?1, ?2)",
                params![epoch, i64::try_from(*merged)?],
            )?;
            conn.execute("DELETE FROM epoch_leaves_skip WHERE epoch = ?1", [epoch])?;
            Ok(())
        }
        Some(LeafUpdate::Skip) => {
            clear_leaves(conn, epoch)?;
            conn.execute(
                "INSERT OR REPLACE INTO epoch_leaves_skip(epoch) VALUES (?1)",
                [epoch],
            )?;
            Ok(())
        }
    }
}

/// The depth a leaf accumulator starts at: the smallest tree that holds the
/// last sealed epoch's members. A later epoch that outgrows it falls back.
fn predicted_depth(conn: &Connection, epoch: u32) -> Result<u32> {
    let members = match store::meta::<u64>(conn, "last_sealed_members")? {
        Some(members) => members,
        None if epoch > 0 => member_count(conn, epoch - 1)?,
        None => 0,
    };
    let capacity = u64::try_from(max_polynomial_members())?;
    let mut depth = 0u32;
    while depth < QrProfile::MAX_DEPTH as u32
        && (u128::from(1u64) << depth) * u128::from(capacity) < u128::from(members)
    {
        depth += 1;
    }
    Ok(depth)
}

fn path_key(path: &[bool]) -> (u32, u32) {
    let profile = path.iter().fold(QrProfile::ROOT, |profile, &side| {
        profile.descend(side)
    });
    (profile.depth, profile.bits)
}

fn started(conn: &Connection, epoch: u32) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM epoch_chunks_started WHERE epoch = ?1",
            [epoch],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn skipped(conn: &Connection, epoch: u32) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM epoch_chunks_skip WHERE epoch = ?1",
            [epoch],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn leaves_skipped(conn: &Connection, epoch: u32) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM epoch_leaves_skip WHERE epoch = ?1",
            [epoch],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn members_bytes(members: &[Tachygram]) -> Vec<u8> {
    members
        .iter()
        .flat_map(|member| {
            let value: Fp = (*member).into();
            value.to_bytes()
        })
        .collect()
}

fn read_members(bytes: &[u8]) -> Result<Vec<Tachygram>> {
    ensure!(
        bytes.len().is_multiple_of(32),
        "stored intake members are not 32-byte values"
    );
    bytes
        .chunks_exact(32)
        .map(|chunk| {
            let value: [u8; 32] = chunk.try_into().expect("exact chunk");
            Fp::from_bytes(value)
                .map(Tachygram::from)
                .context("stored intake member is not canonical")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use oss_core::build_epoch;
    use oss_core::evidence::testing::*;
    use oss_core::verify_pcd;

    use super::*;

    #[test]
    #[ignore = "slow: 9k-member epoch; run in release with --ignored"]
    fn backfilled_leaves_match_a_one_shot_seal() -> Result<()> {
        let mut blocks = vec![genesis()];
        mine_to_epoch(&mut blocks, 1)?;
        populate_epoch(&mut blocks, 9_000)?;
        mine_to_epoch(&mut blocks, 2)?;
        let (inputs, predecessor) = epoch_inputs(&blocks, 1)?;
        let closing = inputs
            .iter()
            .filter(|block| block.epoch == 1)
            .last()
            .expect("epoch 1 closes")
            .height;

        let path = std::env::temp_dir().join(format!("oss-leaves-{}.sqlite", std::process::id()));
        let conn = store::open(&path)?;
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('activation_height', '8')",
            [],
        )?;
        // A steady-state prediction: this epoch should need depth 1.
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('last_sealed_members', '9000')",
            [],
        )?;
        for block in &blocks {
            conn.execute(
                "INSERT OR REPLACE INTO blocks(height, hash, prev_hash, epoch, json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    block.height,
                    block.hash,
                    block.previous_block_hash,
                    block.epoch,
                    serde_json::to_string(block)?
                ],
            )?;
        }
        backfill(&conn, 1, closing)?;
        let chunks = load(&conn, 1)?.context("chunks")?;
        let r0 = crate::epochs::discriminant(&conn, 1)?.context("r0")?;
        let start = inputs
            .iter()
            .find(|block| block.epoch == 1)
            .expect("epoch 1 blocks")
            .entry()?;
        let next = inputs
            .iter()
            .find(|block| block.epoch == 2)
            .expect("epoch 2 block")
            .entry()?;

        let sealed = seal_leaves(&conn, 1, chunks.chunks(), r0, start, next, predecessor)?
            .context("leaf seal")?;
        let batch = build_epoch(&inputs, predecessor, r0)?;
        let mut got: Vec<_> = sealed.iter().map(|bucket| (bucket.0, bucket.2)).collect();
        let mut want: Vec<_> = batch.iter().map(|bucket| (bucket.0, bucket.2)).collect();
        got.sort_by_key(|(profile, _)| (profile.depth, profile.bits));
        want.sort_by_key(|(profile, _)| (profile.depth, profile.bits));
        assert_eq!(got, want);
        for (_, bucket, _) in &sealed {
            assert!(verify_pcd(bucket)?);
        }

        drop(conn);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        Ok(())
    }
}
