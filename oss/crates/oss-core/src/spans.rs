//! Anchor-span chunks over one epoch, ported from devtool `proofs.rs`
//! `anchor_spans`. The OSS extends them block by block from the epoch's entry
//! anchor; devtool's local mode starts at its earliest spend anchor.

use anyhow::{Result, ensure};
use zcash_tachyon::{
    Anchor, EpochIndex, Tachygram,
    stamp::proof::{
        PROOF_SYSTEM,
        pool::{self, AnchorSpan},
    },
    witness,
};

use crate::{Pcd, TachyonBlock, anchor_hex};

/// `(target, chunks, start)`: `from[i]` lifts to `target` along
/// `chunks[start[i]..]`, in order; `start[i]` is `None` when it is `target`.
pub type Spans = (Anchor, Vec<Pcd<AnchorSpan>>, Vec<Option<usize>>);

/// One `AnchorSpan` over `[start, members.last()]`. `members` are the anchors
/// in `(start, end]`, in chain order.
#[derive(Clone)]
pub struct SpanChunk {
    pub start: Anchor,
    pub members: Vec<Anchor>,
    pub pcd: Pcd<AnchorSpan>,
}

impl SpanChunk {
    pub fn end(&self) -> Anchor {
        *self
            .members
            .last()
            .expect("a chunk holds at least one stamp")
    }
}

/// Contiguous chunks from `start`: chunk `i` covers `[a_i, a_{i+1}]` and holds
/// at most `chunk_capacity` members.
#[derive(Clone)]
pub struct SpanBuilder {
    epoch: u32,
    start: Anchor,
    capacity: usize,
    chunks: Vec<SpanChunk>,
}

impl SpanBuilder {
    /// `chunk_capacity` is at most [`crate::max_polynomial_members`], which is
    /// what production uses; tests pass less to exercise rollover.
    pub fn new(epoch: u32, start: Anchor, chunk_capacity: usize) -> Self {
        Self::resume(epoch, start, chunk_capacity, Vec::new())
    }

    /// Continues from chunks a previous builder produced, e.g. loaded from a store.
    pub fn resume(
        epoch: u32,
        start: Anchor,
        chunk_capacity: usize,
        chunks: Vec<SpanChunk>,
    ) -> Self {
        assert!(
            (1..=crate::max_polynomial_members()).contains(&chunk_capacity),
            "chunk capacity must fit one polynomial"
        );
        Self {
            epoch,
            start,
            capacity: chunk_capacity,
            chunks,
        }
    }

    pub fn start(&self) -> Anchor {
        self.start
    }

    pub fn chunks(&self) -> &[SpanChunk] {
        &self.chunks
    }

    /// The anchor every span ends at: the last pushed stamp's, else `start`.
    pub fn target(&self) -> Anchor {
        self.chunks.last().map_or(self.start, SpanChunk::end)
    }

    /// `AnchorSpanSeed` over one stamp, then `AnchorSpanFuse` into the tail
    /// chunk, or a new chunk once the tail is full.
    pub fn push_stamp(
        &mut self,
        before: Anchor,
        members: &[Tachygram],
        after: Anchor,
    ) -> Result<()> {
        ensure!(
            before == self.target(),
            "stamp at {} does not extend the span at {}",
            anchor_hex(before),
            anchor_hex(self.target())
        );
        let (seed, ()) = PROOF_SYSTEM.seed(
            &mut rand::rng(),
            pool::AnchorSpanSeed,
            witness::anchor_span_seed(((), ()), before, EpochIndex::new(self.epoch), members),
        )?;
        ensure!(
            seed.data().1 == after,
            "stamp members do not lead to {}",
            anchor_hex(after)
        );
        match self.chunks.last_mut() {
            Some(tail) if tail.members.len() < self.capacity => {
                // Clone so a failed fuse leaves the tail intact.
                let left = tail.pcd.clone();
                tail.pcd = PROOF_SYSTEM
                    .fuse(
                        &mut rand::rng(),
                        pool::AnchorSpanFuse,
                        witness::anchor_span_fuse(
                            (*left.data(), *seed.data()),
                            &tail.members,
                            &[after],
                        ),
                        left,
                        seed,
                    )?
                    .0;
                tail.members.push(after);
            }
            _ => self.chunks.push(SpanChunk {
                start: before,
                members: vec![after],
                pcd: seed,
            }),
        }
        Ok(())
    }

    /// Pushes the block's stamps from [`Self::target`] on. Earlier stamps are
    /// skipped, so a builder can start mid-block and a block can be fed twice.
    /// Returns how many stamps it pushed; callers check `target()` themselves.
    pub fn push_block(&mut self, block: &TachyonBlock) -> Result<usize> {
        ensure!(
            block.epoch == self.epoch,
            "block {} is in epoch {}, not {}",
            block.height,
            block.epoch,
            self.epoch
        );
        let mut before = block.entry()?;
        let mut pushed = 0;
        for stamp in &block.stamps {
            let after = before.next_stamp(EpochIndex::new(self.epoch), &stamp.commitment()?)?;
            if before == self.target() {
                self.push_stamp(before, &stamp.members()?, after)?;
                pushed += 1;
            }
            before = after;
        }
        Ok(pushed)
    }

    /// The [`Spans`] that lift every anchor of `from` to [`Self::target`].
    /// `None` if any anchor is not `start`, a member, or `target`.
    ///
    /// Chunk choice for `x`: the chunk that starts at `x`, else the chunk that
    /// holds `x` as a member, so an anchor at a chunk edge takes the later
    /// chunk. `chunks` begins at the smallest chosen index.
    pub fn spans_from(&self, from: &[Anchor]) -> Option<Spans> {
        let target = self.target();
        let chosen = from
            .iter()
            .map(|x| {
                if *x == target {
                    return Some(None);
                }
                self.chunks
                    .iter()
                    .position(|c| c.start == *x)
                    .or_else(|| self.chunks.iter().position(|c| c.members.contains(x)))
                    .map(Some)
            })
            .collect::<Option<Vec<_>>>()?;
        let first = chosen
            .iter()
            .flatten()
            .min()
            .copied()
            .unwrap_or(self.chunks.len());
        Some((
            target,
            self.chunks[first..].iter().map(|c| c.pcd.clone()).collect(),
            chosen.into_iter().map(|i| i.map(|i| i - first)).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, time::Instant};

    use anyhow::Context;
    use zcash_tachyon::{
        fee,
        keys::private::SpendingKey,
        note::{self, Note},
        nullifier,
        pasta::Fp,
        stamp, value,
    };

    use super::*;
    use crate::{anchor, evidence::testing::*, verify_pcd};

    /// Genesis, then blocks of 1, 0, 3, 2 and 1 stamps. `anchors[0]` is the
    /// entry anchor and `anchors[i]` follows stamp `i`.
    fn chain() -> Result<(Vec<TachyonBlock>, Vec<Anchor>)> {
        let mut blocks = vec![genesis()];
        populate_epoch(&mut blocks, 1)?;
        mine(&mut blocks)?;
        populate_epoch(&mut blocks, 130)?;
        populate_epoch(&mut blocks, 65)?;
        populate_epoch(&mut blocks, 1)?;
        let mut anchors = vec![blocks[0].entry()?];
        for block in &blocks {
            for stamp in &block.stamps {
                let last = *anchors.last().unwrap();
                anchors.push(last.next_stamp(EpochIndex::new(0), &stamp.commitment()?)?);
            }
        }
        assert_eq!(anchors.len(), 8);
        assert_eq!(anchors[7], anchor(&blocks[5].anchor_after)?);
        Ok((blocks, anchors))
    }

    fn headers<'a>(chunks: impl IntoIterator<Item = &'a Pcd<AnchorSpan>>) -> Vec<(Anchor, Anchor)> {
        chunks.into_iter().map(|c| *c.data()).collect()
    }

    /// A real stamp at `at` (FeeSeed: a plan with no actions), lifted along
    /// `spans` with `ProofStamp::lift`, as devtool lifts its spends. Returns
    /// the lifted stamp's anchor.
    fn lift_fee_stamp(at: Anchor, spans: Vec<Pcd<AnchorSpan>>) -> Result<Anchor> {
        let rng = &mut rand::rng();
        let sk = SpendingKey::random(rng);
        let max_fee_note = Note {
            pk: sk.derive_payment_key(),
            value: value::Positive::ZERO,
            psi: nullifier::Trapdoor::random(rng),
            rcm: note::CommitmentTrapdoor::random(rng),
        };
        let fee = fee::Descriptor {
            cv_feemax: max_fee_note.cv_feemax(),
            v_fee: value::Positive::ZERO,
        };
        let stamp = stamp::Plan::new(vec![], vec![], at, max_fee_note, value::Positive::ZERO)
            .prove(rng, &sk.derive_proof_private(), vec![])?;
        assert_eq!(stamp.anchor, at);
        Ok(stamp
            .lift(rng, &BTreeSet::new(), &BTreeSet::from([fee]), spans)?
            .anchor)
    }

    #[test]
    fn chunks_roll_over_and_every_anchor_lifts_to_the_tip() -> Result<()> {
        let (blocks, anchors) = chain()?;
        let target = anchors[7];
        let mut builder = SpanBuilder::new(0, anchors[0], 3);
        let (t, chunks, start) = builder.spans_from(&[anchors[0]]).context("entry")?;
        assert_eq!((t, chunks.len(), start), (anchors[0], 0, vec![None]));
        assert!(builder.spans_from(&[anchors[1]]).is_none());

        let timer = Instant::now();
        let mut pushed = 0;
        for block in &blocks {
            pushed += builder.push_block(block)?;
        }
        eprintln!("seed+fuse: {:?} per stamp", timer.elapsed() / 7);
        assert_eq!(pushed, 7);
        assert_eq!(builder.push_block(&blocks[3])?, 0, "a block fed twice");
        assert_eq!(builder.target(), target);

        let sizes: Vec<_> = builder.chunks().iter().map(|c| c.members.len()).collect();
        assert_eq!(sizes, [3, 3, 1]);
        for (i, chunk) in builder.chunks().iter().enumerate() {
            assert_eq!(chunk.start, anchors[3 * i]);
            assert_eq!(chunk.members, anchors[3 * i + 1..(3 * i + 4).min(8)]);
            assert_eq!(*chunk.pcd.data(), (chunk.start, chunk.end()));
            assert!(verify_pcd(&chunk.pcd)?);
        }

        // Entry and every member reach the tip. Anchors 3 and 6 sit on chunk
        // edges and take the later chunk; the target itself needs no chunk.
        for (i, x) in anchors.iter().enumerate() {
            let (t, chunks, start) = builder.spans_from(&[*x]).context("known anchor")?;
            assert_eq!(t, target);
            let expected = if i == 7 {
                &[][..]
            } else {
                &builder.chunks()[i / 3..]
            };
            assert_eq!(headers(&chunks), headers(expected.iter().map(|c| &c.pcd)));
            assert_eq!(start, [(i != 7).then_some(0)]);
        }

        // One read for several spends: one target, one chunk list.
        let (t, chunks, start) = builder
            .spans_from(&[anchors[5], anchors[1], target, anchors[6]])
            .context("known anchors")?;
        assert_eq!(t, target);
        assert_eq!(
            headers(&chunks),
            headers(builder.chunks().iter().map(|c| &c.pcd))
        );
        assert_eq!(start, [Some(1), Some(0), None, Some(2)]);
        let (_, chunks, start) = builder
            .spans_from(&[anchors[5], anchors[4]])
            .context("known")?;
        assert_eq!((chunks.len(), start), (2, vec![Some(0), Some(0)]));

        let unknown = Anchor(Fp::from(7));
        assert!(builder.spans_from(&[unknown]).is_none());
        assert!(builder.spans_from(&[anchors[1], unknown]).is_none());

        // A real stamp at a member anchor lifts along the returned chunks.
        let (_, chunks, start) = builder.spans_from(&[anchors[2]]).context("member")?;
        let s = start[0].context("not the target")?;
        assert_eq!(lift_fee_stamp(anchors[2], chunks[s..].to_vec())?, target);
        // StampLift checks membership: skipping the anchor's chunk fails.
        assert!(lift_fee_stamp(anchors[2], chunks[s + 1..].to_vec()).is_err());
        Ok(())
    }

    #[test]
    fn builder_started_mid_block_skips_earlier_stamps() -> Result<()> {
        let (blocks, anchors) = chain()?;
        // anchors[3] follows the second of block 3's three stamps.
        let mut builder = SpanBuilder::new(0, anchors[3], 3);
        let pushed: Vec<_> = blocks
            .iter()
            .map(|b| builder.push_block(b))
            .collect::<Result<_>>()?;
        assert_eq!(pushed, [0, 0, 0, 1, 2, 1]);
        assert_eq!(builder.target(), anchors[7]);
        let sizes: Vec<_> = builder.chunks().iter().map(|c| c.members.len()).collect();
        assert_eq!(sizes, [3, 1]);
        let (t, chunks, start) = builder.spans_from(&[anchors[3]]).context("start")?;
        assert_eq!((t, chunks.len(), start), (anchors[7], 2, vec![Some(0)]));
        assert!(builder.spans_from(&[anchors[2]]).is_none());
        assert!(builder.push_stamp(anchors[2], &[], anchors[3]).is_err());
        Ok(())
    }
}
