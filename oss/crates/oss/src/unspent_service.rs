//! `oss_unspent` (design §3 step 3, §4.2, §4.4, §4.5): validate, answer
//! `Spent` from the archive's tachygram index, then prove one segment.
//!
//! Checked jobs from every client go through one [`Batcher`]. It proves the
//! jobs queued together as one lockstep batch ([`prove_unspent_batch`]),
//! reading each sealed bucket the batch needs once, and still answers each
//! client with its own segment.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    num::NonZeroUsize,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use oss_core::{
    MAX_UNSPENT_NULLIFIERS, Pcd, SpentData, UnspentJob, UnspentParams, UnspentResult, anchor_hex,
    bytes32, pcd_to_hex, profile_of, prove_unspent, prove_unspent_batch,
};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};
use udon::field::Field;
use zcash_tachyon::{
    QrDiscriminant, QrProfile,
    nullifier::Nullifier,
    pasta::Fp,
    stamp::proof::{pool, qr::QrBucket},
};

use crate::{
    archive, epochs,
    server::{Failure, sealed},
    store,
};

/// A checked request: one sealed epoch per nullifier, each pinned to the
/// closing hash it was checked under.
pub struct Job {
    epoch_start: u32,
    nfs: Vec<Nullifier>,
    /// `(closing_hash, R0, profiles)` per epoch.
    evidence: Vec<(String, QrDiscriminant, Vec<QrProfile>)>,
}

/// Every check, in one read: the values, every epoch sealed under its
/// canonical closing hash (else `NotReady`) and inside the archive (else
/// `Unavailable`), then `Spent` if any value is published. The index catches a
/// value published in `e − 1` as the "next" nullifier, which `prove_unspent`
/// would prove absent from `e`.
pub fn check(conn: &Connection, params: &UnspentParams) -> Result<Job> {
    let count = params.nullifiers.len();
    if !(1..=MAX_UNSPENT_NULLIFIERS).contains(&count) {
        bail!(Failure::InvalidParams(format!(
            "expected 1 to {MAX_UNSPENT_NULLIFIERS} nullifiers, got {count}"
        )));
    }
    let values = params
        .nullifiers
        .iter()
        .enumerate()
        .map(|(i, hex)| {
            value(hex).ok_or_else(|| {
                Failure::InvalidParams(format!(
                    "nullifiers[{i}] is not a 32-byte canonical nonzero field element"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let epochs = params.epoch_start
        ..u32::try_from(count)
            .ok()
            .and_then(|n| params.epoch_start.checked_add(n))
            .ok_or_else(|| Failure::InvalidParams("epoch overflow".into()))?;

    let tx = conn.unchecked_transaction()?;
    let mut evidence = Vec::with_capacity(count);
    for epoch in epochs {
        let (closing, profiles) = sealed(&tx, epoch)?;
        let r0 = epochs::discriminant(&tx, epoch)?.context("a sealed epoch has R0")?;
        evidence.push((closing, r0, profiles.iter().map(|p| p.profile).collect()));
    }
    if let Some(&(_, height, stamp)) = archive::tachygram_heights(&tx, &values)?.first() {
        let block = archive::block(&tx, height)?.context("indexed block is archived")?;
        let txid = block
            .stamps
            .get(stamp)
            .context("indexed stamp exists")?
            .txid
            .clone();
        bail!(Failure::Spent(SpentData {
            epoch: block.epoch,
            height,
            txid,
        }));
    }
    Ok(Job {
        epoch_start: params.epoch_start,
        nfs: values.into_iter().map(Nullifier::from).collect(),
        evidence,
    })
}

/// Proves the job alone: `n` steps, each epoch's bucket read under the closing hash
/// [`check`] pinned. Slow; run it on a blocking thread, under the job limit.
pub fn prove(conn: &Connection, job: &Job) -> Result<UnspentResult> {
    let segment = prove_unspent(job.epoch_start, &job.nfs, |epoch, value| {
        let (closing, r0, profiles) = &job.evidence[usize::try_from(epoch - job.epoch_start)?];
        let profile = profile_of(value, *r0, profiles).context("no sealed bucket on the path")?;
        epochs::bucket(conn, epoch, closing, profile)?.context("sealed bucket is missing")
    })?;
    result(&segment)
}

fn result(segment: &Pcd<pool::ArbitraryUnspent>) -> Result<UnspentResult> {
    let (anchor_start, epoch_start, epoch_next, anchor_next) = *segment.data();
    Ok(UnspentResult {
        pcd: pcd_to_hex(segment)?,
        epoch_start: epoch_start.into(),
        epoch_next: epoch_next.into(),
        anchor_start: anchor_hex(anchor_start),
        anchor_next: anchor_hex(anchor_next),
    })
}

/// A 32-byte canonical nonzero field element.
pub(crate) fn value(hex: &str) -> Option<Fp> {
    Fp::from_bytes(bytes32(hex).ok()?).filter(|v| *v != Fp::ZERO)
}

/// The bucket `job` tests its `k`-th value against.
type BucketKey = (u32, String, u32, u32);

fn bucket_key(job: &Job, k: usize) -> Result<BucketKey> {
    let (closing, r0, profiles) = &job.evidence[k];
    let epoch = job.epoch_start + u32::try_from(k)?;
    let profile =
        profile_of(job.nfs[k].into(), *r0, profiles).context("no sealed bucket on the path")?;
    Ok((epoch, closing.clone(), profile.depth, profile.bits))
}

/// Proves `jobs` as one lockstep batch on up to `threads` workers. Every
/// bucket the batch needs is read and decoded once, before proving starts.
/// Each job gets exactly the result [`prove`] would give it.
pub fn prove_batch(
    conn: &Connection,
    jobs: &[Job],
    threads: NonZeroUsize,
) -> Vec<Result<UnspentResult>> {
    let mut buckets: BTreeMap<BucketKey, Pcd<QrBucket>> = BTreeMap::new();
    let mut failed: Vec<Option<anyhow::Error>> = jobs.iter().map(|_| None).collect();
    for (i, job) in jobs.iter().enumerate() {
        for k in 0..job.nfs.len() {
            let loaded = bucket_key(job, k).and_then(|key| {
                if let Entry::Vacant(slot) = buckets.entry(key) {
                    let (epoch, closing, depth, bits) = slot.key();
                    let profile = QrProfile {
                        depth: *depth,
                        bits: *bits,
                    };
                    let pcd = epochs::bucket(conn, *epoch, closing, profile)?
                        .context("sealed bucket is missing")?;
                    slot.insert(pcd);
                }
                Ok(())
            });
            if let Err(error) = loaded {
                failed[i] = Some(error);
                break;
            }
        }
    }
    let core: Vec<UnspentJob> = jobs
        .iter()
        .zip(&failed)
        .map(|(job, failed)| UnspentJob {
            epoch_start: job.epoch_start,
            // A job whose buckets failed to load runs nothing.
            nfs: if failed.is_some() {
                Vec::new()
            } else {
                job.nfs.clone()
            },
        })
        .collect();
    let segments = prove_unspent_batch(
        &core,
        |i, epoch, _| {
            let k = usize::try_from(epoch - jobs[i].epoch_start)?;
            buckets
                .get(&bucket_key(&jobs[i], k)?)
                .cloned()
                .context("bucket was not prefetched")
        },
        threads,
    );
    segments
        .into_iter()
        .zip(failed)
        .map(|(segment, failed)| match failed {
            Some(error) => Err(error),
            None => result(&segment?),
        })
        .collect()
}

type Reply = oneshot::Sender<Result<UnspentResult>>;

/// Queues checked jobs from every client and proves them in batches: one
/// batch at a time, made of whatever queued while the last one ran (up to
/// `max_batch`), optionally waiting `window` for more.
#[derive(Clone)]
pub struct Batcher {
    tx: mpsc::Sender<(Job, Reply)>,
}

impl Batcher {
    pub fn spawn(db: PathBuf, threads: NonZeroUsize, max_batch: usize, window: Duration) -> Self {
        let (tx, rx) = mpsc::channel(max_batch.max(1) * 4);
        tokio::spawn(run(rx, db, threads, max_batch.max(1), window));
        Self { tx }
    }

    /// Proves `job` in the next batch. The proof runs on if the caller hangs up.
    pub async fn prove(&self, job: Job) -> Result<UnspentResult> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send((job, reply))
            .await
            .map_err(|_| anyhow!("the unspent batcher stopped"))?;
        answer
            .await
            .map_err(|_| anyhow!("the unspent batch was dropped"))?
    }
}

async fn run(
    mut rx: mpsc::Receiver<(Job, Reply)>,
    db: PathBuf,
    threads: NonZeroUsize,
    max_batch: usize,
    window: Duration,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + window;
        while batch.len() < max_batch {
            match rx.try_recv() {
                Ok(job) => batch.push(job),
                Err(mpsc::error::TryRecvError::Empty) if !window.is_zero() => {
                    match tokio::time::timeout_at(deadline, rx.recv()).await {
                        Ok(Some(job)) => batch.push(job),
                        _ => break,
                    }
                }
                Err(_) => break,
            }
        }
        let (jobs, replies): (Vec<Job>, Vec<Reply>) = batch.into_iter().unzip();
        let db = db.clone();
        let proved = tokio::task::spawn_blocking(move || -> Result<Vec<Result<UnspentResult>>> {
            Ok(prove_batch(&store::open_read(&db)?, &jobs, threads))
        })
        .await;
        match proved {
            Ok(Ok(results)) => {
                for (result, reply) in results.into_iter().zip(replies) {
                    let _ = reply.send(result);
                }
            }
            Ok(Err(error)) => {
                for reply in replies {
                    let _ = reply.send(Err(anyhow!("unspent batch failed: {error:#}")));
                }
            }
            Err(_) => {
                for reply in replies {
                    let _ = reply.send(Err(anyhow!("the unspent batch panicked")));
                }
            }
        }
    }
}
