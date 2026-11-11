//! Retroactive unspent proving: one `ArbitraryUnspent` segment over a note's
//! nullifiers for consecutive closed epochs. The step calls are devtool
//! `proofs.rs::spendable`'s, composed with `UnspentLift` instead of one bind and
//! lift per epoch. Note-blind: it sees only opaque values.

use anyhow::{Context, Result, ensure};
use zcash_tachyon::{
    EpochIndex, Tachygram,
    nullifier::Nullifier,
    pasta::Fp,
    stamp::proof::{PROOF_SYSTEM, pool, qr},
    witness,
};

use crate::{MAX_UNSPENT_NULLIFIERS, Pcd, trivial};

/// Proves `nfs[i]` absent from epoch `epoch_start + i`, for every `i`:
/// `QrUnspentInit` on the first epoch, then `UnspentLift` on each later one.
/// `bucket(epoch, value)` returns the sealed bucket of `epoch` whose profile
/// holds `value`. The result covers `[entry(epoch_start), entry(epoch_start + n))`.
///
/// Checks nothing against published tachygrams: the caller answers `Spent`.
pub fn prove_unspent(
    epoch_start: u32,
    nfs: &[Nullifier],
    mut bucket: impl FnMut(u32, Fp) -> Result<Pcd<qr::QrBucket>>,
) -> Result<Pcd<pool::ArbitraryUnspent>> {
    ensure!(
        (1..=MAX_UNSPENT_NULLIFIERS).contains(&nfs.len()),
        "expected 1 to {MAX_UNSPENT_NULLIFIERS} nullifiers, got {}",
        nfs.len()
    );
    let epoch_next = u32::try_from(nfs.len())
        .ok()
        .and_then(|n| epoch_start.checked_add(n))
        .context("epoch overflow")?;
    let mut segment: Option<Pcd<pool::ArbitraryUnspent>> = None;
    for (i, (epoch, nf)) in (epoch_start..epoch_next).zip(nfs).enumerate() {
        let evidence = bucket(epoch, (*nf).into())?;
        let value = Tachygram::from(*nf);
        segment = Some(match segment {
            None => {
                PROOF_SYSTEM
                    .fuse(
                        &mut rand::rng(),
                        qr::QrUnspentInit,
                        witness::qr_unspent_init((*evidence.data(), ()), value),
                        evidence,
                        trivial(),
                    )?
                    .0
            }
            Some(left) => {
                PROOF_SYSTEM
                    .fuse(
                        &mut rand::rng(),
                        pool::UnspentLift,
                        witness::unspent_lift((*left.data(), *evidence.data()), value, &nfs[..i]),
                        left,
                        evidence,
                    )?
                    .0
            }
        });
    }
    let segment = segment.expect("at least one nullifier");
    // `UnspentLift` chains epochs, but nothing pins the first bucket's epoch.
    let (_, start, next, _) = *segment.data();
    ensure!(
        (start, next) == (EpochIndex::new(epoch_start), EpochIndex::new(epoch_next)),
        "unspent segment does not cover the requested epochs"
    );
    Ok(segment)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, ops::Range};

    use zcash_tachyon::{
        Anchor, Note, QrClassRoot, QrDiscriminant, QrProfile,
        keys::private::SpendingKey,
        note::CommitmentTrapdoor,
        nullifier::{self, NF_DERIVATION_WIDTH},
        stamp::proof::{delegation, spendable},
        value,
    };

    use super::*;
    use crate::{
        TachyonBlock, build_epoch, evidence::testing::*, max_polynomial_members, pcd_from_hex,
        pcd_to_hex, profile_of, verify_pcd,
    };

    /// `build_epoch`'s output.
    type Sealed = Vec<(QrProfile, Pcd<qr::QrBucket>, usize)>;

    /// What the OSS holds after closing epochs `0..t`: each epoch's private
    /// `R0` and sealed buckets, and its archive's tachygram index. `entries`
    /// (`entry(0..=t)`) stand in for the wallet's own synced anchors.
    struct Oss {
        evidence: Vec<(QrDiscriminant, Sealed)>,
        entries: Vec<Anchor>,
        published: BTreeSet<Tachygram>,
    }

    impl Oss {
        /// Mines epochs `0..t`, `fill` adding blocks at the start of each, and
        /// seals each under its own `r0()`, as the OSS builder does at close.
        fn close(
            t: u32,
            mut r0: impl FnMut() -> QrDiscriminant,
            mut fill: impl FnMut(&mut Vec<TachyonBlock>, u32) -> Result<()>,
        ) -> Result<Self> {
            let mut blocks = vec![genesis()];
            let mut oss = Self {
                evidence: Vec::new(),
                entries: Vec::new(),
                published: BTreeSet::new(),
            };
            for epoch in 0..t {
                oss.entries.push(blocks[0].entry()?);
                fill(&mut blocks, epoch)?;
                mine_to_epoch(&mut blocks, epoch + 1)?;
                let (inputs, predecessor) = epoch_inputs(&blocks, epoch)?;
                let r0 = r0();
                oss.evidence
                    .push((r0, build_epoch(&inputs, predecessor, r0)?));
                for stamp in blocks.iter().flat_map(|b| &b.stamps) {
                    oss.published.extend(stamp.members()?);
                }
                // Keep only the next epoch's first block; `mine` reads the tip.
                blocks.drain(..blocks.len() - 1);
            }
            oss.entries.push(blocks[0].entry()?);
            Ok(oss)
        }

        /// The OSS's lookup (design §3.3): `bucket(e, profile_of(value, R0_e, profiles_e))`.
        fn bucket(&self, epoch: u32, value: Fp) -> Result<Pcd<qr::QrBucket>> {
            let (r0, buckets) = self
                .evidence
                .get(usize::try_from(epoch)?)
                .context("epoch is not closed")?;
            let profiles: Vec<_> = buckets.iter().map(|b| b.0).collect();
            let profile = profile_of(value, *r0, &profiles).context("no bucket on the path")?;
            Ok(buckets.iter().find(|b| b.0 == profile).unwrap().1.clone())
        }

        fn unspent(
            &self,
            epoch_start: u32,
            nfs: &[Nullifier],
        ) -> Result<Pcd<pool::ArbitraryUnspent>> {
            prove_unspent(epoch_start, nfs, |epoch, value| self.bucket(epoch, value))
        }

        fn entry(&self, epoch: u32) -> Anchor {
            self.entries[epoch as usize]
        }
    }

    fn random_r0() -> QrDiscriminant {
        QrDiscriminant::random(&mut rand::rng())
    }

    /// One note's owner, running the wallet steps of devtool `proofs.rs`.
    struct Wallet {
        key: SpendingKey,
        note: Note,
        secret: Pcd<delegation::NoteSecret>,
    }

    impl Wallet {
        /// A fresh note and its `NoteSeed` (devtool `secret`).
        fn new() -> Result<Self> {
            let rng = &mut rand::rng();
            let key = SpendingKey::random(rng);
            let note = Note {
                pk: key.derive_payment_key(),
                value: value::Positive::try_from(1_000_000u64)?,
                psi: nullifier::Trapdoor::random(rng),
                rcm: CommitmentTrapdoor::random(rng),
            };
            let (secret, ()) = PROOF_SYSTEM.seed(
                rng,
                delegation::NoteSeed,
                witness::note_seed(((), ()), note, key.derive_proof_private()),
            )?;
            Ok(Self { key, note, secret })
        }

        fn cm(&self) -> Tachygram {
            self.note.commitment().into()
        }

        fn nfs(&self, epochs: Range<u32>) -> Vec<Nullifier> {
            let mk = self
                .key
                .derive_nullifier_private()
                .derive_note_private(self.note.psi);
            epochs
                .map(|e| mk.derive_nullifier(EpochIndex::new(e)))
                .collect()
        }

        /// devtool `derivation`, one `NullifierDerive` per 16-epoch window
        /// covering `epochs`, joined by `NullifierFuse`.
        fn derivation(&self, epochs: Range<u32>) -> Result<Pcd<delegation::NoteNullifiers>> {
            let width = u32::try_from(NF_DERIVATION_WIDTH)?;
            let first = epochs.start - epochs.start % width;
            let mut derived: Option<Pcd<delegation::NoteNullifiers>> = None;
            for start in (first..epochs.end).step_by(NF_DERIVATION_WIDTH) {
                let (window, ()) = PROOF_SYSTEM.fuse(
                    &mut rand::rng(),
                    delegation::NullifierDerive,
                    witness::nullifier_derive((*self.secret.data(), ()), EpochIndex::new(start)),
                    self.secret.clone(),
                    trivial(),
                )?;
                derived = Some(match derived {
                    None => window,
                    Some(left) => {
                        PROOF_SYSTEM
                            .fuse(
                                &mut rand::rng(),
                                delegation::NullifierFuse,
                                witness::nullifier_fuse(
                                    (*left.data(), *window.data()),
                                    &self.nfs(first..start),
                                    &self.nfs(start..start + width),
                                ),
                                left,
                                window,
                            )?
                            .0
                    }
                });
            }
            Ok(derived.unwrap())
        }

        /// `unspent(k, nfs)` as a remote wallet gets it: over the wire, verified,
        /// its header checked against the wallet's own entry anchors.
        fn fetch(&self, oss: &Oss, epochs: Range<u32>) -> Result<Pcd<pool::ArbitraryUnspent>> {
            let segment = oss.unspent(epochs.start, &self.nfs(epochs.clone()))?;
            let segment = pcd_from_hex::<pool::ArbitraryUnspent>(&pcd_to_hex(&segment)?)?;
            assert!(verify_pcd(&segment)?);
            assert_eq!(
                *segment.data(),
                (
                    oss.entry(epochs.start),
                    EpochIndex::new(epochs.start),
                    EpochIndex::new(epochs.end),
                    oss.entry(epochs.end),
                )
            );
            Ok(segment)
        }

        /// `UnspentBind` with a derivation covering the segment.
        fn bind(&self, segment: Pcd<pool::ArbitraryUnspent>) -> Result<Pcd<pool::NoteUnspent>> {
            let (_, start, next, _) = *segment.data();
            let epochs = u32::from(start)..u32::from(next);
            let derived = self.derivation(epochs.clone())?;
            let (_, first, last) = *derived.data();
            let window = self.nfs(u32::from(first)..u32::from(last) + 1);
            Ok(PROOF_SYSTEM
                .fuse(
                    &mut rand::rng(),
                    pool::UnspentBind,
                    witness::unspent_bind(
                        (*segment.data(), *derived.data()),
                        &window,
                        &self.nfs(epochs),
                    ),
                    segment,
                    derived,
                )?
                .0)
        }

        /// No lineage, `c < t`: `unspent(c, [nf_c])`, `UnspentBind`, then
        /// `QrSpendableInit` on the bucket `profile_of(cm)` names.
        fn start(&self, oss: &Oss, c: u32) -> Result<Pcd<spendable::NoteSpendable>> {
            let unspent = self.bind(self.fetch(oss, c..c + 1)?)?;
            let creation = oss.bucket(c, self.cm().into())?;
            let (lineage, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                spendable::QrSpendableInit,
                (),
                unspent,
                creation,
            )?;
            self.check(oss, &lineage, c + 1)?;
            Ok(lineage)
        }

        /// One page from the lineage's epoch `k`: `unspent(k, [nf_k … nf_{next−1}])`,
        /// `UnspentBind` and `SpendableLift`.
        fn lift(
            &self,
            oss: &Oss,
            lineage: Pcd<spendable::NoteSpendable>,
            next: u32,
        ) -> Result<Pcd<spendable::NoteSpendable>> {
            let k = u32::from(lineage.data().1);
            let unspent = self.bind(self.fetch(oss, k..next)?)?;
            let (lineage, ()) = PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                spendable::SpendableLift,
                (),
                lineage,
                unspent,
            )?;
            self.check(oss, &lineage, next)?;
            Ok(lineage)
        }

        /// The lineage is this note's and rests on `entry(epoch)`.
        fn check(
            &self,
            oss: &Oss,
            lineage: &Pcd<spendable::NoteSpendable>,
            epoch: u32,
        ) -> Result<()> {
            let (cm, at, anchor) = *lineage.data();
            assert_eq!(Tachygram::from(cm), self.cm());
            assert_eq!((at, anchor), (EpochIndex::new(epoch), oss.entry(epoch)));
            assert!(verify_pcd(lineage)?);
            Ok(())
        }
    }

    fn message(result: Result<impl Sized>) -> String {
        result.err().expect("must fail").to_string()
    }

    #[test]
    fn wallet_lifts_a_note_across_populated_and_empty_epochs() -> Result<()> {
        let wallet = Wallet::new()?;
        let (c, t) = (14, 19);
        // Creation epoch populated, then empty, populated, empty, empty.
        let oss = Oss::close(t, random_r0, |blocks, epoch| match epoch {
            14 => {
                mine_stamp(blocks, &[wallet.cm()])?;
                populate_epoch(blocks, 100)
            }
            16 => populate_epoch(blocks, 50),
            _ => Ok(()),
        })?;
        let lineage = wallet.start(&oss, c)?;
        // One page [15, 19) crosses a derivation window: two
        // NullifierDerive and one NullifierFuse.
        wallet.lift(&oss, lineage, t)?;
        Ok(())
    }

    #[test]
    fn a_full_page_and_the_next_each_bind_and_lift() -> Result<()> {
        let wallet = Wallet::new()?;
        let (c, t) = (1, 67);
        let oss = Oss::close(t, random_r0, |blocks, epoch| match epoch {
            1 => mine_stamp(blocks, &[wallet.cm()]),
            e if e % 8 == 3 => populate_epoch(blocks, 20),
            _ => Ok(()),
        })?;
        let lineage = wallet.start(&oss, c)?;
        // 65 epochs [2, 67) need two calls; the client pages at the limit.
        let full = 2 + MAX_UNSPENT_NULLIFIERS as u32;
        assert!(oss.unspent(2, &wallet.nfs(2..t)).is_err());
        let lineage = wallet.lift(&oss, lineage, full)?;
        wallet.lift(&oss, lineage, t)?;
        Ok(())
    }

    #[test]
    fn published_nullifier_cannot_be_proven_absent() -> Result<()> {
        let wallet = Wallet::new()?;
        let nfs = wallet.nfs(0..4);
        // The note is spent in epoch 2, publishing nf_2 (current) and nf_3 (next).
        let oss = Oss::close(4, random_r0, |blocks, epoch| match epoch {
            2 => mine_stamp(blocks, &[nfs[2].into(), nfs[3].into()]),
            _ => Ok(()),
        })?;
        assert!(message(oss.unspent(2, &nfs[2..3])).contains("QrUnspentInit: found nullifier"));
        assert!(message(oss.unspent(1, &nfs[1..3])).contains("UnspentLift: found nullifier"));
        assert!(message(oss.unspent(0, &nfs[0..3])).contains("UnspentLift: found nullifier"));

        // nf_3 went out as the "next" nullifier in epoch 2, so epoch 3's
        // buckets do not hold it and the proof succeeds. No lineage can reach
        // epoch 3 (nf_2 fails above), but the OSS must still answer `Spent`
        // (design §4.4, §4.5): its tachygram index catches it, not `prove_unspent`.
        oss.unspent(3, &nfs[3..4])?;
        assert!(oss.published.contains(&Tachygram::from(nfs[3])));
        Ok(())
    }

    /// The one over-capacity epoch (~16 s): several routed buckets.
    #[test]
    fn values_are_tested_only_against_their_own_routed_bucket() -> Result<()> {
        let wallet = Wallet::new()?;
        let spent = Wallet::new()?.nfs(0..2);
        let cm = Fp::from(wallet.cm());
        let nf = Fp::from(wallet.nfs(0..1)[0]);
        // Route the creation and the nullifier apart, as devtool's
        // `spends_after_epoch_with_more_tachygrams_than_polynomial_capacity` does.
        let apart = || loop {
            let r0 = random_r0();
            if QrClassRoot::of(cm, r0.at(0)).0 != QrClassRoot::of(nf, r0.at(0)).0 {
                return r0;
            }
        };
        let oss = Oss::close(1, apart, |blocks, _| {
            mine_stamp(blocks, &[wallet.cm()])?;
            mine_stamp(blocks, &[spent[0].into(), spent[1].into()])?;
            populate_epoch(blocks, max_polynomial_members() + 19)
        })?;
        let (r0, buckets) = &oss.evidence[0];
        let profiles: Vec<_> = buckets.iter().map(|b| b.0).collect();
        assert!(buckets.len() >= 2);
        assert_ne!(
            profile_of(cm, *r0, &profiles),
            profile_of(nf, *r0, &profiles)
        );
        wallet.start(&oss, 0)?;

        for (profile, bucket, _) in buckets {
            let other = |value| Some(*profile) != profile_of(value, *r0, &profiles);
            let test =
                |value| prove_unspent(0, &[Nullifier::from(value)], |_, _| Ok(bucket.clone()));
            // A wrong bucket fails on the profile, even for an unspent value.
            if other(nf) {
                assert!(message(test(nf)).contains("does not take the bucket's profile"));
            }
            // A published value fails in its own bucket and cannot hide in another.
            let spent = Fp::from(spent[0]);
            let expected = if other(spent) {
                "does not take the bucket's profile"
            } else {
                "found nullifier in the bucket"
            };
            assert!(message(test(spent)).contains(expected));
        }
        Ok(())
    }

    #[test]
    fn rejects_bad_counts_and_a_bucket_from_another_epoch() -> Result<()> {
        let oss = Oss::close(2, random_r0, |_, _| Ok(()))?;
        let nfs = Wallet::new()?.nfs(0..65);
        let unreachable = |_: u32, _: Fp| -> Result<Pcd<qr::QrBucket>> { unreachable!() };
        assert!(prove_unspent(0, &[], unreachable).is_err());
        assert!(prove_unspent(0, &nfs, unreachable).is_err());
        let stale = oss.bucket(0, nfs[1].into())?;
        let result = prove_unspent(1, &nfs[1..2], |_, _| Ok(stale.clone()));
        assert!(message(result).contains("does not cover the requested epochs"));
        Ok(())
    }
}
