//! Closed-epoch QR evidence, routed before merging to bound polynomial size.
//! Ported from devtool `proofs/epoch.rs`, without its caches.
//!
//! [`build_epoch`] routes the whole routing tree once: every internal node
//! splits each of its intakes a single time and descends both sides, so the
//! upper levels are shared by every bucket below them instead of being
//! re-proven per bucket. Independent steps (stamp seeds, the splits of one
//! level, merges inside a group, bucket seals) run as parallel batches.

use std::{
    collections::BTreeSet,
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result, ensure};
use zcash_tachyon::{
    Anchor, EpochIndex, QrClassRoot, QrDiscriminant, QrProfile, Tachygram,
    constants::EPOCH_SIZE,
    stamp::proof::{PROOF_SYSTEM, qr},
    witness,
};

pub use crate::par::default_threads;
use crate::{Pcd, TachyonBlock, anchor, max_polynomial_members, profiles, trivial};

/// Proof steps run by one [`build_epoch_with_stats`] call, by kind.
#[derive(Debug, Default)]
pub struct BuildStats {
    pub seeds: AtomicUsize,
    pub splits: AtomicUsize,
    pub selects: AtomicUsize,
    pub descends: AtomicUsize,
    pub merges: AtomicUsize,
    pub seals: AtomicUsize,
}

impl BuildStats {
    /// Every proof step counted so far.
    pub fn total(&self) -> usize {
        [
            &self.seeds,
            &self.splits,
            &self.selects,
            &self.descends,
            &self.merges,
            &self.seals,
        ]
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .sum()
    }

    fn bump(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Seals every `QrBucket` of a closed epoch: one per routing leaf of
/// [`profiles`], empty siblings included. `blocks` are the epoch's blocks plus
/// the next epoch's first block; `predecessor` is the final anchor of the
/// previous epoch (zero for epoch 0). Returns each profile, its bucket and its
/// member count, in [`profiles`] order.
pub fn build_epoch(
    blocks: &[TachyonBlock],
    predecessor: Anchor,
    r0: QrDiscriminant,
) -> Result<Vec<(QrProfile, Pcd<qr::QrBucket>, usize)>> {
    build_epoch_with_stats(
        blocks,
        predecessor,
        r0,
        default_threads(),
        &BuildStats::default(),
    )
}

/// [`build_epoch`] with an explicit worker count, recording its proof steps.
pub fn build_epoch_with_stats(
    blocks: &[TachyonBlock],
    predecessor: Anchor,
    r0: QrDiscriminant,
    threads: NonZeroUsize,
    stats: &BuildStats,
) -> Result<Vec<(QrProfile, Pcd<qr::QrBucket>, usize)>> {
    let epoch = Epoch::read(blocks)?;
    let builder = Builder { r0, threads, stats };
    let mut sealed = builder.route_all(&epoch, predecessor)?;
    // Return buckets in `profiles` order, which callers and stores rely on.
    profiles(epoch.members.clone(), r0)?
        .into_iter()
        .map(|path| {
            let index = sealed
                .iter()
                .position(|(p, _, _)| *p == path)
                .context("routing tree and profiles disagree")?;
            let (_, bucket, members) = sealed.swap_remove(index);
            let profile = path.iter().fold(QrProfile::ROOT, |p, &b| p.descend(b));
            ensure!(
                bucket.data().0 == EpochIndex::new(epoch.epoch)
                    && bucket.data().1 == epoch.start
                    && bucket.data().2 == epoch.next
                    && bucket.data().3 == r0
                    && bucket.data().4 == profile,
                "sealed bucket does not match its epoch and profile"
            );
            Ok((profile, bucket, members))
        })
        .collect()
}

/// A sealed leaf: its root-first path, bucket and member count.
type SealedLeaf = (Vec<bool>, Pcd<qr::QrBucket>, usize);

#[derive(Clone)]
struct Intake {
    proof: Pcd<qr::QrIntake>,
    members: Vec<Tachygram>,
}

struct Builder<'a> {
    r0: QrDiscriminant,
    threads: NonZeroUsize,
    stats: &'a BuildStats,
}

impl Builder<'_> {
    /// Routes the epoch's tree level by level and seals each leaf. Returns
    /// `(path, bucket, members)` for every leaf, in no particular order.
    fn route_all(&self, epoch: &Epoch, predecessor: Anchor) -> Result<Vec<SealedLeaf>> {
        let mut frontier = vec![(Vec::new(), self.coalesce(self.seed(epoch)?)?)];
        let mut leaves = Vec::new();
        while !frontier.is_empty() {
            let (internal, leaf): (Vec<_>, Vec<_>) =
                frontier
                    .into_iter()
                    .partition(|(_, intakes): &(Vec<bool>, Vec<Intake>)| {
                        intakes.iter().map(|i| i.members.len()).sum::<usize>()
                            > max_polynomial_members()
                    });
            leaves.extend(leaf);
            for (path, _) in &internal {
                ensure!(
                    path.len() < QrProfile::MAX_DEPTH,
                    "epoch cannot be routed within Tachyon's polynomial capacity"
                );
            }
            // One split per intake of this level, all independent.
            let jobs: Vec<(usize, Intake)> = internal
                .iter()
                .enumerate()
                .flat_map(|(node, (_, intakes))| intakes.iter().cloned().map(move |i| (node, i)))
                .collect();
            let halves = self.par_map(jobs, |(node, intake)| Ok((node, self.split(intake)?)))?;
            let mut children: Vec<[Vec<Intake>; 2]> =
                internal.iter().map(|_| [Vec::new(), Vec::new()]).collect();
            // `par_map` keeps order, so each child keeps its anchor order.
            for (node, [no, yes]) in halves {
                children[node][0].push(no);
                children[node][1].push(yes);
            }
            let next: Vec<(Vec<bool>, Vec<Intake>)> = internal
                .into_iter()
                .zip(children)
                .flat_map(|((path, _), sides)| {
                    sides
                        .into_iter()
                        .zip([false, true])
                        .map(move |(intakes, side)| {
                            let mut child = path.clone();
                            child.push(side);
                            (child, intakes)
                        })
                })
                .collect();
            frontier = next
                .into_iter()
                .map(|(path, intakes)| Ok((path, self.coalesce(intakes)?)))
                .collect::<Result<_>>()?;
        }
        self.par_map(leaves, |(path, intakes)| {
            let Intake { proof, members } =
                self.merge_all(intakes)?.context("missing epoch intake")?;
            Self::bump(&self.stats.seals);
            let (bucket, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                qr::QrBucketSeal,
                witness::qr_bucket_seal((*proof.data(), ()), predecessor),
                proof,
                trivial(),
            )?;
            Ok((path, bucket, members.len()))
        })
    }

    /// One intake per stamp, in anchor order; one empty intake if none.
    fn seed(&self, epoch: &Epoch) -> Result<Vec<Intake>> {
        let index = EpochIndex::new(epoch.epoch);
        if epoch.stamps.is_empty() {
            Self::bump(&self.stats.seeds);
            let (proof, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrEmptyIntakeSeed,
                witness::qr_empty_intake_seed(((), ()), epoch.start, index, self.r0),
            )?;
            return Ok(vec![Intake {
                proof,
                members: Vec::new(),
            }]);
        }
        self.par_map(epoch.stamps.clone(), |(before, members)| {
            Self::bump(&self.stats.seeds);
            let (proof, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrStampIntakeSeed,
                witness::qr_stamp_intake_seed(((), ()), before, index, self.r0, &members),
            )?;
            Ok(Intake { proof, members })
        })
    }

    /// Merges adjacent intakes into as few capacity-bounded groups as a
    /// greedy left-to-right pass allows. Empty intakes always join a
    /// neighbour, so `QrIntakeSplit` never sees one unless the node is empty.
    /// Groups are found from member counts first, then merged in parallel.
    fn coalesce(&self, intakes: Vec<Intake>) -> Result<Vec<Intake>> {
        let mut groups: Vec<Vec<Intake>> = Vec::new();
        let mut size = 0;
        for intake in intakes {
            match groups.last_mut() {
                Some(group) if size + intake.members.len() <= max_polynomial_members() => {
                    size += intake.members.len();
                    group.push(intake);
                }
                _ => {
                    size = intake.members.len();
                    groups.push(vec![intake]);
                }
            }
        }
        if groups.len() == 1 {
            // A single group is merged by `merge_all`'s own parallel rounds.
            return Ok(self
                .merge_all(groups.pop().expect("one group"))?
                .into_iter()
                .collect());
        }
        self.par_map(groups, |group| {
            Ok(self
                .merge_all_serial(group)?
                .expect("groups are never empty"))
        })
    }

    /// Balanced adjacent merges, each round in parallel. Order is preserved.
    fn merge_all(&self, mut intakes: Vec<Intake>) -> Result<Option<Intake>> {
        while intakes.len() > 1 {
            let mut pairs = Vec::new();
            let mut it = intakes.into_iter();
            while let Some(left) = it.next() {
                pairs.push((left, it.next()));
            }
            intakes = self.par_map(pairs, |(left, right)| match right {
                Some(right) => self.merge(left, right),
                None => Ok(left),
            })?;
        }
        Ok(intakes.pop())
    }

    /// [`Self::merge_all`] on the calling thread, for use inside a batch.
    fn merge_all_serial(&self, mut intakes: Vec<Intake>) -> Result<Option<Intake>> {
        while intakes.len() > 1 {
            let mut merged = Vec::new();
            let mut it = intakes.into_iter();
            while let Some(left) = it.next() {
                merged.push(match it.next() {
                    Some(right) => self.merge(left, right)?,
                    None => left,
                });
            }
            intakes = merged;
        }
        Ok(intakes.pop())
    }

    fn merge(&self, left: Intake, right: Intake) -> Result<Intake> {
        ensure!(
            left.members.len() + right.members.len() <= max_polynomial_members(),
            "routed epoch bucket exceeds Tachyon's polynomial capacity"
        );
        Self::bump(&self.stats.merges);
        let (proof, ()) = PROOF_SYSTEM.fuse(
            &mut rand::rng(),
            qr::QrIntakeMerge,
            witness::qr_intake_merge(
                (*left.proof.data(), *right.proof.data()),
                &left.members,
                &right.members,
            ),
            left.proof,
            right.proof,
        )?;
        let mut members = left.members;
        members.extend(right.members);
        Ok(Intake { proof, members })
    }

    /// Splits one intake once and descends both sides: `[false, true]`.
    fn split(&self, intake: Intake) -> Result<[Intake; 2]> {
        let depth = intake.proof.data().4.depth;
        Self::bump(&self.stats.splits);
        let (sides, ()) = PROOF_SYSTEM.fuse(
            &mut rand::rng(),
            qr::QrIntakeSplit,
            witness::qr_intake_split((*intake.proof.data(), ()), &intake.members),
            intake.proof,
            trivial(),
        )?;
        let shift = self.r0.at(depth);
        let descend = |side: bool| -> Result<Intake> {
            Self::bump(&self.stats.selects);
            let (selection, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                qr::QrSideSelect,
                witness::qr_side_select((*sides.data(), ()), &intake.members, side),
                sides.clone(),
                trivial(),
            )?;
            Self::bump(&self.stats.descends);
            let (proof, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                qr::QrSideDescend,
                witness::qr_side_descend((*sides.data(), *selection.data()), side),
                sides.clone(),
                selection,
            )?;
            let members = intake
                .members
                .iter()
                .copied()
                .filter(|member| QrClassRoot::of((*member).into(), shift).0 == side)
                .collect();
            Ok(Intake { proof, members })
        };
        Ok([descend(false)?, descend(true)?])
    }

    fn bump(counter: &AtomicUsize) {
        BuildStats::bump(counter);
    }

    fn par_map<T, R, F>(&self, items: Vec<T>, f: F) -> Result<Vec<R>>
    where
        T: Send,
        R: Send,
        F: Fn(T) -> Result<R> + Sync,
    {
        crate::par::par_map(self.threads, items, f)
    }
}

struct Epoch {
    epoch: u32,
    stamps: Vec<(Anchor, Vec<Tachygram>)>,
    members: Vec<Tachygram>,
    start: Anchor,
    next: Anchor,
}

impl Epoch {
    fn read(blocks: &[TachyonBlock]) -> Result<Self> {
        let first = blocks.first().context("missing epoch inputs")?;
        let epoch = first.epoch;
        let next_epoch = epoch.checked_add(1).context("epoch overflow")?;
        let next = blocks
            .iter()
            .find(|b| b.epoch == next_epoch)
            .context("epoch has not closed")?;
        ensure!(
            first.pool_height.is_multiple_of(EPOCH_SIZE),
            "missing epoch start"
        );
        ensure!(
            next.pool_height.is_multiple_of(EPOCH_SIZE),
            "missing next epoch entry"
        );
        let mut stamps = Vec::new();
        let mut members = BTreeSet::new();
        let mut previous: Option<&TachyonBlock> = None;
        for block in blocks.iter().take_while(|b| b.height <= next.height) {
            // A partial epoch must never be sealed as complete, so check
            // contiguous coverage through the closing sentinel. Callers have
            // already run `chain::validate` on every block.
            if let Some(previous) = previous {
                ensure!(
                    previous.height.checked_add(1) == Some(block.height),
                    "chain gap"
                );
                ensure!(
                    previous.hash == block.previous_block_hash,
                    "chain reorganized while sealing"
                );
                ensure!(
                    previous.anchor_after == block.anchor_before,
                    "anchor discontinuity"
                );
            }
            previous = Some(block);
            if block.epoch != epoch {
                break;
            }
            let mut cursor = block.entry()?;
            for stamp in &block.stamps {
                let values = stamp.members()?;
                for value in &values {
                    ensure!(members.insert(*value), "duplicate tachygrams in an epoch");
                }
                stamps.push((cursor, values));
                cursor = cursor.next_stamp(EpochIndex::new(epoch), &stamp.commitment()?)?;
            }
            ensure!(
                cursor == anchor(&block.anchor_after)?,
                "stamps do not reproduce the block anchor"
            );
        }
        Ok(Self {
            epoch,
            stamps,
            members: members.into_iter().collect(),
            start: first.entry()?,
            next: next.entry()?,
        })
    }
}

/// The original per-bucket builder, kept as the oracle for [`build_epoch`]:
/// it re-routes every chunk from the root for each bucket.
#[cfg(any(test, feature = "testing"))]
pub mod reference {
    use super::*;

    /// Step counts and buckets as the pre-sharing implementation produced.
    pub fn build_epoch(
        blocks: &[TachyonBlock],
        predecessor: Anchor,
        r0: QrDiscriminant,
        stats: &BuildStats,
    ) -> Result<Vec<(QrProfile, Pcd<qr::QrBucket>, usize)>> {
        let epoch = Epoch::read(blocks)?;
        let chunks = chunks(&epoch, r0, stats)?;
        profiles(epoch.members.clone(), r0)?
            .into_iter()
            .map(|path| {
                let profile = path.iter().fold(QrProfile::ROOT, |p, &b| p.descend(b));
                let (bucket, members) = build(&epoch, &path, &chunks, predecessor, r0, stats)?;
                Ok((profile, bucket, members))
            })
            .collect()
    }

    pub(super) fn route(
        mut intake: Intake,
        path: &[bool],
        discriminant: QrDiscriminant,
        stats: &BuildStats,
    ) -> Result<Intake> {
        for &side in path {
            let depth = intake.proof.data().4.depth;
            BuildStats::bump(&stats.splits);
            let (sides, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                qr::QrIntakeSplit,
                witness::qr_intake_split((*intake.proof.data(), ()), &intake.members),
                intake.proof,
                trivial(),
            )?;
            BuildStats::bump(&stats.selects);
            let (selection, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                qr::QrSideSelect,
                witness::qr_side_select((*sides.data(), ()), &intake.members, side),
                sides.clone(),
                trivial(),
            )?;
            BuildStats::bump(&stats.descends);
            intake.proof = PROOF_SYSTEM
                .fuse(
                    &mut rand::rng(),
                    qr::QrSideDescend,
                    witness::qr_side_descend((*sides.data(), *selection.data()), side),
                    sides,
                    selection,
                )?
                .0;
            let shift = discriminant.at(depth);
            intake
                .members
                .retain(|member| QrClassRoot::of((*member).into(), shift).0 == side);
        }
        Ok(intake)
    }

    pub(super) fn merge(mut left: Intake, right: Intake, stats: &BuildStats) -> Result<Intake> {
        ensure!(
            left.members.len() + right.members.len() <= max_polynomial_members(),
            "routed epoch bucket exceeds Tachyon's polynomial capacity"
        );
        BuildStats::bump(&stats.merges);
        left.proof = PROOF_SYSTEM
            .fuse(
                &mut rand::rng(),
                qr::QrIntakeMerge,
                witness::qr_intake_merge(
                    (*left.proof.data(), *right.proof.data()),
                    &left.members,
                    &right.members,
                ),
                left.proof,
                right.proof,
            )?
            .0;
        left.members.extend(right.members);
        Ok(left)
    }

    pub(super) fn chunks(
        epoch: &Epoch,
        discriminant: QrDiscriminant,
        stats: &BuildStats,
    ) -> Result<Vec<Intake>> {
        let index = EpochIndex::new(epoch.epoch);
        let mut chunks = Vec::new();
        let mut current: Option<Intake> = None;
        for (before, members) in epoch.stamps.iter().cloned() {
            BuildStats::bump(&stats.seeds);
            let (proof, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrStampIntakeSeed,
                witness::qr_stamp_intake_seed(((), ()), before, index, discriminant, &members),
            )?;
            let intake = Intake { proof, members };
            current = match current.take() {
                Some(left)
                    if left.members.len() + intake.members.len() <= max_polynomial_members() =>
                {
                    Some(merge(left, intake, stats)?)
                }
                Some(chunk) => {
                    chunks.push(chunk);
                    Some(intake)
                }
                None => Some(intake),
            };
        }
        chunks.extend(current);
        if chunks.is_empty() {
            BuildStats::bump(&stats.seeds);
            let (proof, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrEmptyIntakeSeed,
                witness::qr_empty_intake_seed(((), ()), epoch.start, index, discriminant),
            )?;
            chunks.push(Intake {
                proof,
                members: Vec::new(),
            });
        }
        Ok(chunks)
    }

    pub(super) fn build(
        epoch: &Epoch,
        path: &[bool],
        chunks: &[Intake],
        predecessor: Anchor,
        discriminant: QrDiscriminant,
        stats: &BuildStats,
    ) -> Result<(Pcd<qr::QrBucket>, usize)> {
        let mut intakes = chunks.to_vec();
        for &side in path {
            let mut joined: Vec<Intake> = Vec::new();
            for right in intakes {
                match joined.pop() {
                    Some(left) if left.members.is_empty() || right.members.is_empty() => {
                        joined.push(merge(left, right, stats)?);
                    }
                    Some(left) => joined.extend([left, right]),
                    None => joined.push(right),
                }
            }
            intakes = joined
                .into_iter()
                .map(|intake| route(intake, &[side], discriminant, stats))
                .collect::<Result<_>>()?;
        }
        while intakes.len() > 1 {
            let mut merged = Vec::new();
            let mut pairs = intakes.into_iter();
            while let Some(left) = pairs.next() {
                merged.push(match pairs.next() {
                    Some(right) => merge(left, right, stats)?,
                    None => left,
                });
            }
            intakes = merged;
        }
        let Intake { proof, members } = intakes.pop().context("missing epoch intake")?;
        BuildStats::bump(&stats.seals);
        let (bucket, ()) = PROOF_SYSTEM.fuse(
            &mut rand::rng(),
            qr::QrBucketSeal,
            witness::qr_bucket_seal((*proof.data(), ()), predecessor),
            proof,
            trivial(),
        )?;
        ensure!(
            bucket.data().2 == epoch.next,
            "epoch evidence does not meet the next entry anchor"
        );
        Ok((bucket, members.len()))
    }
}

/// Synthetic chains for tests, ported from devtool `src/tests.rs`.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::collections::BTreeSet;

    use anyhow::{Context, Result};
    use udon::field::Field;
    use zcash_tachyon::{
        Anchor, EpochIndex, Tachygram, TachygramSetPoly, constants::EPOCH_SIZE, pasta::Fp,
    };

    use crate::{TachyonBlock, TachyonStamp, anchor, anchor_hex};

    const ACTIVATION: u32 = 8;

    pub fn genesis() -> TachyonBlock {
        let anchor = anchor_hex(Anchor::default());
        TachyonBlock {
            hash: hex::encode([1; 32]),
            previous_block_hash: hex::encode([0; 32]),
            height: ACTIVATION,
            activation_height: ACTIVATION,
            pool_height: 0,
            epoch: 0,
            epoch_length: EPOCH_SIZE,
            anchor_before: anchor.clone(),
            epoch_start_anchor: Some(anchor.clone()),
            anchor_after: anchor,
            stamps: vec![],
            transactions: vec![],
        }
    }

    /// Appends an empty block.
    pub fn mine(blocks: &mut Vec<TachyonBlock>) -> Result<()> {
        let parent = blocks.last().expect("chain starts at genesis");
        let height = parent.height + 1;
        let pool_height = height - ACTIVATION;
        let epoch = pool_height / EPOCH_SIZE;
        let before = anchor(&parent.anchor_after)?;
        let entry = if epoch != parent.epoch {
            before.next_epoch(EpochIndex::new(epoch))?
        } else {
            before
        };
        let block = TachyonBlock {
            hash: format!("{height:064x}"),
            previous_block_hash: parent.hash.clone(),
            height,
            activation_height: ACTIVATION,
            pool_height,
            epoch,
            epoch_length: EPOCH_SIZE,
            anchor_before: parent.anchor_after.clone(),
            epoch_start_anchor: (epoch != parent.epoch).then(|| anchor_hex(entry)),
            anchor_after: anchor_hex(entry),
            stamps: vec![],
            transactions: vec![],
        };
        blocks.push(block);
        Ok(())
    }

    /// Mines until the tip is the first block of `epoch`.
    pub fn mine_to_epoch(blocks: &mut Vec<TachyonBlock>, epoch: u32) -> Result<()> {
        while blocks.last().expect("chain starts at genesis").epoch < epoch {
            mine(blocks)?;
        }
        Ok(())
    }

    /// Mines a block with `count` distinct tachygrams, new to its epoch, in
    /// stamps of 64. Each stamp fits a polynomial; their union need not.
    pub fn populate_epoch(blocks: &mut Vec<TachyonBlock>, count: usize) -> Result<()> {
        mine(blocks)?;
        let mut block = blocks.pop().expect("just mined");
        let epoch = EpochIndex::new(block.epoch);
        let mut cursor = block.entry()?;
        let existing: BTreeSet<_> = blocks
            .iter()
            .filter(|b| b.epoch == block.epoch)
            .flat_map(|b| &b.stamps)
            .map(TachyonStamp::members)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let members: Vec<_> = (1u64..)
            .map(|i| Tachygram::from(Fp::from(i)))
            .filter(|member| !existing.contains(member))
            .take(count)
            .collect();
        for (index, members) in members.chunks(64).enumerate() {
            let commitment = members
                .iter()
                .copied()
                .collect::<TachygramSetPoly>()
                .commit();
            cursor = cursor.next_stamp(epoch, &commitment)?;
            let mut bytes = Vec::new();
            commitment.write(&mut bytes)?;
            block.stamps.push(TachyonStamp {
                transaction_index: index,
                txid: format!("{index:064x}"),
                tachygram_set: hex::encode(bytes),
                tachygrams: members
                    .iter()
                    .map(|m| hex::encode(Fp::from(*m).to_bytes()))
                    .collect(),
            });
        }
        block.anchor_after = anchor_hex(cursor);
        blocks.push(block);
        Ok(())
    }

    /// Mines a block with one stamp of exactly `members`, e.g. a note's `cm`
    /// or a spend's published nullifiers.
    pub fn mine_stamp(blocks: &mut Vec<TachyonBlock>, members: &[Tachygram]) -> Result<()> {
        mine(blocks)?;
        let block = blocks.last_mut().expect("just mined");
        let commitment = members
            .iter()
            .copied()
            .collect::<TachygramSetPoly>()
            .commit();
        let after = block
            .entry()?
            .next_stamp(EpochIndex::new(block.epoch), &commitment)?;
        let mut bytes = Vec::new();
        commitment.write(&mut bytes)?;
        block.stamps.push(TachyonStamp {
            transaction_index: 0,
            txid: format!("{:064x}", block.height),
            tachygram_set: hex::encode(bytes),
            tachygrams: members
                .iter()
                .map(|m| hex::encode(Fp::from(*m).to_bytes()))
                .collect(),
        });
        block.anchor_after = anchor_hex(after);
        Ok(())
    }

    /// The epoch's blocks plus the next epoch's first block, and its
    /// predecessor anchor: what `build_epoch` takes.
    pub fn epoch_inputs(
        blocks: &[TachyonBlock],
        epoch: u32,
    ) -> Result<(Vec<TachyonBlock>, Anchor)> {
        let start = blocks
            .iter()
            .position(|b| b.epoch == epoch)
            .context("missing epoch")?;
        let end = blocks
            .iter()
            .position(|b| b.epoch > epoch)
            .context("epoch has not closed")?;
        let inputs = blocks[start..=end].to_vec();
        let predecessor = if epoch == 0 {
            Anchor(Fp::ZERO)
        } else {
            anchor(&inputs[0].anchor_before)?
        };
        Ok((inputs, predecessor))
    }
}

#[cfg(test)]
mod tests {
    use udon::field::Field;
    use zcash_tachyon::{TachygramSetPoly, pasta::Fp};

    use super::{testing::*, *};
    use crate::{profile_of, verify_pcd};

    /// Every bucket verifies, spans `[entry(e), entry(e+1))` under `r0`, and
    /// commits to exactly the published tachygrams `profile_of` routes to it.
    fn check_epoch(blocks: &[TachyonBlock], epoch: u32, r0: QrDiscriminant) -> Result<usize> {
        let (inputs, predecessor) = epoch_inputs(blocks, epoch)?;
        let buckets = build_epoch(&inputs, predecessor, r0)?;
        let leaves: Vec<_> = buckets.iter().map(|(p, _, _)| *p).collect();
        let published: Vec<Tachygram> = inputs
            .iter()
            .filter(|b| b.epoch == epoch)
            .flat_map(|b| &b.stamps)
            .map(|s| s.members())
            .collect::<Result<Vec<_>>>()?
            .concat();
        let entry = |e| -> Result<Anchor> {
            blocks
                .iter()
                .find(|b| b.epoch == e)
                .context("missing epoch")?
                .entry()
        };
        for (profile, bucket, members) in &buckets {
            let held: Vec<_> = published
                .iter()
                .copied()
                .filter(|tg| profile_of((*tg).into(), r0, &leaves) == Some(*profile))
                .collect();
            let data = bucket.data();
            assert_eq!(data.0, EpochIndex::new(epoch));
            assert_eq!(data.1, entry(epoch)?);
            assert_eq!(data.2, entry(epoch + 1)?);
            assert_eq!(data.3, r0);
            assert_eq!(*members, held.len());
            let commitment = held.into_iter().collect::<TachygramSetPoly>().commit();
            assert_eq!(data.5, commitment.bridge()?);
            assert!(verify_pcd(bucket)?);
        }
        assert!(
            published
                .iter()
                .all(|tg| profile_of((*tg).into(), r0, &leaves).is_some())
        );
        assert_eq!(buckets.iter().map(|b| b.2).sum::<usize>(), published.len());
        Ok(buckets.len())
    }

    /// Builds `epoch` with both builders and checks they seal the same
    /// buckets: same profiles, member counts and headers. Returns the step
    /// totals `(shared, reference)`.
    fn compare_with_reference(
        blocks: &[TachyonBlock],
        epoch: u32,
        r0: QrDiscriminant,
    ) -> Result<(usize, usize)> {
        let (inputs, predecessor) = epoch_inputs(blocks, epoch)?;
        let shared_stats = BuildStats::default();
        let shared =
            build_epoch_with_stats(&inputs, predecessor, r0, default_threads(), &shared_stats)?;
        let reference_stats = BuildStats::default();
        let reference = reference::build_epoch(&inputs, predecessor, r0, &reference_stats)?;
        assert_eq!(shared.len(), reference.len());
        for ((p1, b1, m1), (p2, b2, m2)) in shared.iter().zip(&reference) {
            assert_eq!(p1, p2);
            assert_eq!(m1, m2);
            assert_eq!(b1.data(), b2.data());
            assert!(verify_pcd(b1)?);
        }
        Ok((shared_stats.total(), reference_stats.total()))
    }

    #[test]
    fn shared_routing_seals_the_same_buckets_as_per_bucket_routing() -> Result<()> {
        let mut blocks = vec![genesis()];
        mine_to_epoch(&mut blocks, 1)?;
        populate_epoch(&mut blocks, 3 * max_polynomial_members() + 101)?;
        mine_to_epoch(&mut blocks, 2)?;
        for r0 in [
            QrDiscriminant(Fp::ONE),
            QrDiscriminant::random(&mut rand::rng()),
        ] {
            let (shared, reference) = compare_with_reference(&blocks, 1, r0)?;
            assert!(
                shared < reference,
                "shared {shared} vs reference {reference}"
            );
        }
        assert!(check_epoch(&blocks, 1, QrDiscriminant::random(&mut rand::rng()))? >= 4);
        Ok(())
    }

    /// `cargo test --release -p oss-core -- --ignored --nocapture epoch_build_cost`
    #[test]
    #[ignore = "benchmark; run explicitly"]
    fn epoch_build_cost() -> Result<()> {
        let sizes: Vec<usize> = std::env::var("OSS_BENCH_SIZES")
            .unwrap_or_else(|_| "2,4,8".into())
            .split(',')
            .map(|s| s.trim().parse().expect("size multiplier"))
            .collect();
        println!("capacity,members,buckets,builder,threads,steps,non_seed_steps,seconds");
        for multiple in sizes {
            let mut blocks = vec![genesis()];
            mine_to_epoch(&mut blocks, 1)?;
            populate_epoch(&mut blocks, multiple * max_polynomial_members())?;
            mine_to_epoch(&mut blocks, 2)?;
            let (inputs, predecessor) = epoch_inputs(&blocks, 1)?;
            let r0 = QrDiscriminant::random(&mut rand::rng());
            let members = multiple * max_polynomial_members();
            let threads_all = default_threads();
            for (name, threads) in [("shared", NonZeroUsize::MIN), ("shared", threads_all)] {
                let stats = BuildStats::default();
                let start = std::time::Instant::now();
                let out = build_epoch_with_stats(&inputs, predecessor, r0, threads, &stats)?;
                println!(
                    "{},{members},{},{name},{threads},{},{},{:.2}",
                    max_polynomial_members(),
                    out.len(),
                    stats.total(),
                    stats.total() - stats.seeds.load(Ordering::Relaxed),
                    start.elapsed().as_secs_f64()
                );
            }
            let stats = BuildStats::default();
            let start = std::time::Instant::now();
            let out = reference::build_epoch(&inputs, predecessor, r0, &stats)?;
            println!(
                "{},{members},{},reference,1,{},{},{:.2}",
                max_polynomial_members(),
                out.len(),
                stats.total(),
                stats.total() - stats.seeds.load(Ordering::Relaxed),
                start.elapsed().as_secs_f64()
            );
        }
        Ok(())
    }

    #[test]
    fn empty_epoch_yields_one_root_bucket() -> Result<()> {
        let mut blocks = vec![genesis()];
        mine_to_epoch(&mut blocks, 1)?;
        let (inputs, predecessor) = epoch_inputs(&blocks, 0)?;
        let buckets = build_epoch(&inputs, predecessor, QrDiscriminant(Fp::ONE))?;
        assert_eq!(buckets.len(), 1);
        assert_eq!((buckets[0].0, buckets[0].2), (QrProfile::ROOT, 0));
        assert_eq!(check_epoch(&blocks, 0, QrDiscriminant(Fp::ONE))?, 1);
        Ok(())
    }

    #[test]
    fn fixed_and_random_discriminants_both_seal_valid_evidence() -> Result<()> {
        let mut blocks = vec![genesis()];
        mine_to_epoch(&mut blocks, 1)?;
        populate_epoch(&mut blocks, 100)?;
        mine(&mut blocks)?;
        populate_epoch(&mut blocks, 30)?;
        mine_to_epoch(&mut blocks, 2)?;
        for r0 in [
            QrDiscriminant(Fp::ONE),
            QrDiscriminant::random(&mut rand::rng()),
        ] {
            assert_eq!(check_epoch(&blocks, 1, r0)?, 1);
        }
        // A partial epoch is never sealed.
        let (inputs, predecessor) = epoch_inputs(&blocks, 1)?;
        let seal =
            |inputs: &[TachyonBlock]| build_epoch(inputs, predecessor, QrDiscriminant(Fp::ONE));
        let mut gap = inputs.clone();
        gap.remove(1);
        assert!(seal(&gap).is_err());
        assert!(seal(&inputs[..inputs.len() - 1]).is_err());
        Ok(())
    }

    #[test]
    fn over_capacity_epoch_routes_every_tachygram_to_its_bucket() -> Result<()> {
        let mut blocks = vec![genesis()];
        mine_to_epoch(&mut blocks, 1)?;
        populate_epoch(&mut blocks, max_polynomial_members() + 19)?;
        mine_to_epoch(&mut blocks, 2)?;
        assert!(check_epoch(&blocks, 1, QrDiscriminant::random(&mut rand::rng()))? >= 2);
        Ok(())
    }

    #[test]
    fn empty_chunk_spans_survive_multiple_routing_levels() -> Result<()> {
        let disc = QrDiscriminant(Fp::ONE);
        let query = Tachygram::from(Fp::from(7));
        let side = QrClassRoot::of(query.into(), disc.at(0)).0;
        let excluded = (1u64..)
            .map(Fp::from)
            .map(Tachygram::from)
            .find(|m| QrClassRoot::of((*m).into(), disc.at(0)).0 != side)
            .unwrap();
        let mut cursor = Anchor::default();
        let mut chunks = Vec::new();
        for member in [excluded, query] {
            let members = vec![member];
            let (proof, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrStampIntakeSeed,
                witness::qr_stamp_intake_seed(((), ()), cursor, EpochIndex::new(0), disc, &members),
            )?;
            cursor = cursor.next_stamp(
                EpochIndex::new(0),
                &members
                    .iter()
                    .copied()
                    .collect::<TachygramSetPoly>()
                    .commit(),
            )?;
            chunks.push(Intake { proof, members });
        }
        let epoch = Epoch {
            epoch: 0,
            stamps: Vec::new(),
            members: vec![excluded, query],
            start: Anchor::default(),
            next: cursor.next_epoch(EpochIndex::new(1))?,
        };
        let path = [side, QrClassRoot::of(query.into(), disc.at(1)).0];
        let (bucket, members) = reference::build(
            &epoch,
            &path,
            &chunks,
            Anchor(Fp::ZERO),
            disc,
            &BuildStats::default(),
        )?;
        assert_eq!(members, 1);
        assert_eq!(bucket.data().1, Anchor::default());
        assert_eq!(
            bucket.data().4,
            QrProfile::ROOT.descend(path[0]).descend(path[1])
        );
        assert!(verify_pcd(&bucket)?);
        Ok(())
    }

    #[test]
    fn empty_routed_stamps_still_cover_their_anchor_transitions() -> Result<()> {
        let epoch = EpochIndex::new(0);
        let discriminant = QrDiscriminant(Fp::from(1));
        let query = Tachygram::from(Fp::from(7));
        let side = QrClassRoot::of(query.into(), discriminant.at(0)).0;
        let excluded: Vec<_> = (1u64..)
            .map(|i| Tachygram::from(Fp::from(i)))
            .filter(|value| QrClassRoot::of((*value).into(), discriminant.at(0)).0 != side)
            .take(2)
            .collect();
        let mut anchor = Anchor::default();
        let mut intakes = Vec::new();
        for members in [vec![excluded[0]], vec![excluded[1]], vec![query]] {
            let (proof, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrStampIntakeSeed,
                witness::qr_stamp_intake_seed(((), ()), anchor, epoch, discriminant, &members),
            )?;
            anchor = anchor.next_stamp(
                epoch,
                &members
                    .iter()
                    .copied()
                    .collect::<TachygramSetPoly>()
                    .commit(),
            )?;
            intakes.push(reference::route(
                Intake { proof, members },
                &[side],
                discriminant,
                &BuildStats::default(),
            )?);
        }
        assert!(intakes[0].members.is_empty());
        assert!(intakes[1].members.is_empty());
        assert_eq!(intakes[2].members, vec![query]);
        let mut intakes = intakes.into_iter();
        let stats = BuildStats::default();
        let first = reference::merge(intakes.next().unwrap(), intakes.next().unwrap(), &stats)?;
        let intake = reference::merge(first, intakes.next().unwrap(), &stats)?;
        assert_eq!(intake.proof.data().1, Anchor::default());
        assert_eq!(intake.proof.data().2, anchor);
        let (bucket, ()) = PROOF_SYSTEM.fuse(
            &mut rand::rng(),
            qr::QrBucketSeal,
            witness::qr_bucket_seal((*intake.proof.data(), ()), Anchor(Fp::from(0))),
            intake.proof,
            trivial(),
        )?;
        assert_eq!(bucket.data().2, anchor.next_epoch(EpochIndex::new(1))?);
        // Absence from a different bucket does not prove absence from the epoch:
        // QrUnspentInit must bind the tested value to the selected path.
        let result = PROOF_SYSTEM.fuse(
            &mut rand::rng(),
            qr::QrUnspentInit,
            witness::qr_unspent_init((*bucket.data(), ()), excluded[0]),
            bucket,
            trivial(),
        );
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn routing_stops_at_capacity_and_descends_further_for_skewed_members() -> Result<()> {
        let discriminant = QrDiscriminant(Fp::from(1));
        let query = Fp::from(7);
        let side = QrClassRoot::of(query, discriminant.at(0)).0;
        let members: Vec<_> = (1u64..)
            .map(|i| Tachygram::from(Fp::from(i)))
            .filter(|v| QrClassRoot::of((*v).into(), discriminant.at(0)).0 == side)
            .take(max_polynomial_members() + 1)
            .collect();
        let leaves = |members: &[Tachygram]| -> Result<Vec<QrProfile>> {
            Ok(profiles(members.to_vec(), discriminant)?
                .iter()
                .map(|path| path.iter().fold(QrProfile::ROOT, |p, &b| p.descend(b)))
                .collect())
        };
        assert_eq!(
            profile_of(
                query,
                discriminant,
                &leaves(&members[..max_polynomial_members()])?
            ),
            Some(QrProfile::ROOT)
        );
        assert_eq!(
            profile_of(query, discriminant, &leaves(&members)?),
            Some(
                QrProfile::ROOT
                    .descend(side)
                    .descend(QrClassRoot::of(query, discriminant.at(1)).0)
            )
        );
        Ok(())
    }
}
