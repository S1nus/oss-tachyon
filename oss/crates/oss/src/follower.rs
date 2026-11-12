//! Follows Zakura over JSON-RPC and archives each canonical Tachyon block once,
//! validated against its parent (design §2).
//!
//! `meta` keys written here:
//! - `genesis_hash`, `activation_height`, `activation_hash`: the network
//!   identity, checked on every start.
//! - `rewind_from`: the lowest height a reorg disconnected since the builder
//!   last cleared it. Lowered in the same transaction that deletes the rows.

use std::{fmt, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use oss_core::{TachyonBlock, bytes32, chain};
use reqwest::{Client, Url};
use rusqlite::{Connection, params};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json, value::RawValue};
use tokio::sync::watch;
use zcash_tachyon::constants::EPOCH_SIZE;

use crate::{archive, store};

const MAX_BODY: usize = 16 * 1024 * 1024;
const POLL: Duration = Duration::from_secs(1);

/// An error object the node answered with.
#[derive(Debug)]
pub struct RpcError {
    pub method: String,
    pub code: Option<i64>,
    pub message: String,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (method, code, message) = (&self.method, self.code, &self.message);
        write!(f, "{method}: node RPC error {code:?}: {message}")
    }
}

impl std::error::Error for RpcError {}

/// The node has pruned a block body the archive still needs, and no backfill
/// node is configured. Fatal: the follower never skips a height.
#[derive(Debug)]
pub struct Pruned(pub u32);

impl fmt::Display for Pruned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the node has pruned the Tachyon body of block {}, which the archive still needs; \
             restart with --backfill-url pointing at an archive node, or start a new --db \
             with --start-height at an epoch boundary the node still retains",
            self.0
        )
    }
}

impl std::error::Error for Pruned {}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    id: Value,
}

/// Zakura JSON-RPC, as devtool's `rpc::Rpc`: optional cookie auth, no
/// credentials in the URL, a bounded body.
pub struct Node {
    client: Client,
    url: Url,
    cookie: Option<PathBuf>,
}

impl Node {
    pub fn new(url: &str, cookie: Option<PathBuf>) -> Result<Self> {
        let url = Url::parse(url)?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "use an HTTP(S) node URL without embedded credentials; use a cookie file for authentication"
        );
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            url,
            cookie,
        })
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let mut request = self
            .client
            .post(self.url.clone())
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(path) = &self.cookie {
            let cookie = std::fs::read_to_string(path).context("cannot read node cookie")?;
            let (user, password) = cookie
                .trim()
                .split_once(':')
                .context("invalid node cookie")?;
            request = request.basic_auth(user, Some(password));
        }
        let mut response = request
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("{method}: transport failed"))?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("{method}: reading response failed"))?
        {
            ensure!(
                chunk.len() <= MAX_BODY.saturating_sub(bytes.len()),
                "{method}: node response exceeds 16 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let envelope: Envelope = serde_json::from_slice(&bytes)
            .with_context(|| format!("{method}: invalid JSON-RPC response (HTTP {status})"))?;
        if let Some(error) = envelope.error {
            return Err(RpcError {
                method: method.into(),
                code: error["code"].as_i64(),
                message: error["message"].as_str().unwrap_or_default().into(),
            }
            .into());
        }
        ensure!(status.is_success(), "{method}: HTTP {status}");
        ensure!(envelope.id == 1, "{method}: response ID mismatch");
        let result = envelope.result.context("missing RPC result")?;
        Ok(serde_json::from_str(result.get())?)
    }

    /// `getblockcount`: the node's best height.
    pub async fn height(&self) -> Result<u32> {
        self.call("getblockcount", json!([])).await
    }

    pub async fn hash(&self, height: u32) -> Result<String> {
        self.call("getblockhash", json!([height])).await
    }

    /// `gettachyonblock`: the verbatim result text, and its parse.
    pub async fn tachyon_block(&self, height: u32) -> Result<(String, TachyonBlock)> {
        let raw: Box<RawValue> = self
            .call("gettachyonblock", json!([height.to_string()]))
            .await?;
        let block: TachyonBlock = serde_json::from_str(raw.get())?;
        ensure!(block.height == height, "gettachyonblock height mismatch");
        Ok((raw.get().to_owned(), block))
    }
}

/// Zakura's answer for a body it no longer has: "body pruned or not retained".
fn is_pruned(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcError>()
        .is_some_and(|e| e.message.contains("pruned"))
}

/// Writes the archive. Publishes the archive tip height after every commit;
/// the value is only a wake-up (design §2.4).
pub struct Follower {
    conn: Connection,
    node: Node,
    backfill: Option<Node>,
    activation: u32,
    start: u32,
    tip: watch::Sender<u32>,
}

impl Follower {
    /// Checks the node's network identity against `meta`, recording it on first
    /// start. An empty archive starts at `start_height`, which must be an epoch
    /// boundary, or at activation. A non-empty one resumes from its tip.
    pub async fn new(
        mut conn: Connection,
        node: Node,
        backfill: Option<Node>,
        start_height: Option<u32>,
    ) -> Result<Self> {
        let activation = match store::meta(&conn, "activation_height")? {
            Some(height) => height,
            // The tip's body is always retained, and it reports activation.
            None => {
                let tip = node.height().await?;
                node.tachyon_block(tip).await?.1.activation_height
            }
        };
        let identity = [
            ("genesis_hash", node.hash(0).await?),
            ("activation_height", activation.to_string()),
            ("activation_hash", node.hash(activation).await?),
        ];
        let tx = conn.transaction()?;
        for (key, value) in identity {
            tx.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES (?1, ?2)",
                [key, &value],
            )?;
            let stored: String =
                tx.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))?;
            ensure!(
                stored == value,
                "network identity changed (including possible regenesis): the node's {key} is \
                 {value}, the archive's is {stored}; use a new --db"
            );
        }
        tx.commit()?;
        let start = start_height.unwrap_or(activation);
        ensure!(
            start
                .checked_sub(activation)
                .is_some_and(|pool| pool.is_multiple_of(EPOCH_SIZE)),
            "--start-height {start} must be an epoch boundary: activation {activation} plus a multiple of {EPOCH_SIZE}"
        );
        let tip = archive::tip(&conn)?.map_or(0, |b| b.height);
        Ok(Self {
            conn,
            node,
            backfill,
            activation,
            start,
            tip: watch::channel(tip).0,
        })
    }

    /// The archive tip height, updated after every commit. 0 while empty.
    pub fn subscribe(&self) -> watch::Receiver<u32> {
        self.tip.subscribe()
    }

    /// Polls the node every second and syncs on each new best block. Returns
    /// only on [`Pruned`]; other errors are logged and retried.
    pub async fn run(mut self) -> Result<()> {
        let mut synced = String::new();
        loop {
            match self
                .node
                .call::<String>("getbestblockhash", json!([]))
                .await
            {
                Ok(best) if best != synced => match self.sync().await {
                    Ok(()) => synced = best,
                    Err(e) if e.is::<Pruned>() => return Err(e),
                    Err(e) => eprintln!("follower: {e:#}"),
                },
                Ok(_) => {}
                Err(e) => eprintln!("follower: {e:#}"),
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Archives every block up to the node's current tip, first disconnecting
    /// any blocks the node has reorganized away.
    // shortcut: SQLite writes run on the async worker; they are short, but move
    // them to spawn_blocking if they ever stall the runtime.
    pub async fn sync(&mut self) -> Result<()> {
        let node_tip = self.node.height().await?;
        let mut parent = self.reconcile(node_tip).await?;
        loop {
            let height = parent.as_ref().map_or(self.start, |p| p.height + 1);
            if height > node_tip {
                return Ok(());
            }
            let (json, block) = self.fetch(height).await?;
            match &parent {
                Some(p) if p.hash != block.previous_block_hash => {
                    let rewound = self.reconcile(node_tip).await?;
                    ensure!(
                        rewound.as_ref().map(|b| &b.hash) != Some(&p.hash),
                        "chain changed during sync; retrying"
                    );
                    parent = rewound;
                    continue;
                }
                Some(p) => chain::validate(&block, Some(p), self.activation)?,
                None => chain::validate_checkpoint(&block, self.activation)?,
            }
            self.commit(&json, &block)?;
            parent = Some(block);
        }
    }

    // `&mut`: a shared `&Follower` holds the `!Sync` connection, so it cannot
    // cross an await in a spawned task.
    async fn fetch(&mut self, height: u32) -> Result<(String, TachyonBlock)> {
        match self.node.tachyon_block(height).await {
            Err(e) if is_pruned(&e) => {
                let backfill = self.backfill.as_ref().ok_or(Pruned(height))?;
                let fetched = backfill.tachyon_block(height).await?;
                ensure!(
                    fetched.1.hash == self.node.hash(height).await?,
                    "backfill block {height} is not on the node's best chain"
                );
                Ok(fetched)
            }
            result => result,
        }
    }

    /// Walks back from the archive tip to the newest block still on the node's
    /// best chain, and disconnects everything above it. Returns the new tip.
    async fn reconcile(&mut self, node_tip: u32) -> Result<Option<TachyonBlock>> {
        let Some(tip) = archive::tip(&self.conn)? else {
            return Ok(None);
        };
        // The lowest height to disconnect; the whole archive if none matches.
        let mut fork = tip.height + 1;
        while let Some(below) = fork.checked_sub(1)
            && let Some(ours) = archive::hash(&self.conn, below)?
            && (below > node_tip || self.node.hash(below).await? != ours)
        {
            fork = below;
        }
        if fork > tip.height {
            return Ok(Some(tip));
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM tachygrams WHERE height >= ?1", [fork])?;
        tx.execute("DELETE FROM blocks WHERE height >= ?1", [fork])?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('rewind_from', ?1)
             ON CONFLICT(key) DO UPDATE SET value = MIN(CAST(value AS INTEGER), ?1)",
            [fork],
        )?;
        tx.commit()?;
        let tip = archive::tip(&self.conn)?;
        self.tip.send_replace(tip.as_ref().map_or(0, |b| b.height));
        Ok(tip)
    }

    fn commit(&mut self, json: &str, block: &TachyonBlock) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO blocks(height, hash, prev_hash, epoch, json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                block.height,
                block.hash,
                block.previous_block_hash,
                block.epoch,
                json
            ],
        )?;
        {
            let mut insert =
                tx.prepare("INSERT INTO tachygrams(tg, height, stamp) VALUES (?1, ?2, ?3)")?;
            for (index, stamp) in block.stamps.iter().enumerate() {
                for value in &stamp.tachygrams {
                    insert.execute(params![bytes32(value)?, block.height, index])?;
                }
            }
        }
        tx.commit()?;
        self.tip.send_replace(block.height);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use oss_core::evidence::testing::*;
    use zcash_tachyon::pasta::Fp;

    use super::*;
    use crate::testing::{FakeNode, TempDb, served};

    async fn follower(
        db: &TempDb,
        node: &FakeNode,
        backfill: Option<&FakeNode>,
        start: Option<u32>,
    ) -> Result<Follower> {
        let backfill = backfill.map(|b| Node::new(&b.url, None)).transpose()?;
        Follower::new(
            store::open(&db.0)?,
            Node::new(&node.url, None)?,
            backfill,
            start,
        )
        .await
    }

    /// The archive holds exactly the node's chain from `from`, and its index
    /// exactly those blocks' tachygrams.
    fn assert_mirrors(conn: &Connection, node: &FakeNode, from: u32) -> Result<()> {
        let node = node.data();
        let expected: Vec<_> = node.blocks.iter().filter(|b| b.height >= from).collect();
        let stored: Vec<(u32, String)> = conn
            .prepare("SELECT height, hash FROM blocks ORDER BY height")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let hashes: Vec<_> = expected
            .iter()
            .map(|b| (b.height, b.hash.clone()))
            .collect();
        assert_eq!(stored, hashes);
        let indexed: usize = conn.query_row("SELECT COUNT(*) FROM tachygrams", [], |r| r.get(0))?;
        let published = expected.iter().flat_map(|b| &b.stamps);
        assert_eq!(
            indexed,
            published.map(|s| s.tachygrams.len()).sum::<usize>()
        );
        Ok(())
    }

    fn rewind_from(conn: &Connection) -> Result<Option<String>> {
        store::meta(conn, "rewind_from")
    }

    #[tokio::test]
    async fn ingests_more_than_two_epochs() -> Result<()> {
        let mut blocks = vec![genesis()];
        populate_epoch(&mut blocks, 3)?;
        mine_to_epoch(&mut blocks, 1)?;
        populate_epoch(&mut blocks, 70)?;
        mine_to_epoch(&mut blocks, 2)?;
        populate_epoch(&mut blocks, 1)?;
        let node = FakeNode::start(blocks.clone())?;
        let db = TempDb::new();
        let mut follower = follower(&db, &node, None, None).await?;
        let tip = follower.subscribe();
        follower.sync().await?;

        let conn = store::open(&db.0)?;
        let activation = blocks[0].height;
        assert_mirrors(&conn, &node, activation)?;
        assert_eq!(*tip.borrow(), blocks.last().unwrap().height);
        assert_eq!(archive::first_height(&conn)?, Some(activation));
        assert_eq!(
            archive::tip(&conn)?.unwrap().hash,
            blocks.last().unwrap().hash
        );
        let json = archive::block_json(&conn, blocks[1].height)?.unwrap();
        assert_eq!(json, serde_json::to_string(&served(&blocks[1]))?);
        assert_eq!(
            archive::block(&conn, blocks[1].height)?
                .unwrap()
                .stamps
                .len(),
            1
        );

        let first = |epoch| blocks.iter().position(|b| b.epoch == epoch).unwrap();
        let (e1, e2) = (first(1), first(2));
        let epoch0 = archive::epoch_blocks(&conn, 0)?;
        assert_eq!(epoch0.len(), e1 + 1);
        assert_eq!(epoch0.last().unwrap().hash, blocks[e1].hash);
        assert_eq!(archive::epoch_blocks(&conn, 2)?.len(), blocks.len() - e2);
        assert_eq!(
            archive::closing_hash(&conn, 0)?,
            Some(blocks[e1 - 1].hash.clone())
        );
        assert_eq!(
            archive::closing_hash(&conn, 1)?,
            Some(blocks[e2 - 1].hash.clone())
        );
        assert_eq!(archive::closing_hash(&conn, 2)?, None);
        assert_eq!(archive::entry_anchor(&conn, 1)?, Some(blocks[e1].entry()?));
        assert_eq!(archive::entry_anchor(&conn, 3)?, None);

        // Values 1..=64 fill each populated block's first stamp, 65..=70 the second.
        let (one, last) = (Fp::from(1), Fp::from(70));
        let heights = |i: usize| blocks[i].height;
        assert_eq!(
            archive::tachygram_heights(&conn, &[one, last, Fp::from(1000)])?,
            [
                (one, heights(1), 0),
                (one, heights(e1 + 1), 0),
                (one, blocks.last().unwrap().height, 0),
                (last, heights(e1 + 1), 1)
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn reorg_inside_an_epoch_disconnects_rows_above_the_fork() -> Result<()> {
        let mut blocks = vec![genesis()];
        for _ in 0..20 {
            mine(&mut blocks)?;
        }
        populate_epoch(&mut blocks, 5)?;
        for _ in 0..10 {
            mine(&mut blocks)?;
        }
        let node = FakeNode::start(blocks.clone())?;
        let db = TempDb::new();
        let mut follower = follower(&db, &node, None, None).await?;
        let tip = follower.subscribe();
        follower.sync().await?;
        let conn = store::open(&db.0)?;
        assert_eq!(rewind_from(&conn)?, None);

        // A shorter branch from height 25 publishes only values 1 and 2.
        let mut branch = blocks[..17].to_vec();
        let fork = branch.last().unwrap().height + 1;
        populate_epoch(&mut branch, 2)?;
        for _ in 0..5 {
            mine(&mut branch)?;
        }
        node.reorg(fork, branch[17..].to_vec());
        follower.sync().await?;

        assert_mirrors(&conn, &node, blocks[0].height)?;
        assert_eq!(rewind_from(&conn)?, Some(fork.to_string()));
        assert_eq!(*tip.borrow(), node.data().blocks.last().unwrap().height);
        let values = [Fp::from(1), Fp::from(3)];
        assert_eq!(
            archive::tachygram_heights(&conn, &values)?,
            [(values[0], fork, 0)]
        );
        Ok(())
    }

    #[tokio::test]
    async fn reorg_across_an_epoch_boundary_keeps_the_lowest_rewind() -> Result<()> {
        let mut blocks = vec![genesis()];
        mine_to_epoch(&mut blocks, 1)?;
        for _ in 0..5 {
            mine(&mut blocks)?;
        }
        let node = FakeNode::start(blocks.clone())?;
        let db = TempDb::new();
        let mut follower = follower(&db, &node, None, None).await?;
        follower.sync().await?;
        let conn = store::open(&db.0)?;
        let boundary = blocks.iter().position(|b| b.epoch == 1).unwrap();
        let old_entry = archive::entry_anchor(&conn, 1)?;

        // Fork two blocks before the boundary, with new stamps in epoch 0.
        let mut branch = blocks[..boundary - 2].to_vec();
        let low = branch.last().unwrap().height + 1;
        populate_epoch(&mut branch, 4)?;
        mine_to_epoch(&mut branch, 1)?;
        for _ in 0..8 {
            mine(&mut branch)?;
        }
        node.reorg(low, branch[boundary - 2..].to_vec());
        follower.sync().await?;
        assert_mirrors(&conn, &node, blocks[0].height)?;
        assert_eq!(rewind_from(&conn)?, Some(low.to_string()));
        let chain = node.data().blocks.clone();
        assert_eq!(
            archive::closing_hash(&conn, 0)?,
            Some(chain[boundary - 1].hash.clone())
        );
        assert_ne!(chain[boundary - 1].hash, blocks[boundary - 1].hash);
        assert_eq!(
            archive::entry_anchor(&conn, 1)?,
            Some(chain[boundary].entry()?)
        );
        assert_ne!(archive::entry_anchor(&conn, 1)?, old_entry);

        // A later, higher reorg leaves the builder's pending rewind at `low`.
        let high = chain[boundary + 3].height;
        let mut branch = chain[..boundary + 3].to_vec();
        populate_epoch(&mut branch, 1)?;
        node.reorg(high, branch[boundary + 3..].to_vec());
        follower.sync().await?;
        assert_mirrors(&conn, &node, blocks[0].height)?;
        assert_eq!(rewind_from(&conn)?, Some(low.to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn pruned_gap_fails_closed_and_backfill_fills_it() -> Result<()> {
        let mut blocks = vec![genesis()];
        for _ in 0..30 {
            mine(&mut blocks)?;
        }
        populate_epoch(&mut blocks, 3)?;
        mine(&mut blocks)?;
        let node = FakeNode::start(blocks.clone())?;
        let pruned_below = blocks[20].height;
        node.prune_below(pruned_below);
        let db = TempDb::new();

        let error = follower(&db, &node, None, None).await?.sync().await;
        assert!(error.unwrap_err().is::<Pruned>());
        let conn = store::open(&db.0)?;
        assert!(archive::tip(&conn)?.is_none());
        // `run` stops on it rather than retrying past the gap.
        let run = tokio::spawn(follower(&db, &node, None, None).await?.run());
        let error = tokio::time::timeout(Duration::from_secs(10), run).await??;
        assert!(error.unwrap_err().is::<Pruned>());
        assert!(archive::tip(&conn)?.is_none());

        let archive_node = FakeNode::start(blocks.clone())?;
        follower(&db, &node, Some(&archive_node), None)
            .await?
            .sync()
            .await?;
        assert_mirrors(&conn, &node, blocks[0].height)?;
        let backfilled: Vec<u32> = archive_node
            .data()
            .requests
            .iter()
            .filter(|(method, _)| method == "gettachyonblock")
            .map(|(_, params)| params[0].as_str().unwrap().parse().unwrap())
            .collect();
        assert_eq!(
            backfilled,
            (blocks[0].height..pruned_below).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn checkpoint_start_skips_history_below_it() -> Result<()> {
        let mut blocks = vec![genesis()];
        populate_epoch(&mut blocks, 2)?;
        mine_to_epoch(&mut blocks, 1)?;
        populate_epoch(&mut blocks, 3)?;
        mine(&mut blocks)?;
        let start = blocks.iter().find(|b| b.epoch == 1).unwrap().height;
        let node = FakeNode::start(blocks.clone())?;
        node.prune_below(start);
        let db = TempDb::new();
        assert!(follower(&db, &node, None, Some(start + 1)).await.is_err());
        assert!(follower(&db, &node, None, Some(start - 1)).await.is_err());

        follower(&db, &node, None, Some(start))
            .await?
            .sync()
            .await?;
        let conn = store::open(&db.0)?;
        assert_mirrors(&conn, &node, start)?;
        assert!(archive::epoch_blocks(&conn, 0)?.is_empty());
        assert_eq!(archive::closing_hash(&conn, 0)?, None);
        assert_eq!(
            archive::entry_anchor(&conn, 1)?,
            Some(blocks[blocks.len() - 3].entry()?)
        );
        Ok(())
    }

    #[tokio::test]
    async fn restart_resumes_without_refetching() -> Result<()> {
        let mut blocks = vec![genesis()];
        for _ in 0..20 {
            mine(&mut blocks)?;
        }
        populate_epoch(&mut blocks, 2)?;
        let node = FakeNode::start(blocks)?;
        let db = TempDb::new();
        follower(&db, &node, None, None).await?.sync().await?;
        let tip = node.data().blocks.last().unwrap().height;

        node.data().requests.clear();
        for _ in 0..3 {
            mine(&mut node.data().blocks)?;
        }
        let mut follower = follower(&db, &node, None, None).await?;
        assert_eq!(*follower.subscribe().borrow(), tip);
        follower.sync().await?;
        let fetched: Vec<String> = node
            .data()
            .requests
            .iter()
            .filter(|(method, _)| method == "gettachyonblock")
            .map(|(_, params)| params[0].as_str().unwrap().into())
            .collect();
        assert_eq!(fetched, [tip + 1, tip + 2, tip + 3].map(|h| h.to_string()));
        assert_mirrors(&store::open(&db.0)?, &node, tip - 21)?;
        Ok(())
    }

    #[tokio::test]
    async fn network_identity_mismatch_is_refused() -> Result<()> {
        let mut blocks = vec![genesis()];
        mine(&mut blocks)?;
        let node = FakeNode::start(blocks.clone())?;
        let db = TempDb::new();
        follower(&db, &node, None, None).await?.sync().await?;

        // Same heights, different activation block: a regenesis.
        let mut other = vec![genesis()];
        other[0].hash = "ab".repeat(32);
        mine(&mut other)?;
        let other = FakeNode::start(other)?;
        let Err(error) = follower(&db, &other, None, None).await else {
            panic!("a different activation block must be refused");
        };
        assert!(error.to_string().contains("network identity changed"));
        follower(&db, &node, None, None).await?;
        Ok(())
    }
}
