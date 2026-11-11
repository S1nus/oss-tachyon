//! Sealed QR evidence per closed epoch. Rows are keyed by the epoch's closing
//! hash and read only under the canonical one, so a reorg of the closing block
//! hides stale evidence without any cleanup.

use anyhow::{Context, Result, anyhow, ensure};
use oss_core::{BucketProfile, Pcd, TachyonBlock, build_epoch, pcd_from_bytes, pcd_to_bytes};
use rusqlite::{Connection, OptionalExtension, params};
use udon::field::Field;
use zcash_tachyon::{Anchor, QrDiscriminant, QrProfile, pasta::Fp, stamp::proof::qr::QrBucket};

/// Seals every bucket of `epoch` under `closing_hash`, the hash of its last
/// block. `blocks` and `predecessor` are what [`build_epoch`] takes. A no-op if
/// that evidence already exists.
pub fn seal_epoch(
    conn: &Connection,
    epoch: u32,
    blocks: &[TachyonBlock],
    predecessor: Anchor,
    closing_hash: &str,
) -> Result<()> {
    ensure!(
        blocks.first().is_some_and(|b| b.epoch == epoch)
            && blocks
                .iter()
                .rfind(|b| b.epoch == epoch)
                .is_some_and(|b| b.hash == closing_hash),
        "blocks do not close epoch {epoch} at {closing_hash}"
    );
    if evidence(conn, epoch, closing_hash)?.is_some() {
        return Ok(());
    }
    // R0 is fixed per epoch on first use and survives reorgs and restarts.
    conn.execute(
        "INSERT OR IGNORE INTO epochs(epoch, discriminant) VALUES (?1, ?2)",
        params![epoch, QrDiscriminant::random(&mut rand::rng()).0.to_bytes()],
    )?;
    let r0 = discriminant(conn, epoch)?.context("missing discriminant")?;
    let buckets = build_epoch(blocks, predecessor, r0)?;
    let mut profiles = Vec::with_capacity(buckets.len());
    let tx = conn.unchecked_transaction()?;
    for (profile, pcd, members) in &buckets {
        tx.execute(
            "INSERT OR REPLACE INTO buckets(epoch, closing_hash, depth, bits, members, pcd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                epoch,
                closing_hash,
                profile.depth,
                profile.bits,
                members,
                pcd_to_bytes(pcd)?
            ],
        )?;
        profiles.push(BucketProfile {
            profile: *profile,
            members: u32::try_from(*members)?,
        });
    }
    // Written last, in the same transaction: its presence means every bucket is.
    tx.execute(
        "INSERT OR REPLACE INTO evidence(epoch, closing_hash, profiles) VALUES (?1, ?2, ?3)",
        params![epoch, closing_hash, serde_json::to_string(&profiles)?],
    )?;
    tx.commit()?;
    Ok(())
}

/// The epoch's sealed bucket profiles under `closing_hash`, or `None` if it has
/// none (still open, building, or sealed only under a reorged-out block).
pub fn evidence(
    conn: &Connection,
    epoch: u32,
    closing_hash: &str,
) -> Result<Option<Vec<BucketProfile>>> {
    conn.query_row(
        "SELECT profiles FROM evidence WHERE epoch = ?1 AND closing_hash = ?2",
        params![epoch, closing_hash],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|json| Ok(serde_json::from_str(&json)?))
    .transpose()
}

/// The sealed bucket of `profile` under `closing_hash`. Decoded only: callers
/// that hand it across a process boundary still verify it.
pub fn bucket(
    conn: &Connection,
    epoch: u32,
    closing_hash: &str,
    profile: QrProfile,
) -> Result<Option<Pcd<QrBucket>>> {
    conn.query_row(
        "SELECT pcd FROM buckets WHERE epoch = ?1 AND closing_hash = ?2 AND depth = ?3 AND bits = ?4",
        params![epoch, closing_hash, profile.depth, profile.bits],
        |row| row.get::<_, Vec<u8>>(0),
    )
    .optional()?
    .map(|bytes| pcd_from_bytes(&bytes))
    .transpose()
}

/// The epoch's private routing discriminant `R0`. Publish it only once
/// [`evidence`] is ready for the canonical closing hash.
pub fn discriminant(conn: &Connection, epoch: u32) -> Result<Option<QrDiscriminant>> {
    conn.query_row(
        "SELECT discriminant FROM epochs WHERE epoch = ?1",
        [epoch],
        |row| row.get::<_, [u8; 32]>(0),
    )
    .optional()?
    .map(|bytes| {
        Fp::from_bytes(bytes)
            .map(QrDiscriminant)
            .ok_or_else(|| anyhow!("epoch {epoch} has a non-canonical discriminant"))
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use oss_core::{evidence::testing::*, verify_pcd};

    use super::*;
    use crate::store;

    #[test]
    fn reorg_of_the_closing_block_hides_stale_rows() -> Result<()> {
        let path = std::env::temp_dir().join(format!("oss-epochs-{}.sqlite", std::process::id()));
        let conn = store::open(&path)?;
        let mut blocks = vec![genesis()];
        mine_to_epoch(&mut blocks, 1)?;
        let (mut inputs, predecessor) = epoch_inputs(&blocks, 0)?;
        let closing = inputs[inputs.len() - 2].hash.clone();

        assert!(discriminant(&conn, 0)?.is_none());
        assert!(seal_epoch(&conn, 0, &inputs, predecessor, "00").is_err());
        assert!(seal_epoch(&conn, 1, &inputs, predecessor, &closing).is_err());
        seal_epoch(&conn, 0, &inputs, predecessor, &closing)?;
        let r0 = discriminant(&conn, 0)?.context("sealed epoch has R0")?;
        let root = BucketProfile {
            profile: QrProfile::ROOT,
            members: 0,
        };
        assert_eq!(evidence(&conn, 0, &closing)?, Some(vec![root]));
        let pcd = bucket(&conn, 0, &closing, QrProfile::ROOT)?.context("sealed bucket")?;
        assert_eq!(pcd.data().1, inputs[0].entry()?);
        assert_eq!(pcd.data().2, inputs[inputs.len() - 1].entry()?);
        assert_eq!(pcd.data().3, r0);
        assert!(verify_pcd(&pcd)?);
        assert!(bucket(&conn, 0, &closing, QrProfile::ROOT.descend(true))?.is_none());

        // Idempotent: no new rows and the same R0.
        seal_epoch(&conn, 0, &inputs, predecessor, &closing)?;
        let rows = |conn: &Connection| -> Result<u32> {
            Ok(conn.query_row("SELECT COUNT(*) FROM buckets", [], |r| r.get(0))?)
        };
        assert_eq!(rows(&conn)?, 1);

        // A new closing block hides the old rows until it is sealed, under the same R0.
        let n = inputs.len();
        let reorged = "ab".repeat(32);
        inputs[n - 2].hash.clone_from(&reorged);
        inputs[n - 1].previous_block_hash.clone_from(&reorged);
        assert!(evidence(&conn, 0, &reorged)?.is_none());
        assert!(bucket(&conn, 0, &reorged, QrProfile::ROOT)?.is_none());
        seal_epoch(&conn, 0, &inputs, predecessor, &reorged)?;
        assert_eq!(evidence(&conn, 0, &reorged)?, Some(vec![root]));
        assert_eq!(
            bucket(&conn, 0, &reorged, QrProfile::ROOT)?
                .context("resealed bucket")?
                .data()
                .3,
            r0
        );
        assert_eq!(discriminant(&conn, 0)?, Some(r0));
        assert_eq!(rows(&conn)?, 2);

        drop(conn);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        Ok(())
    }
}
