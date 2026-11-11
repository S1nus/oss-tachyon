//! Reads of the canonical Tachyon archive that the follower writes. Any
//! connection works, including a read-only one.
//!
//! `blocks.json` is the verbatim `gettachyonblock` text. `tachygrams.stamp` is
//! the index into that block's `stamps`.

use anyhow::Result;
use oss_core::TachyonBlock;
use rusqlite::{Connection, OptionalExtension, params};
use udon::field::Field;
use zcash_tachyon::{Anchor, pasta::Fp};

fn parse(json: Option<String>) -> Result<Option<TachyonBlock>> {
    Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
}

/// The newest archived block.
pub fn tip(conn: &Connection) -> Result<Option<TachyonBlock>> {
    parse(
        conn.query_row(
            "SELECT json FROM blocks ORDER BY height DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?,
    )
}

/// The oldest archived height: the activation height or the checkpoint.
pub fn first_height(conn: &Connection) -> Result<Option<u32>> {
    Ok(conn.query_row("SELECT MIN(height) FROM blocks", [], |r| r.get(0))?)
}

pub fn block(conn: &Connection, height: u32) -> Result<Option<TachyonBlock>> {
    parse(block_json(conn, height)?)
}

/// The stored `gettachyonblock` text, for `oss_block`.
pub fn block_json(conn: &Connection, height: u32) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT json FROM blocks WHERE height = ?1", [height], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn hash(conn: &Connection, height: u32) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT hash FROM blocks WHERE height = ?1", [height], |r| {
            r.get(0)
        })
        .optional()?)
}

/// The epoch's blocks plus the next epoch's first block, if archived: what
/// `build_epoch` takes. One statement, so one consistent snapshot.
pub fn epoch_blocks(conn: &Connection, epoch: u32) -> Result<Vec<TachyonBlock>> {
    let mut stmt = conn.prepare(
        "SELECT json FROM blocks WHERE epoch = ?1
           OR height = (SELECT MAX(height) FROM blocks WHERE epoch = ?1) + 1
         ORDER BY height",
    )?;
    let rows = stmt.query_map([epoch], |r| r.get::<_, String>(0))?;
    rows.map(|json| Ok(serde_json::from_str(&json?)?)).collect()
}

/// The hash of the epoch's last block, once the next epoch's first block is
/// archived (the epoch has closed).
pub fn closing_hash(conn: &Connection, epoch: u32) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT hash FROM blocks WHERE epoch = ?1
               AND height = (SELECT MIN(height) FROM blocks WHERE epoch = ?1 + 1) - 1",
            [epoch],
            |r| r.get(0),
        )
        .optional()?)
}

/// `entry(epoch)`: the anchor after the epoch crossing, from its first block.
pub fn entry_anchor(conn: &Connection, epoch: u32) -> Result<Option<Anchor>> {
    let first = parse(
        conn.query_row(
            "SELECT json FROM blocks WHERE epoch = ?1 ORDER BY height LIMIT 1",
            [epoch],
            |r| r.get(0),
        )
        .optional()?,
    )?;
    first.map(|b| b.entry()).transpose()
}

/// Every archived `(value, height, stamp)` among `values`, for the `Spent`
/// check and creation lookups.
pub fn tachygram_heights(conn: &Connection, values: &[Fp]) -> Result<Vec<(Fp, u32, usize)>> {
    let mut stmt =
        conn.prepare("SELECT height, stamp FROM tachygrams WHERE tg = ?1 ORDER BY height")?;
    let mut hits = Vec::new();
    for value in values {
        let rows = stmt.query_map(params![value.to_bytes()], |r| {
            Ok((*value, r.get(0)?, r.get(1)?))
        })?;
        for row in rows {
            hits.push(row?);
        }
    }
    Ok(hits)
}
