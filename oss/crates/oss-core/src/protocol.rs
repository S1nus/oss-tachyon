//! The wallet ↔ OSS wire contract (design §4.2), the `gettachyonblock` types
//! both sides read, PCD hex framing and QR profile lookup.

use std::marker::PhantomData;

use anyhow::{Result, anyhow, ensure};
use ragu::header::{Header, Suffix};
use ragu_circuits::polynomials::{ProductionRank, Rank};
use ragu_core::{
    drivers::{Driver, DriverValue},
    gadgets::Bound,
    pasta::Pasta,
};
use ragu_primitives::allocator::Allocator;
use serde::{Deserialize, Serialize};
use zcash_tachyon::{
    Anchor, EpochIndex, QrClassRoot, QrDiscriminant, QrProfile, Tachygram, TachygramSetCommit,
    pasta::Fp,
    stamp::proof::{PROOF_SYSTEM, pool, qr},
};

pub type Pcd<H> = ragu::Pcd<Pasta, ProductionRank, H>;

// ---------------------------------------------------------------------------
// Chain data: the `gettachyonblock` JSON, moved from devtool `chain.rs`.
// Validation lives in `chain.rs`.

/// A set of N roots needs N + 1 coefficients, even with mock proofs.
pub fn max_polynomial_members() -> usize {
    ProductionRank::num_coeffs() - 1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TachyonBlock {
    pub hash: String,
    pub previous_block_hash: String,
    pub height: u32,
    pub activation_height: u32,
    pub pool_height: u32,
    pub epoch: u32,
    pub epoch_length: u32,
    pub anchor_before: String,
    pub epoch_start_anchor: Option<String>,
    pub anchor_after: String,
    pub stamps: Vec<TachyonStamp>,
    /// Not part of `gettachyonblock`: devtool fills it from `getblock`.
    #[serde(default)]
    pub transactions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TachyonStamp {
    pub transaction_index: usize,
    pub txid: String,
    pub tachygram_set: String,
    pub tachygrams: Vec<String>,
}

pub fn bytes32(value: &str) -> Result<[u8; 32]> {
    hex::decode(value)?
        .try_into()
        .map_err(|_| anyhow!("expected 32 bytes"))
}

pub fn anchor(value: &str) -> Result<Anchor> {
    Ok(Anchor::read(&bytes32(value)?[..])?)
}

pub fn anchor_hex(value: Anchor) -> String {
    let mut bytes = [0; 32];
    value
        .write(&mut bytes[..])
        .expect("fixed-size anchor fits its encoding");
    hex::encode(bytes)
}

impl TachyonStamp {
    pub fn members(&self) -> Result<Vec<Tachygram>> {
        ensure!(
            self.tachygrams.len() <= max_polynomial_members(),
            "stamp exceeds Tachyon's polynomial capacity"
        );
        self.tachygrams
            .iter()
            .map(|s| Ok(Tachygram::read(&bytes32(s)?[..])?))
            .collect()
    }

    pub fn commitment(&self) -> Result<TachygramSetCommit> {
        Ok(TachygramSetCommit::read(
            &bytes32(&self.tachygram_set)?[..],
        )?)
    }
}

impl TachyonBlock {
    pub fn entry(&self) -> Result<Anchor> {
        self.epoch_start_anchor
            .as_deref()
            .map(anchor)
            .unwrap_or_else(|| anchor(&self.anchor_before))
    }
}

// ---------------------------------------------------------------------------
// Methods (design §4.2). Params are one named object. 32-byte values are
// lowercase hex of Tachyon wire bytes; hashes and txids are display-order hex;
// PCDs are `pcd_to_hex`.

pub const PROTOCOL_VERSION: u32 = 1;
/// Most nullifiers (consecutive epochs) one `oss_unspent` call covers.
pub const MAX_UNSPENT_NULLIFIERS: usize = 64;

/// `oss_info` params.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct InfoParams {}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoResult {
    pub protocol_version: u32,
    pub genesis_hash: String,
    pub activation_height: u32,
    pub epoch_length: u32,
    pub proof_system: ProofSystem,
    pub tip: Tip,
    pub archive_from_height: u32,
    pub evidence_ready_through: Option<u32>,
    pub node: NodeStatus,
}

/// The tachyon and ragu revs. A mismatch shows up as failed PCD verification.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProofSystem {
    pub tachyon: String,
    pub ragu: String,
    pub mock: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tip {
    pub height: u32,
    pub hash: String,
    pub epoch: u32,
    pub anchor: String,
    pub entry_anchor: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub height: u32,
    pub lag_blocks: u32,
}

/// `oss_block` params: `{"height": h}` or `{"hash": "…"}`. The result is the
/// exact `gettachyonblock` JSON ([`TachyonBlock`]).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockParams {
    Height(u32),
    Hash(String),
}

/// `oss_epoch` params.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct EpochParams {
    pub epoch: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochResult {
    pub epoch: u32,
    pub first_height: u32,
    pub last_height: Option<u32>,
    pub entry_anchor: String,
    pub final_anchor: Option<String>,
    pub next_entry_anchor: Option<String>,
    pub closing_hash: Option<String>,
    pub evidence: EpochEvidence,
}

/// `discriminant` and `profiles` are present only when `status` is `Ready`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct EpochEvidence {
    pub status: EvidenceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discriminant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<BucketProfile>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceStatus {
    Open,
    Building,
    Ready,
}

/// One sealed bucket: `{depth, bits, members}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct BucketProfile {
    #[serde(flatten, with = "QrProfileDef")]
    pub profile: QrProfile,
    pub members: u32,
}

/// `{depth, bits}`, serialized straight from Tachyon's `QrProfile`.
#[derive(Deserialize, Serialize)]
#[serde(remote = "QrProfile")]
struct QrProfileDef {
    depth: u32,
    bits: u32,
}

/// `oss_bucket` params.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct BucketParams {
    pub epoch: u32,
    #[serde(with = "QrProfileDef")]
    pub profile: QrProfile,
}

/// The sealed `QrBucket` PCD of the requested profile.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct BucketResult {
    pub pcd: String,
}

/// `oss_anchorspans` params.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnchorSpansParams {
    pub from: String,
}

/// `AnchorSpan` PCDs that lift a stamp anchored at `from` to `target`, in
/// order. Empty when `from == target`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnchorSpansResult {
    pub epoch: u32,
    pub target: String,
    pub spans: Vec<String>,
}

/// `oss_unspent` params: one nullifier per consecutive closed epoch from
/// `epoch_start`, 1 ≤ n ≤ [`MAX_UNSPENT_NULLIFIERS`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnspentParams {
    pub epoch_start: u32,
    pub nullifiers: Vec<String>,
}

/// One `ArbitraryUnspent` PCD over epochs `[epoch_start, epoch_next)`, from
/// `entry(epoch_start)` to `entry(epoch_next)`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnspentResult {
    pub pcd: String,
    pub epoch_start: u32,
    pub epoch_next: u32,
    pub anchor_start: String,
    pub anchor_next: String,
}

/// JSON-RPC `error.code` values. `Unavailable` and `Reorged` carry no data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// `data`: a message.
    InvalidParams = -32602,
    /// `data`: [`NotReadyData`]. Retryable.
    NotReady = 1001,
    /// `data`: [`SpentData`].
    Spent = 1002,
    /// The height or epoch is outside the archive.
    Unavailable = 1003,
    /// The requested anchor or epoch is no longer canonical.
    Reorged = 1004,
}

impl ErrorCode {
    pub const ALL: [Self; 5] = [
        Self::InvalidParams,
        Self::NotReady,
        Self::Spent,
        Self::Unavailable,
        Self::Reorged,
    ];

    pub fn code(self) -> i64 {
        self as i64
    }

    pub fn from_code(code: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }
}

/// An epoch is still open or its evidence is building.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotReadyData {
    pub retry_after_ms: u64,
    pub reason: String,
}

/// One of the requested values is published.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpentData {
    pub epoch: u32,
    pub height: u32,
    pub txid: String,
}

// ---------------------------------------------------------------------------
// PCDs.

/// The unit PCD that single-input fuse steps take as their right child.
pub fn trivial() -> Pcd<()> {
    PROOF_SYSTEM.bootstrap_proof().clone().carry(())
}

/// Devtool's `verify_proof`: compress, then `verify_compressed`. Run it on every
/// PCD that crosses a process boundary, and check its header fields too.
pub fn verify_pcd<H: Header<Fp>>(pcd: &Pcd<H>) -> Result<bool> {
    let compressed = PROOF_SYSTEM.compress(pcd, &mut rand::rng())?;
    Ok(PROOF_SYSTEM.verify_compressed(&compressed)?)
}

/// A header whose PCDs cross the OSS wire. At the pinned rev Tachyon decodes
/// `QrBucket` but not `AnchorSpan` or `ArbitraryUnspent`, so oss-core does.
pub trait WireHeader: Header<Fp> {
    /// Rebuilds the data from exactly the elements `Header::encode` emits.
    fn decode(elements: &[Fp]) -> Option<Self::Data>;
}

impl WireHeader for qr::QrBucket {
    fn decode(elements: &[Fp]) -> Option<Self::Data> {
        <Self as ragu::WireHeader<Fp>>::decode(elements)
    }
}

impl WireHeader for pool::AnchorSpan {
    fn decode(elements: &[Fp]) -> Option<Self::Data> {
        let [start, end] = <[Fp; 2]>::try_from(elements).ok()?;
        Some((Anchor(start), Anchor(end)))
    }
}

impl WireHeader for pool::ArbitraryUnspent {
    fn decode(elements: &[Fp]) -> Option<Self::Data> {
        let [anchor_start, epoch_start, epoch_next, anchor_next] =
            <[Fp; 4]>::try_from(elements).ok()?;
        Some((
            Anchor(anchor_start),
            EpochIndex::try_from(epoch_start).ok()?,
            EpochIndex::try_from(epoch_next).ok()?,
            Anchor(anchor_next),
        ))
    }
}

/// `H` under ragu's own mock PCD framing (same suffix and elements), so the
/// bytes equal what a native `ragu::WireHeader` impl for `H` would produce.
struct Wire<H>(PhantomData<H>);

impl<H: WireHeader> Header<Fp> for Wire<H> {
    const SUFFIX: Suffix = H::SUFFIX;
    type Data = H::Data;
    type Output = H::Output;

    fn encode<'dr, D: Driver<'dr, F = Fp>, A: Allocator<'dr, D>>(
        dr: &mut D,
        allocator: &mut A,
        witness: DriverValue<D, Self::Data>,
    ) -> ragu_core::Result<Bound<'dr, D, Self::Output>> {
        H::encode(dr, allocator, witness)
    }
}

impl<H: WireHeader> ragu::WireHeader<Fp> for Wire<H> {
    fn decode(elements: &[Fp]) -> Option<Self::Data> {
        H::decode(elements)
    }
}

/// The PCD's ragu mock wire bytes (`Pcd::to_bytes`), e.g. for a BLOB column.
pub fn pcd_to_bytes<H: WireHeader>(pcd: &Pcd<H>) -> Result<Vec<u8>> {
    let (proof, data) = pcd.clone().into_parts();
    proof
        .carry::<Wire<H>>(data)
        .to_bytes()
        .map_err(|e| anyhow!("cannot encode PCD: {e}"))
}

/// Decodes representation only. Check the header fields and [`verify_pcd`]
/// before using the result.
pub fn pcd_from_bytes<H: WireHeader>(bytes: &[u8]) -> Result<Pcd<H>> {
    let (proof, data) = Pcd::<Wire<H>>::from_bytes(bytes)
        .map_err(|e| anyhow!("invalid PCD: {e}"))?
        .into_parts();
    Ok(proof.carry(data))
}

/// Lowercase hex of [`pcd_to_bytes`]: the wire form of a PCD.
pub fn pcd_to_hex<H: WireHeader>(pcd: &Pcd<H>) -> Result<String> {
    Ok(hex::encode(pcd_to_bytes(pcd)?))
}

/// [`pcd_from_bytes`] of hex. Representation only: verify before use.
pub fn pcd_from_hex<H: WireHeader>(value: &str) -> Result<Pcd<H>> {
    pcd_from_bytes(&hex::decode(value)?)
}

// ---------------------------------------------------------------------------
// QR profiles, from devtool `proofs/epoch.rs`.

/// Every leaf of the capacity-bounded routing tree, including empty siblings,
/// as root-first sides. Queries absent from the epoch can still select those
/// empty profiles.
pub fn profiles(members: Vec<Tachygram>, discriminant: QrDiscriminant) -> Result<Vec<Vec<bool>>> {
    let mut pending = vec![(Vec::new(), members)];
    let mut leaves = Vec::new();
    while let Some((path, members)) = pending.pop() {
        if members.len() <= max_polynomial_members() {
            leaves.push(path);
            continue;
        }
        ensure!(
            path.len() < QrProfile::MAX_DEPTH,
            "epoch cannot be routed within Tachyon's polynomial capacity"
        );
        let shift = discriminant.at(u32::try_from(path.len())?);
        let (yes, no): (Vec<_>, Vec<_>) = members
            .into_iter()
            .partition(|m| QrClassRoot::of((*m).into(), shift).0);
        for (side, members) in [(false, no), (true, yes)] {
            let mut child = path.clone();
            child.push(side);
            pending.push((child, members));
        }
    }
    Ok(leaves)
}

/// The bucket profile that holds `value`: the first of `profiles` on its
/// routing path. `None` if the path meets none of them.
pub fn profile_of(
    value: Fp,
    discriminant: QrDiscriminant,
    profiles: &[QrProfile],
) -> Option<QrProfile> {
    let mut profile = QrProfile::ROOT;
    while !profiles.contains(&profile) {
        if profile.depth as usize == QrProfile::MAX_DEPTH {
            return None;
        }
        profile = profile.descend(QrClassRoot::of(value, discriminant.at(profile.depth)).0);
    }
    Some(profile)
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};
    use udon::field::Field;
    use zcash_tachyon::{TachygramSetPoly, witness};

    use super::*;

    /// Devtool's `routing_path`, kept as the oracle for `profile_of`.
    fn routing_path(
        mut members: Vec<Tachygram>,
        query: Tachygram,
        discriminant: QrDiscriminant,
    ) -> Result<Vec<bool>> {
        let mut path = Vec::new();
        while members.len() > max_polynomial_members() {
            ensure!(
                path.len() < QrProfile::MAX_DEPTH,
                "epoch cannot be routed within Tachyon's polynomial capacity"
            );
            let shift = discriminant.at(u32::try_from(path.len())?);
            let side = QrClassRoot::of(query.into(), shift).0;
            members.retain(|member| QrClassRoot::of((*member).into(), shift).0 == side);
            path.push(side);
        }
        Ok(path)
    }

    fn profile(path: &[bool]) -> QrProfile {
        path.iter().fold(QrProfile::ROOT, |p, &b| p.descend(b))
    }

    #[test]
    fn profiles_include_empty_siblings_and_follow_query_rules() -> Result<()> {
        let disc = QrDiscriminant(Fp::ONE);
        assert_eq!(profiles(Vec::new(), disc)?, vec![Vec::<bool>::new()]);
        assert_eq!(
            profile_of(Fp::from(7), disc, &[QrProfile::ROOT]),
            Some(QrProfile::ROOT)
        );
        assert_eq!(profile_of(Fp::from(7), disc, &[]), None);
        let members: Vec<_> = (1u64..)
            .map(Fp::from)
            .map(Tachygram::from)
            .filter(|m| QrClassRoot::of((*m).into(), disc.at(0)).0)
            .take(max_polynomial_members() + 1)
            .collect();
        let paths = profiles(members.clone(), disc)?;
        assert!(paths.contains(&vec![false]));
        assert!(paths.iter().any(|p| p.len() > 1));
        let leaves: Vec<_> = paths.iter().map(|p| profile(p)).collect();
        for query in (0u64..4).map(Fp::from) {
            let expected = profile(&routing_path(members.clone(), query.into(), disc)?);
            assert_eq!(profile_of(query, disc, &leaves), Some(expected));
        }
        Ok(())
    }

    #[test]
    fn seeded_pcd_round_trips_through_hex_and_verifies() -> Result<()> {
        let members = [Tachygram::from(Fp::from(7))];
        let (span, ()) = PROOF_SYSTEM.seed(
            &mut rand::rng(),
            pool::AnchorSpanSeed,
            witness::anchor_span_seed(((), ()), Anchor::default(), EpochIndex::new(0), &members),
        )?;
        let hex = pcd_to_hex(&span)?;
        let decoded = pcd_from_hex::<pool::AnchorSpan>(&hex)?;
        assert_eq!(decoded.data(), span.data());
        assert_eq!(pcd_to_hex(&decoded)?, hex);
        assert!(verify_pcd(&decoded)?);
        assert!(pcd_from_hex::<pool::ArbitraryUnspent>(&hex).is_err());

        // Pin the element layout of the other wire headers. These carry
        // made-up data, so they would not verify.
        let (proof, _) = span.into_parts();
        let unspent = proof.clone().carry::<pool::ArbitraryUnspent>((
            Anchor(Fp::from(1)),
            EpochIndex::new(2),
            EpochIndex::new(3),
            Anchor(Fp::from(4)),
        ));
        let back = pcd_from_hex::<pool::ArbitraryUnspent>(&pcd_to_hex(&unspent)?)?;
        assert_eq!(back.data(), unspent.data());
        let bridge = members
            .into_iter()
            .collect::<TachygramSetPoly>()
            .commit()
            .bridge()?;
        let bucket = proof.carry::<qr::QrBucket>((
            EpochIndex::new(2),
            Anchor(Fp::from(1)),
            Anchor(Fp::from(4)),
            QrDiscriminant(Fp::ONE),
            QrProfile::ROOT.descend(true),
            bridge,
        ));
        let hex = pcd_to_hex(&bucket)?;
        // Same bytes as Tachyon's own wire impl, so devtool's cached buckets match.
        assert_eq!(hex, hex::encode(bucket.to_bytes().unwrap()));
        assert_eq!(pcd_to_hex(&pcd_from_hex::<qr::QrBucket>(&hex)?)?, hex);
        Ok(())
    }

    /// Parses the sample and checks it serializes back to the same JSON.
    fn round_trip<T: Serialize + DeserializeOwned>(sample: Value) -> Result<T> {
        let parsed: T = serde_json::from_value(sample.clone())?;
        assert_eq!(serde_json::to_value(&parsed)?, sample);
        Ok(parsed)
    }

    const H: &str = "0100000000000000000000000000000000000000000000000000000000000000";
    const H2: &str = "0200000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn gettachyonblock_json_round_trips() -> Result<()> {
        let mut sample = json!({
            "hash": H, "previousBlockHash": H2, "height": 101, "activationHeight": 100,
            "poolHeight": 1, "epoch": 0, "epochLength": 4096, "finalized": false,
            "anchorBefore": H, "epochStartAnchor": null, "anchorAfter": H2,
            "stamps": [{"transactionIndex": 1, "txid": H, "tachygramSet": H2,
                        "tachygrams": [H, H2]}],
        });
        let block: TachyonBlock = serde_json::from_value(sample.clone())?;
        assert_eq!(block.entry()?, Anchor(Fp::ONE));
        assert_eq!(block.stamps[0].members()?.len(), 2);
        assert_eq!(anchor_hex(block.entry()?), H);
        sample.as_object_mut().unwrap().remove("finalized");
        sample["transactions"] = json!([]);
        round_trip::<TachyonBlock>(sample)?;
        Ok(())
    }

    #[test]
    fn every_method_message_round_trips() -> Result<()> {
        round_trip::<InfoParams>(json!({}))?;
        round_trip::<InfoResult>(json!({
            "protocolVersion": 1, "genesisHash": H, "activationHeight": 100, "epochLength": 4096,
            "proofSystem": {"tachyon": "088f4e1", "ragu": "839cf50", "mock": true},
            "tip": {"height": 5000, "hash": H, "epoch": 1, "anchor": H, "entryAnchor": H2},
            "archiveFromHeight": 100, "evidenceReadyThrough": 0,
            "node": {"height": 5001, "lagBlocks": 1},
        }))?;
        let info = round_trip::<InfoResult>(json!({
            "protocolVersion": 1, "genesisHash": H, "activationHeight": 100, "epochLength": 4096,
            "proofSystem": {"tachyon": "088f4e1", "ragu": "839cf50", "mock": true},
            "tip": {"height": 100, "hash": H, "epoch": 0, "anchor": H, "entryAnchor": H},
            "archiveFromHeight": 100, "evidenceReadyThrough": null,
            "node": {"height": 100, "lagBlocks": 0},
        }))?;
        assert_eq!(info.evidence_ready_through, None);

        assert_eq!(
            round_trip::<BlockParams>(json!({"height": 7}))?,
            BlockParams::Height(7)
        );
        round_trip::<BlockParams>(json!({"hash": H}))?;
        assert!(serde_json::from_value::<BlockParams>(json!({"height": 7, "hash": H})).is_err());

        round_trip::<EpochParams>(json!({"epoch": 3}))?;
        let open = round_trip::<EpochResult>(json!({
            "epoch": 3, "firstHeight": 12388, "lastHeight": null, "entryAnchor": H,
            "finalAnchor": null, "nextEntryAnchor": null, "closingHash": null,
            "evidence": {"status": "open"},
        }))?;
        assert_eq!(open.evidence.status, EvidenceStatus::Open);
        let ready = round_trip::<EpochResult>(json!({
            "epoch": 2, "firstHeight": 8292, "lastHeight": 12387, "entryAnchor": H,
            "finalAnchor": H2, "nextEntryAnchor": H, "closingHash": H2,
            "evidence": {"status": "ready", "discriminant": H, "profiles": [
                {"depth": 1, "bits": 0, "members": 0},
                {"depth": 2, "bits": 2, "members": 4096},
            ]},
        }))?;
        let profiles = ready.evidence.profiles.unwrap();
        assert_eq!(
            profiles[1].profile,
            QrProfile::ROOT.descend(true).descend(false)
        );
        round_trip::<EpochResult>(json!({
            "epoch": 2, "firstHeight": 8292, "lastHeight": 12387, "entryAnchor": H,
            "finalAnchor": H2, "nextEntryAnchor": H, "closingHash": H2,
            "evidence": {"status": "building"},
        }))?;

        let bucket =
            round_trip::<BucketParams>(json!({"epoch": 2, "profile": {"depth": 1, "bits": 1}}))?;
        assert_eq!(bucket.profile, QrProfile::ROOT.descend(true));
        round_trip::<BucketResult>(json!({"pcd": "abcd"}))?;
        round_trip::<AnchorSpansParams>(json!({"from": H}))?;
        round_trip::<AnchorSpansResult>(json!({"epoch": 3, "target": H2, "spans": ["ab", "cd"]}))?;
        round_trip::<AnchorSpansResult>(json!({"epoch": 3, "target": H, "spans": []}))?;
        round_trip::<UnspentParams>(json!({"epochStart": 1, "nullifiers": [H, H2]}))?;
        round_trip::<UnspentResult>(json!({
            "pcd": "abcd", "epochStart": 1, "epochNext": 3, "anchorStart": H, "anchorNext": H2,
        }))?;

        round_trip::<NotReadyData>(json!({"retryAfterMs": 1000, "reason": "epoch 3 is open"}))?;
        round_trip::<SpentData>(json!({"epoch": 2, "height": 9000, "txid": H}))?;
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_code(code.code()), Some(code));
        }
        assert_eq!(ErrorCode::NotReady.code(), 1001);
        assert_eq!(ErrorCode::InvalidParams.code(), -32602);
        assert_eq!(ErrorCode::from_code(-32601), None);
        Ok(())
    }
}
