//! Reads of the canonical Tachyon archive that the follower writes. Any
//! connection works, including a read-only one.
//!
//! `blocks.json` is the verbatim `gettachyonblock` text. `tachygrams.stamp` is
//! the index into that block's `stamps`. `thin_blocks.json` is that text
//! through [`thin_json`], written with the block.

use anyhow::{Context, Result, ensure};
use oss_core::TachyonBlock;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
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

/// The epoch of the oldest archived block.
pub fn first_epoch(conn: &Connection) -> Result<Option<u32>> {
    Ok(conn
        .query_row(
            "SELECT epoch FROM blocks ORDER BY height LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
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

/// The newest archived height, without reading its block.
pub fn tip_height(conn: &Connection) -> Result<Option<u32>> {
    Ok(conn.query_row("SELECT MAX(height) FROM blocks", [], |r| r.get(0))?)
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

/// The thin form of `json`, a block's `gettachyonblock` text: every stamp's
/// `tachygrams` emptied and `finalized` removed, since it depends on the tip
/// and is added per request by [`with_finalized`].
pub fn thin_json(json: &str) -> Result<String> {
    let mut block: Value = serde_json::from_str(json)?;
    let object = block.as_object_mut().context("a block is a JSON object")?;
    object.remove("finalized");
    ensure!(!object.is_empty(), "a block has fields");
    if let Some(stamps) = object.get_mut("stamps").and_then(Value::as_array_mut) {
        for stamp in stamps {
            stamp["tachygrams"] = json!([]);
        }
    }
    Ok(serde_json::to_string(&block)?)
}

/// A [`thin_json`] body with `finalized` set, by one splice: no parse.
pub fn with_finalized(thin: &str, finalized: bool) -> Result<String> {
    let rest = thin
        .strip_prefix('{')
        .context("a thin block is a JSON object")?;
    Ok(format!("{{\"finalized\":{finalized},{rest}"))
}

/// The cached [`thin_json`] of the block at `height`, and the byte length of
/// its full JSON. `None` if it is not archived, or archived before the cache
/// and not yet filled.
pub fn thin_block(conn: &Connection, height: u32) -> Result<Option<(String, usize)>> {
    Ok(conn
        .query_row(
            "SELECT json, full_len FROM thin_blocks WHERE height = ?1",
            [height],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Caches the thin form of the block at `height`, whose text is `json`.
pub fn cache_thin(conn: &Connection, height: u32, json: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO thin_blocks(height, json, full_len) VALUES (?1, ?2, ?3)",
        params![height, thin_json(json)?, json.len()],
    )?;
    Ok(())
}
