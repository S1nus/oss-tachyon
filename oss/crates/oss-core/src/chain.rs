//! Block checks, moved from devtool `chain.rs` (`Block::validate*`): every
//! block's anchor chain is re-validated once, for every wallet.

use anyhow::{Context, Result, ensure};
use zcash_tachyon::{Anchor, EpochIndex, TachygramSetPoly, constants};

use crate::{TachyonBlock, anchor, anchor_hex, bytes32};

pub const MAX_EPOCH_MEMBERS: usize = 65_536;

pub fn validate(
    block: &TachyonBlock,
    previous: Option<&TachyonBlock>,
    activation: u32,
) -> Result<()> {
    validate_inner(block, previous, activation, false)
}

/// Start at an epoch boundary trusted from the configured node. This checks
/// the block's transitions, but does not prove the preceding chain history.
pub fn validate_checkpoint(block: &TachyonBlock, activation: u32) -> Result<()> {
    ensure!(
        block.pool_height.is_multiple_of(constants::EPOCH_SIZE),
        "checkpoint must start at an epoch boundary"
    );
    validate_inner(block, None, activation, true)
}

fn validate_inner(
    block: &TachyonBlock,
    previous: Option<&TachyonBlock>,
    activation: u32,
    checkpoint: bool,
) -> Result<()> {
    bytes32(&block.hash)?;
    bytes32(&block.previous_block_hash)?;
    ensure!(
        block.activation_height == activation,
        "NuTachyon activation height drift"
    );
    ensure!(
        block.height.checked_sub(activation) == Some(block.pool_height),
        "invalid pool height"
    );
    ensure!(
        block.epoch_length == constants::EPOCH_SIZE,
        "node and wallet epoch lengths differ"
    );
    ensure!(
        block.epoch == block.pool_height / constants::EPOCH_SIZE,
        "invalid epoch"
    );
    ensure!(
        block.epoch < constants::EPOCH_MAX,
        "final protocol epoch is unsupported"
    );
    ensure!(block.stamps.len() <= MAX_EPOCH_MEMBERS, "too many stamps");
    let mut cursor = anchor(&block.anchor_before)?;
    match previous {
        Some(previous) => {
            ensure!(
                previous.height.checked_add(1) == Some(block.height),
                "chain gap"
            );
            ensure!(
                previous.hash == block.previous_block_hash,
                "chain reorganized while syncing"
            );
            ensure!(
                previous.anchor_after == block.anchor_before,
                "anchor discontinuity"
            );
        }
        None => {
            ensure!(
                checkpoint || block.height == activation,
                "feed must start at activation"
            );
            if block.height == activation {
                ensure!(cursor == Anchor::default(), "incorrect activation anchor");
            }
        }
    }
    if block.pool_height.is_multiple_of(constants::EPOCH_SIZE) {
        if block.epoch != 0 {
            cursor = cursor.next_epoch(EpochIndex::new(block.epoch))?;
        }
        ensure!(
            block.epoch_start_anchor.as_deref() == Some(anchor_hex(cursor).as_str()),
            "incorrect epoch crossing"
        );
    } else {
        ensure!(
            block.epoch_start_anchor.is_none(),
            "unexpected epoch crossing"
        );
    }
    let mut last_index = None;
    let mut member_count = 0usize;
    for stamp in &block.stamps {
        bytes32(&stamp.txid)?;
        ensure!(
            last_index.is_none_or(|last| stamp.transaction_index > last),
            "stamps are out of order"
        );
        last_index = Some(stamp.transaction_index);
        let members = stamp.members()?;
        member_count = member_count
            .checked_add(members.len())
            .context("member count overflow")?;
        ensure!(
            member_count <= MAX_EPOCH_MEMBERS,
            "block exceeds wallet's experimental capacity"
        );
        let unique: std::collections::BTreeSet<_> = members.iter().copied().collect();
        ensure!(
            unique.len() == members.len(),
            "duplicate tachygrams in a stamp"
        );
        let commit = members.into_iter().collect::<TachygramSetPoly>().commit();
        ensure!(
            commit == stamp.commitment()?,
            "tachygrams do not match their commitment"
        );
        cursor = cursor.next_stamp(EpochIndex::new(block.epoch), &commit)?;
    }
    ensure!(
        cursor == anchor(&block.anchor_after)?,
        "stamps do not reproduce the block anchor"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{evidence::testing::*, max_polynomial_members};

    #[test]
    fn rejects_corrupt_inputs_and_discontinuities() -> Result<()> {
        let mut blocks = vec![genesis()];
        let activation = blocks[0].height;
        populate_epoch(&mut blocks, 70)?;
        mine_to_epoch(&mut blocks, 1)?;
        let n = blocks.len();
        validate(&blocks[0], None, activation)?;
        validate(&blocks[1], Some(&blocks[0]), activation)?;
        validate(&blocks[n - 1], Some(&blocks[n - 2]), activation)?;
        validate_checkpoint(&blocks[n - 1], activation)?;
        assert!(validate_checkpoint(&blocks[1], activation).is_err());
        assert!(validate(&blocks[n - 1], None, activation).is_err());

        let rejects = |index: usize, corrupt: fn(&mut TachyonBlock)| {
            let mut bad = blocks[index].clone();
            corrupt(&mut bad);
            validate(&bad, Some(&blocks[index - 1]), activation).is_err()
        };
        assert!(rejects(1, |b| drop(b.stamps[0].tachygrams.pop())));
        assert!(rejects(1, |b| b.stamps.swap(0, 1)));
        assert!(rejects(1, |b| b.previous_block_hash = hex::encode([9; 32])));
        assert!(rejects(1, |b| b.epoch_start_anchor = Some(b.anchor_before.clone())));
        assert!(rejects(1, |b| b.epoch_length /= 2));
        assert!(rejects(n - 1, |b| b.epoch_start_anchor =
            Some(b.anchor_before.clone())));
        assert!(rejects(n - 1, |b| b.epoch_start_anchor = None));

        let mut bad = blocks[1].clone();
        // Reject before constructing a polynomial, including under mock Ragu.
        bad.stamps[0].tachygrams = vec![hex::encode([0; 32]); max_polynomial_members() + 1];
        let error = validate(&bad, Some(&blocks[0]), activation).unwrap_err();
        assert!(error.to_string().contains("polynomial capacity"));
        Ok(())
    }
}
