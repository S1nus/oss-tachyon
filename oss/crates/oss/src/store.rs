//! The OSS database: one SQLite file in WAL mode. This module owns the schema;
//! each task owns the queries on its tables.

use std::{path::Path, time::Duration};

use anyhow::{Result, ensure};
use rusqlite::Connection;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS blocks(height INTEGER PRIMARY KEY, hash TEXT NOT NULL UNIQUE,
  prev_hash TEXT NOT NULL, epoch INTEGER NOT NULL, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tachygrams(tg BLOB NOT NULL, height INTEGER NOT NULL,
  stamp INTEGER NOT NULL, PRIMARY KEY(tg, height));
CREATE INDEX IF NOT EXISTS tachygrams_by_height ON tachygrams(height);
CREATE TABLE IF NOT EXISTS epochs(epoch INTEGER PRIMARY KEY, discriminant BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS evidence(epoch INTEGER NOT NULL, closing_hash TEXT NOT NULL,
  profiles TEXT NOT NULL, PRIMARY KEY(epoch, closing_hash));
CREATE TABLE IF NOT EXISTS buckets(epoch INTEGER NOT NULL, closing_hash TEXT NOT NULL,
  depth INTEGER NOT NULL, bits INTEGER NOT NULL, members INTEGER NOT NULL, pcd BLOB NOT NULL,
  PRIMARY KEY(epoch, closing_hash, depth, bits));
CREATE TABLE IF NOT EXISTS spans(epoch INTEGER NOT NULL, chunk INTEGER NOT NULL,
  start_anchor BLOB NOT NULL, end_height INTEGER NOT NULL, members BLOB NOT NULL,
  pcd BLOB NOT NULL, PRIMARY KEY(epoch, chunk));
";

/// Opens the database at `path`, creating it and its tables if needed.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // The follower and the builder both write; wait for the lock, don't fail.
    conn.busy_timeout(Duration::from_secs(5))?;
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    ensure!(
        mode == "wal",
        "{} cannot use WAL mode: got {mode}",
        path.display()
    );
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_is_idempotent_and_uses_wal() -> Result<()> {
        let path = std::env::temp_dir().join(format!("oss-store-{}.sqlite", std::process::id()));
        for _ in 0..2 {
            let conn = open(&path)?;
            conn.execute("INSERT OR REPLACE INTO meta VALUES ('k', 'v')", [])?;
        }
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        Ok(())
    }
}
