//! Closed-epoch QR evidence, routed before merging to bound polynomial size.
//! Ported from devtool `proofs/epoch.rs`, without its caches.

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use zcash_tachyon::{
    Anchor, EpochIndex, QrClassRoot, QrDiscriminant, QrProfile, Tachygram,
    constants::EPOCH_SIZE,
    stamp::proof::{PROOF_SYSTEM, qr},
    witness,
};

use crate::{Pcd, TachyonBlock, anchor, max_polynomial_members, profiles, trivial};

/// Seals every `QrBucket` of a closed epoch: one per routing leaf of
/// [`profiles`], empty siblings included. `blocks` are the epoch's blocks plus
/// the next epoch's first block; `predecessor` is the final anchor of the
/// previous epoch (zero for epoch 0). Returns each profile, its bucket and its
/// member count.
pub fn build_epoch(
    blocks: &[TachyonBlock],
    predecessor: Anchor,
    r0: QrDiscriminant,
) -> Result<Vec<(QrProfile, Pcd<qr::QrBucket>, usize)>> {
    let epoch = Epoch::read(blocks)?;
    let chunks = epoch.chunks(r0)?;
    profiles(epoch.members.clone(), r0)?
        .into_iter()
        .map(|path| {
            let profile = path.iter().fold(QrProfile::ROOT, |p, &b| p.descend(b));
            let (bucket, members) = epoch.build(&path, &chunks, predecessor, r0)?;
            ensure!(
                bucket.data().0 == EpochIndex::new(epoch.epoch)
                    && bucket.data().1 == epoch.start
                    && bucket.data().3 == r0
                    && bucket.data().4 == profile,
                "sealed bucket does not match its epoch and profile"
            );
            Ok((profile, bucket, members))
        })
        .collect()
}

#[derive(Clone)]
struct Intake {
    proof: Pcd<qr::QrIntake>,
    members: Vec<Tachygram>,
}

impl Intake {
    fn route(mut self, path: &[bool], discriminant: QrDiscriminant) -> Result<Self> {
        for &side in path {
            let depth = self.proof.data().4.depth;
            let (sides, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                qr::QrIntakeSplit,
                witness::qr_intake_split((*self.proof.data(), ()), &self.members),
                self.proof,
                trivial(),
            )?;
            let (selection, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                qr::QrSideSelect,
                witness::qr_side_select((*sides.data(), ()), &self.members, side),
                sides.clone(),
                trivial(),
            )?;
            self.proof = PROOF_SYSTEM
                .fuse(
                    &mut rand::rng(),
                    qr::QrSideDescend,
                    witness::qr_side_descend((*sides.data(), *selection.data()), side),
                    sides,
                    selection,
                )?
                .0;
            let shift = discriminant.at(depth);
            self.members
                .retain(|member| QrClassRoot::of((*member).into(), shift).0 == side);
        }
        Ok(self)
    }

    fn merge(mut self, right: Self) -> Result<Self> {
        ensure!(
            self.members.len() + right.members.len() <= max_polynomial_members(),
            "routed epoch bucket exceeds Tachyon's polynomial capacity"
        );
        self.proof = PROOF_SYSTEM
            .fuse(
                &mut rand::rng(),
                qr::QrIntakeMerge,
                witness::qr_intake_merge(
                    (*self.proof.data(), *right.proof.data()),
                    &self.members,
                    &right.members,
                ),
                self.proof,
                right.proof,
            )?
            .0;
        self.members.extend(right.members);
        Ok(self)
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
            // contiguous coverage through the closing sentinel.
            // shortcut: only linkage is checked here; full block validation runs at
            // ingest (T1). Call `chain::validate` here once T1 lands.
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

    /// One seeded intake per stamp, merged in order into capacity-sized chunks.
    fn chunks(&self, discriminant: QrDiscriminant) -> Result<Vec<Intake>> {
        let epoch = EpochIndex::new(self.epoch);
        let mut chunks = Vec::new();
        let mut current: Option<Intake> = None;
        for (before, members) in self.stamps.iter().cloned() {
            let (proof, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrStampIntakeSeed,
                witness::qr_stamp_intake_seed(((), ()), before, epoch, discriminant, &members),
            )?;
            let intake = Intake { proof, members };
            current = match current.take() {
                Some(left)
                    if left.members.len() + intake.members.len() <= max_polynomial_members() =>
                {
                    Some(left.merge(intake)?)
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
            let (proof, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrEmptyIntakeSeed,
                witness::qr_empty_intake_seed(((), ()), self.start, epoch, discriminant),
            )?;
            chunks.push(Intake {
                proof,
                members: Vec::new(),
            });
        }
        Ok(chunks)
    }

    /// Routes every chunk along `path`, merges them and seals the bucket.
    fn build(
        &self,
        path: &[bool],
        chunks: &[Intake],
        predecessor: Anchor,
        discriminant: QrDiscriminant,
    ) -> Result<(Pcd<qr::QrBucket>, usize)> {
        let mut intakes = chunks.to_vec();
        for &side in path {
            // QrIntakeSplit rejects empty intakes. Absorb empty spans into an
            // adjacent nonempty intake before descending, preserving coverage
            // without inventing a proof or dropping an anchor transition.
            let mut joined: Vec<Intake> = Vec::new();
            for right in intakes {
                match joined.pop() {
                    Some(left) if left.members.is_empty() || right.members.is_empty() => {
                        joined.push(left.merge(right)?);
                    }
                    Some(left) => joined.extend([left, right]),
                    None => joined.push(right),
                }
            }
            intakes = joined
                .into_iter()
                .map(|intake| intake.route(&[side], discriminant))
                .collect::<Result<_>>()?;
        }
        // Balanced, adjacent merges avoid rebuilding an ever-growing prefix for
        // every stamp, while preserving the original anchor order.
        while intakes.len() > 1 {
            let mut merged = Vec::new();
            let mut pairs = intakes.into_iter();
            while let Some(left) = pairs.next() {
                merged.push(match pairs.next() {
                    Some(right) => left.merge(right)?,
                    None => left,
                });
            }
            intakes = merged;
        }
        let Intake { proof, members } = intakes.pop().context("missing epoch intake")?;
        let (bucket, ()) = PROOF_SYSTEM.fuse(
            &mut rand::rng(),
            qr::QrBucketSeal,
            witness::qr_bucket_seal((*proof.data(), ()), predecessor),
            proof,
            trivial(),
        )?;
        ensure!(
            bucket.data().2 == self.next,
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
        let (bucket, members) = epoch.build(&path, &chunks, Anchor(Fp::ZERO), disc)?;
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
            intakes.push(Intake { proof, members }.route(&[side], discriminant)?);
        }
        assert!(intakes[0].members.is_empty());
        assert!(intakes[1].members.is_empty());
        assert_eq!(intakes[2].members, vec![query]);
        let mut intakes = intakes.into_iter();
        let intake = intakes
            .next()
            .unwrap()
            .merge(intakes.next().unwrap())?
            .merge(intakes.next().unwrap())?;
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
