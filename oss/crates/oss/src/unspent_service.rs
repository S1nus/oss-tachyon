//! `oss_unspent` (design §3 step 3, §4.2, §4.4, §4.5): validate, answer
//! `Spent` from the archive's tachygram index, then prove one segment.

use anyhow::{Context, Result, bail};
use oss_core::{
    MAX_UNSPENT_NULLIFIERS, SpentData, UnspentParams, UnspentResult, anchor_hex, bytes32,
    pcd_to_hex, profile_of, prove_unspent,
};
use rusqlite::Connection;
use udon::field::Field;
use zcash_tachyon::{QrDiscriminant, QrProfile, nullifier::Nullifier, pasta::Fp};

use crate::{
    archive, epochs,
    server::{Failure, sealed},
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

/// Proves the job: `n` steps, each epoch's bucket read under the closing hash
/// [`check`] pinned. Slow; run it on a blocking thread, under the job limit.
pub fn prove(conn: &Connection, job: &Job) -> Result<UnspentResult> {
    let segment = prove_unspent(job.epoch_start, &job.nfs, |epoch, value| {
        let (closing, r0, profiles) = &job.evidence[usize::try_from(epoch - job.epoch_start)?];
        let profile = profile_of(value, *r0, profiles).context("no sealed bucket on the path")?;
        epochs::bucket(conn, epoch, closing, profile)?.context("sealed bucket is missing")
    })?;
    let (anchor_start, epoch_start, epoch_next, anchor_next) = *segment.data();
    Ok(UnspentResult {
        pcd: pcd_to_hex(&segment)?,
        epoch_start: epoch_start.into(),
        epoch_next: epoch_next.into(),
        anchor_start: anchor_hex(anchor_start),
        anchor_next: anchor_hex(anchor_next),
    })
}

fn value(hex: &str) -> Option<Fp> {
    Fp::from_bytes(bytes32(hex).ok()?).filter(|v| *v != Fp::ZERO)
}
