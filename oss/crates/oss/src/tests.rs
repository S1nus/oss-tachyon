//! End-to-end tests over real HTTP against `FakeNode`. Most drive the follower
//! and the builder by hand, so "the builder is behind" is a fixed state, not a
//! race; the last runs the whole process with `spawn_oss`.

use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, atomic::AtomicU32},
};

use anyhow::{Context, Result, ensure};
use oss_core::{
    AnchorSpansResult, BucketParams, BucketProfile, BucketResult, EpochResult,
    ErrorCode::{InvalidParams, NotReady, Spent, Unavailable},
    EvidenceStatus, InfoResult, SpentData, TachyonBlock, UnspentResult, anchor, anchor_hex,
    evidence::testing::*,
    pcd_from_hex, verify_pcd,
};
use reqwest::{
    Client, StatusCode,
    header::{ACCEPT_ENCODING, CONTENT_ENCODING},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use udon::field::Field;
use zcash_tachyon::{
    EpochIndex, QrProfile,
    constants::EPOCH_SIZE,
    keys::private::SpendingKey,
    nullifier::{self, Nullifier},
    pasta::Fp,
    stamp::proof::{pool, qr},
};

use crate::{
    archive, builder, epochs,
    follower::{Follower, Node},
    server::{self, Shared},
    store,
    testing::{FakeNode, TempDb, spawn_oss},
};

/// Serves the RPC alone, so the test drives the follower and builder by hand.
async fn serve(db: &Path, cookie: Option<&str>, node_height: u32) -> Result<String> {
    let shared = Arc::new(Shared {
        db: db.into(),
        cookie: cookie.map(Into::into),
        jobs: Arc::new(Semaphore::new(2)),
        node_height: Arc::new(AtomicU32::new(node_height)),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, server::router(shared)).await });
    Ok(url)
}

async fn envelope(url: &str, method: &str, params: Value) -> Result<Value> {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let response = Client::new().post(url).json(&request).send().await?;
    Ok(response.error_for_status()?.json().await?)
}

async fn call<T: DeserializeOwned>(url: &str, method: &str, params: Value) -> Result<T> {
    let mut envelope = envelope(url, method, params).await?;
    ensure!(
        envelope["error"].is_null(),
        "{method}: {}",
        envelope["error"]
    );
    Ok(serde_json::from_value(envelope["result"].take())?)
}

/// The error code and data `method` fails with.
async fn fail(url: &str, method: &str, params: Value) -> Result<(i64, Value)> {
    let mut envelope = envelope(url, method, params).await?;
    let code = envelope["error"]["code"].as_i64();
    Ok((
        code.with_context(|| format!("{method} did not fail"))?,
        envelope["error"]["data"].take(),
    ))
}

async fn code(url: &str, method: &str, params: Value) -> Result<i64> {
    Ok(fail(url, method, params).await?.0)
}

/// One note's nullifiers for epochs `0..n`, derived as a wallet does.
fn nullifiers(n: u32) -> Vec<Nullifier> {
    let rng = &mut rand::rng();
    let mk = SpendingKey::random(rng)
        .derive_nullifier_private()
        .derive_note_private(nullifier::Trapdoor::random(rng));
    (0..n)
        .map(|e| mk.derive_nullifier(EpochIndex::new(e)))
        .collect()
}

fn hex32(nf: Nullifier) -> String {
    hex::encode(Fp::from(nf).to_bytes())
}

fn first(blocks: &[TachyonBlock], epoch: u32) -> usize {
    blocks.iter().position(|b| b.epoch == epoch).unwrap()
}

/// Epochs 0 and 1 closed, epoch 2 open, all archived but nothing built. A note
/// spent at `spend_height` in epoch 0 published `spent[0]`, and `spent[1]` as
/// the "next" nullifier.
struct Closed {
    _node: FakeNode,
    db: TempDb,
    url: String,
    blocks: Vec<TachyonBlock>,
    spent: Vec<Nullifier>,
    spend_height: u32,
}

impl Closed {
    async fn new() -> Result<Self> {
        let spent = nullifiers(2);
        let mut blocks = vec![genesis()];
        populate_epoch(&mut blocks, 3)?;
        mine_stamp(&mut blocks, &[spent[0].into(), spent[1].into()])?;
        let spend_height = blocks.last().unwrap().height;
        mine_to_epoch(&mut blocks, 1)?;
        populate_epoch(&mut blocks, 2)?;
        mine_to_epoch(&mut blocks, 2)?;
        populate_epoch(&mut blocks, 1)?;
        let node = FakeNode::start(blocks.clone())?;
        let db = TempDb::new();
        let mut follower =
            Follower::new(store::open(&db.0)?, Node::new(&node.url, None)?, None, None).await?;
        let tip = blocks.last().unwrap().height;
        let url = serve(&db.0, None, tip + 2).await?;
        assert_eq!(code(&url, "oss_info", json!({})).await?, NotReady.code());
        follower.sync().await?;
        Ok(Self {
            _node: node,
            db,
            url,
            blocks,
            spent,
            spend_height,
        })
    }

    fn build(&self) -> Result<()> {
        builder::catch_up(&store::open(&self.db.0)?, &|| false)
    }
}

#[tokio::test]
async fn every_method_answers_and_every_error_code_is_produced() -> Result<()> {
    let closed = Closed::new().await?;
    let (url, blocks) = (&closed.url, &closed.blocks);
    let tip = blocks.last().unwrap();
    let (e1, e2) = (first(blocks, 1), first(blocks, 2));
    let entry2 = blocks[e2].entry()?;

    // Archived, nothing built yet.
    let info: InfoResult = call(url, "oss_info", json!({})).await?;
    assert_eq!(info.genesis_hash, hex::encode([0; 32]));
    assert_eq!((info.activation_height, info.epoch_length), (8, EPOCH_SIZE));
    assert!(info.proof_system.mock && info.proof_system.tachyon.starts_with("088f4e1"));
    assert_eq!((info.tip.height, info.tip.epoch), (tip.height, 2));
    assert_eq!(
        (&info.tip.hash, &info.tip.anchor),
        (&tip.hash, &tip.anchor_after)
    );
    assert_eq!(info.tip.entry_anchor, anchor_hex(entry2));
    assert_eq!(info.archive_from_height, 8);
    assert_eq!(info.evidence_ready_through, None);
    assert_eq!(
        (info.node.height, info.node.lag_blocks),
        (tip.height + 2, 2)
    );
    let epoch0: EpochResult = call(url, "oss_epoch", json!({"epoch": 0})).await?;
    assert_eq!(epoch0.closing_hash, Some(blocks[e1 - 1].hash.clone()));
    assert_eq!(epoch0.evidence.status, EvidenceStatus::Building);
    let root = |epoch| {
        serde_json::to_value(BucketParams {
            epoch,
            profile: QrProfile::ROOT,
        })
        .unwrap()
    };
    assert_eq!(code(url, "oss_bucket", root(0)).await?, NotReady.code());
    let from = |anchors: &[_]| json!({"from": anchors.iter().copied().map(anchor_hex).collect::<Vec<_>>()});
    let (not_ready, data) = fail(url, "oss_anchorspans", from(&[entry2])).await?;
    assert_eq!(
        (not_ready, data["retryAfterMs"].as_u64()),
        (NotReady.code(), Some(1000))
    );
    let unspent = json!({"epochStart": 0, "nullifiers": [hex32(closed.spent[0])]});
    assert_eq!(code(url, "oss_unspent", unspent).await?, NotReady.code());

    closed.build()?;
    let info: InfoResult = call(url, "oss_info", json!({})).await?;
    assert_eq!(info.evidence_ready_through, Some(1));
    let conn = store::open_read(&closed.db.0)?;
    let r0 = epochs::discriminant(&conn, 0)?.context("sealed")?;
    let epoch0: EpochResult = call(url, "oss_epoch", json!({"epoch": 0})).await?;
    assert_eq!(epoch0.evidence.status, EvidenceStatus::Ready);
    assert_eq!(
        epoch0.evidence.discriminant,
        Some(hex::encode(r0.0.to_bytes()))
    );
    let members = |members| {
        Some(vec![BucketProfile {
            profile: QrProfile::ROOT,
            members,
        }])
    };
    assert_eq!(epoch0.evidence.profiles, members(5));
    let epoch1: EpochResult = call(url, "oss_epoch", json!({"epoch": 1})).await?;
    assert_eq!(epoch1.evidence.profiles, members(2));
    let epoch2: EpochResult = call(url, "oss_epoch", json!({"epoch": 2})).await?;
    assert_eq!(
        (epoch2.closing_hash, epoch2.evidence.status),
        (None, EvidenceStatus::Open)
    );
    assert_eq!(epoch2.evidence.discriminant, None);

    let bucket: BucketResult = call(url, "oss_bucket", root(0)).await?;
    let pcd = pcd_from_hex::<qr::QrBucket>(&bucket.pcd)?;
    assert!(verify_pcd(&pcd)?);
    let data = pcd.data();
    let expected = (EpochIndex::new(0), blocks[0].entry()?, blocks[e1].entry()?);
    assert_eq!((data.0, data.1, data.2), expected);
    assert_eq!((data.3, data.4), (r0, QrProfile::ROOT));
    let leaf = json!({"epoch": 0, "profile": {"depth": 1, "bits": 1}});
    assert_eq!(code(url, "oss_bucket", leaf).await?, Unavailable.code());
    assert_eq!(code(url, "oss_bucket", root(2)).await?, NotReady.code());

    let target = anchor(&tip.anchor_after)?;
    let spans: AnchorSpansResult = call(url, "oss_anchorspans", from(&[entry2, target])).await?;
    assert_eq!((spans.epoch, &spans.target), (2, &tip.anchor_after));
    assert_eq!((spans.chunks.len(), spans.start), (1, vec![Some(0), None]));
    let chunk = pcd_from_hex::<pool::AnchorSpan>(&spans.chunks[0])?;
    assert!(verify_pcd(&chunk)?);
    assert_eq!(*chunk.data(), (entry2, target));
    // The builder keeps only the current and previous epoch.
    let entry0 = blocks[0].entry()?;
    assert_eq!(
        code(url, "oss_anchorspans", from(&[entry0])).await?,
        Unavailable.code()
    );
    for bad in [
        json!({"from": []}),
        from(&[entry2; 65]),
        json!({"from": ["zz"]}),
    ] {
        assert_eq!(
            code(url, "oss_anchorspans", bad).await?,
            InvalidParams.code()
        );
    }

    // `finalized` is recomputed: at least 1000 blocks below the tip.
    let deep = tip.height - 1000;
    for (height, finalized) in [
        (9, true),
        (deep, true),
        (deep + 1, false),
        (tip.height, false),
    ] {
        let block: Value = call(url, "oss_block", json!({"height": height})).await?;
        assert_eq!(block["finalized"], finalized, "block {height}");
        let block: TachyonBlock = serde_json::from_value(block)?;
        assert_eq!(block.hash, blocks[(height - 8) as usize].hash);
    }
    let above = json!({"height": tip.height + 1});
    assert_eq!(code(url, "oss_block", above).await?, Unavailable.code());
    assert_eq!(
        code(url, "oss_block", json!({})).await?,
        InvalidParams.code()
    );
    assert_eq!(
        code(url, "oss_epoch", json!({"epoch": "x"})).await?,
        InvalidParams.code()
    );
    assert_eq!(code(url, "getblock", json!([])).await?, -32601);
    Ok(())
}

#[tokio::test]
async fn thin_blocks_keep_the_anchor_chain_and_lookups_find_published_values() -> Result<()> {
    let closed = Closed::new().await?;
    let (url, height) = (&closed.url, closed.spend_height);
    let at = |thin| json!({"height": height, "thin": thin});
    let full: TachyonBlock = call(url, "oss_block", at(false)).await?;
    let thin: TachyonBlock = call(url, "oss_block", at(true)).await?;
    assert!(thin.is_thin() && !full.is_thin());
    assert_eq!(
        (&thin.hash, &thin.anchor_after, thin.stamps.len()),
        (&full.hash, &full.anchor_after, full.stamps.len())
    );
    let previous = &closed.blocks[(height - 9) as usize];
    oss_core::chain::validate_thin(&thin, Some(previous), 8)?;
    assert!(oss_core::chain::validate(&thin, Some(previous), 8).is_err());

    // A range serves each block as `oss_block` does, and stops at the tip.
    let range = |from: u32, count: u32| json!({"from": from, "count": count, "thin": true});
    let page: oss_core::BlocksResult = call(url, "oss_blocks", range(height - 1, 100)).await?;
    let heights: Vec<u32> = page.blocks.iter().map(|b| b.height).collect();
    assert_eq!(heights, (height - 1..height + 99).collect::<Vec<_>>());
    assert_eq!(
        serde_json::to_value(&page.blocks[1])?,
        serde_json::to_value(&thin)?
    );
    let tip = closed.blocks.last().unwrap().height;
    let end: oss_core::BlocksResult = call(url, "oss_blocks", range(tip - 1, 100)).await?;
    assert_eq!(end.blocks.len(), 2);
    assert_eq!(
        code(url, "oss_blocks", range(tip + 1, 1)).await?,
        Unavailable.code()
    );
    for count in [0, 1001] {
        assert_eq!(
            code(url, "oss_blocks", range(height, count)).await?,
            InvalidParams.code()
        );
    }

    let unknown = nullifiers(3)[2];
    let values =
        json!({"values": [hex32(closed.spent[0]), hex32(unknown), hex32(closed.spent[1])]});
    let found: oss_core::TachygramsResult = call(url, "oss_tachygrams", values).await?;
    assert_eq!(found.heights, vec![Some(height), None, Some(height)]);
    for bad in [
        json!({"values": []}),
        json!({"values": vec![hex32(unknown); 65]}),
        json!({"values": [hex::encode([0; 32])]}),
        json!({"values": ["abc"]}),
    ] {
        assert_eq!(
            code(url, "oss_tachygrams", bad).await?,
            InvalidParams.code()
        );
    }
    Ok(())
}

#[tokio::test]
async fn transactions_are_found_once_the_index_reaches_the_tip() -> Result<()> {
    let closed = Closed::new().await?;
    let (url, height) = (&closed.url, closed.spend_height);
    let unknown = "ab".repeat(32);
    let ids = json!({"txids": [format!("{height:064x}"), unknown]});
    let found: oss_core::TransactionsResult = call(url, "oss_transactions", ids.clone()).await?;
    assert_eq!(found.heights, vec![Some(height), None]);
    for bad in [
        json!({"txids": []}),
        json!({"txids": vec![unknown.clone(); 65]}),
        json!({"txids": ["abc"]}),
    ] {
        assert_eq!(
            code(url, "oss_transactions", bad).await?,
            InvalidParams.code()
        );
    }
    // Behind the archive tip, a miss could be an unindexed block.
    store::open(&closed.db.0)?.execute(
        "UPDATE meta SET value = '1' WHERE key = 'txindex_through'",
        [],
    )?;
    assert_eq!(code(url, "oss_transactions", ids).await?, NotReady.code());
    Ok(())
}

#[tokio::test]
async fn unspent_proves_closed_epochs_and_answers_spent() -> Result<()> {
    let closed = Closed::new().await?;
    closed.build()?;
    let url = &closed.url;
    let note = nullifiers(3);
    let unspent = |start: u32, nfs: &[Nullifier]| json!({"epochStart": start, "nullifiers": nfs.iter().copied().map(hex32).collect::<Vec<_>>()});

    let result: UnspentResult = call(url, "oss_unspent", unspent(0, &note[..2])).await?;
    let segment = pcd_from_hex::<pool::ArbitraryUnspent>(&result.pcd)?;
    assert!(verify_pcd(&segment)?);
    let conn = store::open_read(&closed.db.0)?;
    let entry = |e| archive::entry_anchor(&conn, e)?.context("archived epoch");
    let (entry0, entry2) = (entry(0)?, entry(2)?);
    let expected = (entry0, EpochIndex::new(0), EpochIndex::new(2), entry2);
    assert_eq!(*segment.data(), expected);
    assert_eq!((result.epoch_start, result.epoch_next), (0, 2));
    assert_eq!(
        (result.anchor_start, result.anchor_next),
        (anchor_hex(entry0), anchor_hex(entry2))
    );

    // `spent[1]` went out in epoch 0 as the "next" nullifier: epoch 1's
    // buckets do not hold it, so only the tachygram index catches it.
    let (code_, data) = fail(url, "oss_unspent", unspent(1, &closed.spent[1..])).await?;
    assert_eq!(code_, Spent.code());
    let height = closed.spend_height;
    let txid = format!("{height:064x}");
    let spent: SpentData = serde_json::from_value(data)?;
    assert_eq!(
        spent,
        SpentData {
            epoch: 0,
            height,
            txid
        }
    );

    assert_eq!(
        code(url, "oss_unspent", unspent(1, &note[1..3])).await?,
        NotReady.code()
    );
    let zero = hex::encode([0; 32]);
    let invalid = [
        unspent(0, &[]),
        unspent(0, &[note[0]; 65]),
        unspent(u32::MAX, &note[..2]),
        json!({"epochStart": 0, "nullifiers": [zero]}),
        json!({"epochStart": 0, "nullifiers": ["ff".repeat(32)]}),
        json!({"epochStart": 0, "nullifiers": ["abc"]}),
        json!({"nullifiers": []}),
    ];
    for params in invalid {
        assert_eq!(
            code(url, "oss_unspent", params).await?,
            InvalidParams.code()
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_builder_held_busy_catches_up_and_a_checkpoint_bounds_the_archive() -> Result<()> {
    let mut blocks = vec![genesis()];
    mine_to_epoch(&mut blocks, 1)?;
    populate_epoch(&mut blocks, 2)?;
    let start = blocks[first(&blocks, 1)].height;
    let node = FakeNode::start(blocks)?;
    node.prune_below(start);
    let db = TempDb::new();
    let mut follower = Follower::new(
        store::open(&db.0)?,
        Node::new(&node.url, None)?,
        None,
        Some(start),
    )
    .await?;
    let url = &serve(&db.0, None, 0).await?;
    follower.sync().await?;
    let conn = store::open(&db.0)?;
    builder::catch_up(&conn, &|| false)?;

    let info: InfoResult = call(url, "oss_info", json!({})).await?;
    assert_eq!(info.archive_from_height, start);
    assert_eq!(
        code(url, "oss_epoch", json!({"epoch": 0})).await?,
        Unavailable.code()
    );
    let bucket = json!({"epoch": 0, "profile": {"depth": 0, "bits": 0}});
    assert_eq!(code(url, "oss_bucket", bucket).await?, Unavailable.code());
    let unspent = json!({"epochStart": 0, "nullifiers": [hex32(nullifiers(1)[0])]});
    assert_eq!(code(url, "oss_unspent", unspent).await?, Unavailable.code());

    // 50 blocks arrive, 10 with a stamp, while the builder is busy.
    let built = node.data().blocks.last().unwrap().anchor_after.clone();
    for i in 0..50 {
        if i % 5 == 0 {
            populate_epoch(&mut node.data().blocks, 1)?;
        } else {
            mine(&mut node.data().blocks)?;
        }
    }
    follower.sync().await?;
    let chain = node.data().blocks.clone();
    let entry = chain[first(&chain, 1)].entry()?;
    let target = anchor(&chain.last().unwrap().anchor_after)?;
    let from = |anchors: &[_]| json!({"from": anchors.iter().copied().map(anchor_hex).collect::<Vec<_>>()});
    let (not_ready, data) = fail(url, "oss_anchorspans", from(&[target])).await?;
    assert_eq!(not_ready, NotReady.code());
    assert!(data["reason"].as_str().unwrap().contains("behind"));
    // What is built stays served meanwhile, in one consistent read.
    let stale: AnchorSpansResult = call(url, "oss_anchorspans", from(&[entry])).await?;
    assert_eq!(stale.target, built);

    builder::catch_up(&conn, &|| false)?;
    let spans: AnchorSpansResult = call(url, "oss_anchorspans", from(&[entry, target])).await?;
    assert_eq!((spans.epoch, spans.target), (1, anchor_hex(target)));
    assert_eq!((spans.chunks.len(), spans.start), (1, vec![Some(0), None]));
    let chunk = pcd_from_hex::<pool::AnchorSpan>(&spans.chunks[0])?;
    assert!(verify_pcd(&chunk)?);
    assert_eq!(*chunk.data(), (entry, target));
    Ok(())
}

#[tokio::test]
async fn transport_negotiates_gzip_and_enforces_auth_and_the_body_limit() -> Result<()> {
    let mut blocks = vec![genesis()];
    populate_epoch(&mut blocks, 1)?;
    let node = FakeNode::start(blocks)?;
    let db = TempDb::new();
    let node_rpc = Node::new(&node.url, None)?;
    Follower::new(store::open(&db.0)?, node_rpc, None, None)
        .await?
        .sync()
        .await?;
    let cookie_path = db.0.with_extension("cookie");
    let token = server::write_cookie(&cookie_path)?;
    let cookie = std::fs::read_to_string(&cookie_path)?;
    assert_eq!(cookie, format!("__cookie__:{token}"));
    let mode = std::fs::metadata(&cookie_path)?.permissions().mode();
    std::fs::remove_file(&cookie_path)?;
    assert_eq!(mode & 0o777, 0o600);
    let url = serve(&db.0, Some(&token), 0).await?;

    let request =
        json!({"jsonrpc": "2.0", "id": 7, "method": "oss_block", "params": {"height": 9}});
    let plain = Client::builder().no_gzip().build()?;
    let post = |client: &Client| client.post(&url).json(&request);
    assert_eq!(
        post(&plain).send().await?.status(),
        StatusCode::UNAUTHORIZED
    );
    let wrong = post(&plain).basic_auth("__cookie__", Some("wrong"));
    assert_eq!(wrong.send().await?.status(), StatusCode::UNAUTHORIZED);

    // The cookie file's credentials, sent as devtool's client sends them.
    let (user, password) = cookie.split_once(':').unwrap();
    let authed = |client: &Client| post(client).basic_auth(user, Some(password));
    let response = authed(&plain).send().await?;
    assert_eq!(response.headers().get(CONTENT_ENCODING), None);
    let response = authed(&plain)
        .header(ACCEPT_ENCODING, "gzip")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_ENCODING], "gzip");
    // reqwest's `gzip` feature negotiates and decodes it.
    let body: Value = authed(&Client::new()).send().await?.json().await?;
    assert_eq!(
        (&body["id"], &body["result"]["height"]),
        (&json!(7), &json!(9))
    );

    let pad = "x".repeat(1 << 20);
    let big = json!({"jsonrpc": "2.0", "id": 1, "method": "oss_info", "params": {"pad": pad}});
    let response = plain
        .post(&url)
        .basic_auth(user, Some(password))
        .json(&big)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

/// The whole process: three epochs, a reorg across a sealed epoch boundary,
/// then a restart on the same database.
#[tokio::test]
async fn three_epochs_a_reorg_across_a_sealed_boundary_and_a_restart() -> Result<()> {
    let mut blocks = vec![genesis()];
    populate_epoch(&mut blocks, 2)?;
    mine_to_epoch(&mut blocks, 1)?;
    populate_epoch(&mut blocks, 3)?;
    mine_to_epoch(&mut blocks, 2)?;
    for _ in 0..3 {
        mine(&mut blocks)?;
    }
    let node = FakeNode::start(blocks.clone())?;
    let db = TempDb::new();
    let (url, oss) = spawn_oss(&node.url, &db.0)?;
    oss.wait_built(blocks.last().unwrap().height)?;
    oss.wait_ready(1)?;
    let boundary = first(&blocks, 2);
    let sealed: EpochResult = call(&url, "oss_epoch", json!({"epoch": 1})).await?;
    assert_eq!(sealed.closing_hash, Some(blocks[boundary - 1].hash.clone()));
    assert_eq!(sealed.evidence.status, EvidenceStatus::Ready);

    // Two blocks below epoch 2's first block, after epoch 1 is sealed: new
    // stamps in epoch 1, then on into epoch 3.
    let mut branch = blocks[..boundary - 2].to_vec();
    let fork = branch.last().unwrap().height + 1;
    populate_epoch(&mut branch, 4)?;
    mine_to_epoch(&mut branch, 3)?;
    populate_epoch(&mut branch, 1)?;
    node.reorg(fork, branch.split_off(boundary - 2));
    let chain = node.data().blocks.clone();
    let tip = chain.last().unwrap();
    oss.wait_built(tip.height)?;
    oss.wait_ready(2)?;

    let info: InfoResult = call(&url, "oss_info", json!({})).await?;
    assert_eq!(
        (&info.tip.hash, info.evidence_ready_through),
        (&tip.hash, Some(2))
    );
    let resealed: EpochResult = call(&url, "oss_epoch", json!({"epoch": 1})).await?;
    assert_eq!(
        resealed.closing_hash,
        Some(chain[boundary - 1].hash.clone())
    );
    assert_ne!(resealed.closing_hash, sealed.closing_hash);
    // Same R0; 3 old values and 4 new ones.
    assert_eq!(resealed.evidence.discriminant, sealed.evidence.discriminant);
    let root = BucketProfile {
        profile: QrProfile::ROOT,
        members: 7,
    };
    assert_eq!(resealed.evidence.profiles, Some(vec![root]));
    let params = json!({"epoch": 1, "profile": {"depth": 0, "bits": 0}});
    let bucket: BucketResult = call(&url, "oss_bucket", params).await?;
    let pcd = pcd_from_hex::<qr::QrBucket>(&bucket.pcd)?;
    assert!(verify_pcd(&pcd)?);
    let entries = (chain[first(&chain, 1)].entry()?, chain[boundary].entry()?);
    assert_eq!((pcd.data().1, pcd.data().2), entries);

    let from = |a| json!({"from": [anchor_hex(a)]});
    let spans: AnchorSpansResult = call(
        &url,
        "oss_anchorspans",
        from(chain[first(&chain, 3)].entry()?),
    )
    .await?;
    assert_eq!((spans.epoch, &spans.target), (3, &tip.anchor_after));
    let reorged = from(blocks[boundary].entry()?);
    assert_eq!(
        code(&url, "oss_anchorspans", reorged).await?,
        Unavailable.code()
    );
    let conn = store::open_read(&db.0)?;
    assert_eq!(store::meta::<u32>(&conn, "rewind_from")?, None);
    let sealed_rows = || -> Result<u32> {
        Ok(conn.query_row("SELECT COUNT(*) FROM evidence", [], |r| r.get(0))?)
    };
    // Epochs 0 and 2, and epoch 1 under both closing hashes.
    assert_eq!(sealed_rows()?, 4);

    // Restart: the archive, evidence and spans are served at once.
    drop(oss);
    let (url, oss) = spawn_oss(&node.url, &db.0)?;
    let restarted: InfoResult = call(&url, "oss_info", json!({})).await?;
    assert_eq!(restarted.tip, info.tip);
    assert_eq!(restarted.evidence_ready_through, Some(2));
    let again: AnchorSpansResult = call(
        &url,
        "oss_anchorspans",
        from(chain[first(&chain, 3)].entry()?),
    )
    .await?;
    assert_eq!(again, spans);
    // The builder resumes from its cursor and seals nothing again.
    populate_epoch(&mut node.data().blocks, 1)?;
    let tip = node.data().blocks.last().unwrap().clone();
    oss.wait_built(tip.height)?;
    let spans: AnchorSpansResult = call(
        &url,
        "oss_anchorspans",
        from(chain[first(&chain, 3)].entry()?),
    )
    .await?;
    assert_eq!(spans.target, tip.anchor_after);
    assert_eq!(sealed_rows()?, 4);
    Ok(())
}
