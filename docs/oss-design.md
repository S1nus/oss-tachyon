# Tachyon Oblivious Synchronization Service (OSS): prototype design

Status: design for handoff, 2026-11-11. The companion task list is
[`oss-tasks.md`](oss-tasks.md).

- Revision 2 switches to the retroactive model and keeps the devtool
  self-prove option.
- Revision 3 applies the audit fixes:
  - remote sync waits for the OSS tip;
  - one spans request per payment;
  - the builder catches up from a cursor;
  - local spans start at the spend anchor;
  - a safe local bucket cache;
  - a workload gate that handles `null`;
  - a trimmed protocol;
  - recomputed `finalized`;
  - the span chunk-edge rule.
- Revision 4 applies the code-audit fixes:
  - same-epoch spends prove their spans locally (§4.4);
  - remote mode proves epochs below the OSS archive locally (§4.6);
  - an `oss_unspent` job keeps its slot until its proof ends.
- Revision 5 adds thin sync (§4.3): closed epochs sync without their
  tachygrams, and spends in them are looked up with `oss_tachygrams`. It
  reveals more to the OSS (§4.4), accepted until PIR hides those lookups.

Normative sources: the Tachyon book in `repos/tachyon/book/src/` (`proof-tree.md`
§Roles, `nullifiers.md` §Delegation, `anchor.md`, `tachygrams.md`) and the code at
the revisions below. The book's `zips/oss.md` is still an empty stub, so this
document is the working spec.

## 0. Pinned inputs

All repos are cloned read-only under `repos/`:

| Repo (cnode-agent/…) | Branch | Rev audited | Notes |
| --- | --- | --- | --- |
| zakura | `c/mock-wire+nu-tachyon` | `3e7cf2f` | node |
| zcash-devtool | `c/mock-wire+proof-cache` | `1716bff` | **one commit past** the pinned `6395778`: "prewarm closed epochs with durable single-flight evidence" |
| tachyon | `c/mock-decompress-clean` | `088f4e1` | `zcash_tachyon` crate + book |
| ragu | `c/mock-decompress` | `839cf50` | mock PCD backend |
| common | `c/mock-wire+driver` / `c/devtool-tachyon` | `b99212e` / `b20577b` | forked librustzcash crates; no OSS impact |
| tachyscan | `pr-1` | `66bbbd8` | explorer; no OSS impact |
| tachyon-workload | `main` | `cc1544b` | **one commit past** the pinned `f6c2dda`: "keep mining while closed-epoch evidence prewarms" |
| tachyon-faucet | `main` | `6f9522c` | drives its own devtool wallet |

Protocol constants that drive the design: `EPOCH_SIZE = 4096` blocks.
`NF_DERIVATION_WIDTH = 16` epochs per `NullifierDerive`. Polynomial capacity is
`2^13 − 1 = 8191` members (`ProductionRank`, R13). A mock PCD serializes two
8192-coefficient polynomials, so it is about 512 KiB.

## 1. What the OSS is

The book splits the proof tree across three parties (`proof-tree.md`, "Roles"):

- **Wallet**: runs every step that touches the note, `cm`, `psi`, or `mk`.
  These are `NoteSeed`, `NullifierDerive`/`Fuse`, `SpendableInit`,
  `QrSpendableInit`, `UnspentBind`, `SpendableLift`, the spend, output, merge
  and fee steps, and `StampLift`.
- **Sync service (OSS)**: runs every step that is note-independent or sees only
  opaque nullifier values. These are `AnchorSpanSeed`/`Fuse`, the `Summary*`
  steps, the QR routing steps (`QrStampIntakeSeed`, `QrEmptyIntakeSeed`,
  `QrIntakeMerge`, `QrIntakeSplit`, `QrSideSelect`, `QrSideDescend`,
  `QrBucketSeal`), the `EvidenceTree*` steps, and `QrUnspentInit`,
  `UnspentLift` and `UnspentFuse`. "It never sees a note, `cm`, `psi`, or `mk`."
  The book marks every sync-service step "possible" for a wallet too. That is
  what makes self-prove mode (§4.6) work.
- **Aggregator**: merges stamps. It is not part of this work.

The scaling comes from two kinds of sharing:

1. **Note-independent work is built once, ahead of time, for everyone.** An
   epoch's QR buckets (proofs that partition every tachygram of the epoch) and
   the anchor spans are the same for every wallet. The OSS builds them as
   blocks arrive and epochs close. Today each of about 33 devtool wallets on the
   box rebuilds or co-builds them through a shared SQLite file.
2. **Per-note work is done on request, retroactively.** A wallet comes online
   after time away and asks the OSS to prove its note's nullifiers absent
   across the epochs it missed. The OSS runs one step per epoch against buckets
   it already holds (`QrUnspentInit`, then `UnspentLift`) and returns a single
   `ArbitraryUnspent` segment. The wallet then runs one `UnspentBind` and one
   `SpendableLift`. **The OSS keeps no per-wallet state.**

**Model choice: retroactive, not delegated.** The blog post also describes a
delegated model, in which the wallet registers *future* nullifiers so the OSS
can advance its state while the wallet is offline. We do not build it, for two
reasons:

- It needs per-wallet state.
- It shows the OSS nullifiers that later appear on chain, so the OSS learns
  which transaction spent the note.

In the retroactive model, every nullifier the OSS receives is for a closed
epoch in which it was absent. Such a value can never be published later
(§4.4).

## 2. OSS ↔ Zakura node

### 2.1 Interface

**Decision.** The OSS is a separate process. It follows Zakura through the
JSON-RPC methods the node already has, so **no Zakura change is required.**

| Call | Use |
| --- | --- |
| `getblockchaininfo`, `getblockhash 0` | startup: network identity and node tip |
| `getbestblockhash` (polled every 1 s) | tip-change detection |
| `getblockhash h` | reorg walk-back |
| `gettachyonblock h` | **the data feed**. Returns hash, previous hash, height, `poolHeight`, `epoch`, `epochLength`, `finalized`, `anchorBefore`, `epochStartAnchor`, `anchorAfter`, and `stamps[{transactionIndex, txid, tachygramSet, tachygrams[]}]` |

`gettachyonblock` is the right feed because Zakura already parses the v7
Tachyon bundles and recomputes the anchor chain for each block
(`repos/zakura/crates/zakura-rpc/src/methods/types/tachyon.rs`). The OSS
re-validates each block once, for every wallet, using the `validate` port in
`oss-core` (§4.6).

Rejected alternatives:

- **Raw blocks** from the indexer gRPC (`GetBlockRange`, or the
  `NonFinalizedStateChange` stream). These would need Zakura's v7 transaction
  parser in the OSS.
- **Reading Zakura's RocksDB directly.** It couples the OSS to node internals.

Upgrade path for push notification: Zakura's indexer gRPC `ChainTipChange`
stream (`repos/zakura/crates/zakura-rpc/proto/indexer.proto`, enabled by
`rpc.indexer_listen_addr`) can replace polling. It is not needed for the
prototype.

### 2.2 Pruning facts (Zakura, as audited)

- **Kept forever in every storage mode:** headers, `txid → location`,
  per-height anchors (`tachyon_anchor_by_height`) and epoch anchors.
- **Expired at each epoch start** by
  `zakura_db/shielded.rs::prepare_tachyon_tachygram_batch`. The node deletes
  `tachyon_tachygrams` (tachygram → height) and `tachyon_anchors` (the anchor
  validity set) older than one epoch length. Consensus therefore keeps only
  about the current and previous epoch, which is exactly the two-epoch
  duplicate window.
- **`storage_mode = "pruned"`** deletes raw transaction bytes older than
  `tx_retention` blocks. The floor is 10,000 on Mainnet/Testnet kinds and
  `MAX_BLOCK_REORG_HEIGHT + 1 = 1001` on Regtest. Stamps and their tachygrams
  exist **only** inside raw transaction bytes, so after pruning
  `gettachyonblock` fails with "body pruned or not retained; use an archive
  node". Devtool already hits this case: "proof-service recovery is not
  implemented" (`wallet/sync.rs:28`).
- **An archive node** still has the bytes but no index by tachygram value. It
  has to re-parse every block for each query.

### 2.3 Who archives and indexes what

**Decision.** The OSS is the long-term Tachyon history archive and index. The
node stays the source of truth for chain selection, consensus windows, raw data
and the mempool.

| Data | Zakura node | OSS |
| --- | --- | --- |
| Raw blocks and transaction bytes | **Owner.** Archive mode keeps all; pruned mode keeps ≥ `tx_retention` blocks | Never stores raw bytes |
| Headers, `height ↔ hash`, `txid → location` | **Owner**, kept forever | Keeps `hash`/`prev`/`height` of ingested Tachyon-era blocks for reorg detection, and txids of stamp-bearing transactions only |
| Two-epoch tachygram set and anchor validity set (consensus) | **Owner** | — |
| Per-block Tachyon projection: anchors before/entry/after, stamps, tachygram-set commitments, tachygrams | Implicit in raw bytes; lost when pruned | **Owner (archive).** The verbatim `gettachyonblock` JSON per canonical block, from the OSS's start height onward |
| Tachygram value → (height, stamp) index | Two-epoch window only | **Owner**, full history. Used for the `Spent` check and creation lookup |
| Per-epoch QR evidence: private discriminant `R0`, sealed bucket PCDs, profile list (EvidenceTree later) | — | **Owner** |
| Anchor spans (current and previous epoch) | — | **Owner** |
| Per-request unspent segments | — | Computed and returned. **Nothing stored.** |
| Mempool, relay, `sendrawtransaction`, transparent UTXO and address indexes | **Owner** | — |
| Keys, notes, receipts, `NoteSecret`/`NoteNullifiers`/`NoteSpendable` lineages | — | — (wallet only) |

**Operational rule.** The OSS must ingest each block before the node prunes
it. When the OSS is live this happens immediately. For a cold start or a long
outage, use one of two paths:

- **(a) Checkpoint.** Start at an epoch boundary inside the retention window.
  The OSS can only serve evidence for epochs it saw in full.
- **(b) Backfill.** Pass `--backfill-url` pointing at an archive node for older
  ranges.

The OSS reports `node.lagBlocks`. It fails closed with a clear message if
continuing would leave a gap across pruned heights. On this box the node can
stay in archive mode, but the OSS must not depend on it.

Optional Zakura additions for later (not needed now):

- `gettachyonanchor h` that works on pruned bodies, to validate imported
  archives.
- A batch `gettachyonblocks start count` for faster backfill.

### 2.4 Reorgs and finality

- **Canonical chain only.** The OSS stores only the canonical chain. On a
  `prev_hash` mismatch it walks back with `getblockhash` and deletes rows above
  the fork, as devtool `sync_with_cache` does.
- **Hash-keyed evidence.** Epoch evidence is keyed by `closing_hash`, the hash
  of the epoch's last block. Reads ignore rows whose `closing_hash` is no
  longer canonical. Spans are rebuilt from the fork height.

  Reorg safety therefore needs no lease or fencing protocol. That is unlike
  devtool's `evidence.rs`, which needs one only because many processes share
  one file.
- **Eager build.** Zakura's finalization depth is 1000 blocks, about a quarter
  of an epoch, which is too long to wait. Evidence is built eagerly at the
  epoch boundary and rebuilt if a reorg replaces the closing block.
- **No reorg state for segments.** Unspent segments are computed per request
  from canonical buckets, so they need no reorg handling of their own.
- **The builder can never miss a block.** The follower's notifications are
  only wake-ups (a `tokio::sync::watch` of the archive tip). The builder keeps
  its own cursor, `meta.builder_height`, and reads every block it has not yet
  processed from the archive. On a reorg, the follower lowers
  `meta.rewind_from` to the lowest height it disconnected. Before going
  forward, the builder applies that rewind: it drops spans at or above that
  height and moves its cursor back.

## 3. What the OSS computes, and when

The OSS is one process. A follower task writes blocks. One builder thread does
the ahead-of-time proving, catching up from the archive with its cursor
(§2.4). The RPC server reads through read-only SQLite connections and runs
request proving on a small blocking pool. A `tokio::sync::watch` channel
carries tip and readiness.

1. **Ahead of time, for each canonical block in the open epoch E**
   1. Archive the block JSON and index its tachygrams.
   2. Extend E's **anchor-span chunks**: `AnchorSpanSeed` per stamp, then
      `AnchorSpanFuse` into the tail chunk. Start a new chunk once the tail
      holds 8191 members.
   3. *(Optional, T6)* Seed QR stamp intakes (`QrStampIntakeSeed`) with E's
      private discriminant, so less work remains at close.
2. **Ahead of time, at epoch close** (the first block of E+1 is ingested)
   1. Route all of E's tachygrams into capacity-bounded buckets and seal each
      one (`oss_core::build_epoch`).
   2. Store each bucket PCD and the profile list. Publish `R0` and the
      profiles.
   3. Mark E ready (`evidenceReadyThrough = E`).
3. **On request: `oss_unspent{epochStart, nullifiers[n]}`**
   1. Reject the request as `Spent` if any of the values is in the tachygram
      index.
   2. Otherwise run `oss_core::prove_unspent`. For each epoch it looks up the
      bucket for `profile_of(nf_e)`. It runs `QrUnspentInit` on the first and
      `UnspentLift` on each later one: `n` steps in total. Return the segment.
   3. Answer `oss_bucket` and `oss_anchorspans` from storage.

**Discriminant.** The OSS samples `R0` at random for each epoch and keeps it
private until the epoch is sealed. This follows the book: a builder keeps `R0`
private so it can route in flight and nobody can grind tachygrams into one
bucket. A wallet needs `R0` only after close, to compute the profile of its own
`cm`. Devtool's self-prove mode keeps the fixed `R0 = 1`: it is deterministic,
nothing has to be persisted, and only that wallet uses it.

**Left out of the prototype on purpose:**

- `EvidenceTree*`. It saves storage by keeping one root proof instead of every
  bucket proof. At about 40–64 buckets × 512 KiB per full epoch, storage is not
  yet a problem.
- `Summary*`. It is an alternative intake path.
- OSS-side `StampLift`.
- The delegated model (§1).

## 4. Wallet ↔ OSS protocol

### 4.1 Transport and encoding

- **Transport.** JSON-RPC 2.0 over HTTP POST `/`, with the same envelope as
  Zakura. Devtool's existing `rpc::Rpc` client (cookie auth, cancellation,
  bounded body) is reused with a second URL. Params are one named object.
- **Server.** axum 0.8 with tower-http 0.6 `CompressionLayer` (gzip), the same
  stack as tachyscan's backend. Clients enable reqwest's `gzip` feature.
- **Auth.** Optional HTTP Basic auth with a cookie file, as in Zakura. Default
  is to bind to localhost.
- **32-byte protocol values** (anchors, tachygrams/nullifiers, discriminant,
  tachygram-set commitments): lowercase hex of canonical Tachyon wire bytes,
  the same as `gettachyonblock`. Block hashes and txids are hex in display
  order, as in Zakura.
- **PCDs**: lowercase hex of `ragu::Pcd::to_bytes()` (mock wire format). One is
  about 1 MiB of hex. Polynomials are zero-padded and gzip well. No response
  carries more than about 10 PCDs, and the client body limit is raised to
  64 MiB for the OSS URL.
- **Version.** `oss_info.proofSystem` names the tachyon and ragu revs. A
  mismatch shows up as a PCD verification failure, and the wallet reports both
  revs.

### 4.2 Methods

All methods are stateless. Results for closed epochs can be cached.

| Method | Params | Result |
| --- | --- | --- |
| `oss_info` | `{}` | `{genesisHash, activationHeight, epochLength, proofSystem:{tachyon,ragu,mock}, tip:{height,hash,epoch,anchor,entryAnchor}, archiveFromHeight, evidenceReadyThrough:int\|null, node:{height,lagBlocks}}` |
| `oss_block` | `{height, thin?}` | The stored `gettachyonblock` JSON for that canonical block, served from the raw stored text. `finalized` is recomputed as true when the block is at least 1000 blocks below the tip. It works for heights the node has pruned. Devtool's `chain::Block` already deserializes it. With `thin: true`, every stamp's `tachygrams` is empty; `tachygramSet` still folds into the anchors (`chain::validate_thin`). |
| `oss_tachygrams` | `{values:[hex…]}`, 1 ≤ n ≤ 64 | `{heights:[int\|null…]}`: the lowest archived height that publishes each value. Thin sync's spend check; the lookup PIR would replace. |
| `oss_epoch` | `{epoch}` | `{epoch, closingHash\|null, evidence:{status:"open"\|"building"\|"ready", discriminant?, profiles?:[{depth,bits,members}]}}`. `discriminant` and `profiles` appear only when `ready`. |
| `oss_bucket` | `{epoch, profile:{depth,bits}}` | `{pcd}`: the sealed `QrBucket` PCD of that profile |
| `oss_anchorspans` | `{from:[anchor…]}`: one per spend, all in the same epoch (current or previous), 1 ≤ n ≤ 64 | `{epoch, target, chunks:[pcd…], start:[int\|null…]}`, read in **one** consistent read so every spend lifts to the same `target`. `from[i]` lifts along `chunks[start[i]..]` in order. `start[i]` is null when `from[i] == target`. Each chunk PCD appears once. **Chunk choice:** if `x` is a chunk's start anchor, that chunk is chosen; otherwise the chunk that holds `x` as a member. An anchor that is already in the archive but not yet reached by the span builder returns `NotReady`. |
| `oss_unspent` | `{epochStart, nullifiers:[hex…]}`: one value per consecutive epoch, 1 ≤ n ≤ 64, all closed | `{pcd, epochStart, epochNext, anchorStart, anchorNext}`: one `ArbitraryUnspent` PCD covering epochs `[epochStart, epochStart+n)`, half-open in anchor space from `entry(epochStart)` to `entry(epochStart+n)` |

Errors use the JSON-RPC `error.code`, with `error.data` as shown:

| Code | Name | `data` |
| --- | --- | --- |
| -32602 | invalid params | message |
| 1001 | NotReady | `{retryAfterMs, reason}`. An epoch is still open, its evidence is building, or the span builder has not reached the anchor yet. Retryable. |
| 1002 | Spent | `{epoch, height, txid}`: one of the values is published |
| 1003 | Unavailable | Outside the archive, or no longer canonical (reorged away) |

Limits (configurable): 64 nullifiers per `oss_unspent` call, 64 anchors per
`oss_anchorspans` call, a bounded number of concurrent `oss_unspent` jobs,
64 MiB response.

### 4.3 Wallet flows (devtool, in either mode)

The flow is the same whether the wallet uses the OSS or proves locally (§4.6).
Only the source of buckets, segments and spans changes.

**`spendable(note)`** for note `cm`, created in epoch `c`, at current epoch
`t`:

| Lineage state | Action |
| --- | --- |
| Cached lineage at epoch `t` | Done. Verify and reuse it, as today. |
| No lineage and `c == t` | `SpendableInit` on the creation block data. |
| No lineage and `c < t` | Get `seg = unspent(c, [nf_c])`. Run `UnspentBind` with a derivation covering `c`. Get the bucket holding `cm` with `bucket(c, profile_of(cm))`. Run `QrSpendableInit`. The lineage is now at `entry(c+1)`. |
| Lineage at `k < t` | In pages of at most 64 epochs, get `seg = unspent(k, [nf_k … nf_{t−1}])`. Build a derivation covering the page: `NullifierDerive` per 16-epoch window, then `NullifierFuse`. Run `UnspentBind` and `SpendableLift`. The lineage is now at `entry(t)`. Cache it. |

`Spent` (or a local tachygram-index hit) marks the note spent.

Spent detection for balances reads the wallet's synced tachygram index
(`refresh_note_nullifiers`), and asks `oss_tachygrams` about epochs synced
thin (below).

**Spend.**

1. Make **one** spans request, listing every spendable's anchor. Use the
   returned `target` as the plan anchor. Each spend's `ActionLift` uses
   `chunks[start[i]..]`. Ask the OSS only when every anchor is the current
   epoch's entry anchor; otherwise prove the spans locally (§4.4).
2. Prove the stamp at that anchor. Every spend reaches it through `ActionLift`, so the stamp is already at `target`.
3. No `StampLift` is needed. Devtool deleted `lift_stamp` (T7a).
4. Sign, then `sendrawtransaction` to the **node**.

Separate requests per spend are forbidden: a block arriving between them would
give different targets, and the payment would fail to prove.

**Sync in remote mode.**

1. Wait, for at most 10 s, until `oss_info.tip.height` reaches the node's tip.
2. Sync up to the lower of the two tips. If the OSS tip is lower, report
   `caught_up: false`.
3. Read heights from `archiveFromHeight` up to the OSS tip with `oss_block`.
   Read any lower heights from the node's `gettachyonblock`.
4. **Thin sync.** A height inside the OSS archive whose epoch is already
   closed is read with `oss_block {thin: true}`, unless the command is a
   `--from-height` rescan. The current epoch, and everything below the
   archive, stays full: same-epoch spans and local proving need it.
   - A located receipt's block comes from the node in full and replaces the
     thin copy. The thin copy is still requested, so the OSS sees no gap at
     the note's height.
   - Once a thin epoch is complete, each note's nullifier for it goes to
     `oss_tachygrams`, one request per note. A claimed spend is checked in
     its block, fetched in full and kept; a false claim would hide the note.
   - Local proving refuses a thin epoch. A wallet that synced thin needs the
     OSS for those epochs.

**When it runs.** Lineages advance lazily at payment time, as today. An
optional `sync` flag can advance them eagerly, so the first payment after a
long absence is fast.

**Wallet MUST:**

- Verify every PCD it receives (`verify_proof` = compress and
  `verify_compressed`, as devtool does today).
- Check that header fields equal what it asked for: epochs and anchors.
- Check that `anchorNext` of the newest segment equals its synced entry anchor
  for epoch `t`.
- Treat `NotReady` as retry-after. If the wallet gives up (bounded retries), it exits non-zero, and stderr contains the exact phrase `OSS not ready`. The workload keys on that phrase.

### 4.4 Trust and privacy

- **Integrity.** Integrity is cryptographic. Every OSS artifact is a PCD that
  the wallet verifies, and `UnspentBind` checks the tested values against the
  wallet's own derivation (`nullifiers.md` §Delegation). A malicious OSS can
  only withhold service, or claim `Spent` falsely; the wallet can check a
  `Spent` claim in its own synced tachygram index, or, for an epoch synced
  thin, in that block fetched in full. Wrong anchors cannot produce
  a spend that consensus accepts.
- **The key property of the retroactive model.** Consensus publishes `nf_e`
  only in epoch `e` (as the current nullifier) or in `e−1` (as the next one).
  `oss_unspent` accepts only closed epochs and answers `Spent` if a value is
  already published. So a successful request means none of its values will
  ever appear on chain. The OSS cannot link anything it was shown to a later
  spend. Thin sync's `oss_tachygrams` gives this up for spends the wallet
  did not see (below).
- **What the OSS still learns:**
  - Per request, a run of nullifier values from one note. These are linkable
    to each other, not to `cm`.
  - Roughly when the wallet came online.
  - The approximate bucket that holds a wallet's `cm` in its creation epoch,
    from `oss_bucket`. That narrows `cm` to roughly 8k members.
  - A spans request, shortly before the transaction is published, tells the
    OSS that a closed-epoch note is about to be spent. Wallets send only the
    epoch's entry anchor, which every such note shares. A note created in
    the current epoch starts right after its creation stamp, which would name
    the creating transaction, so its spans are always proven locally.
  - Network metadata linking a client's requests.
  - **Thin sync** adds more. These are accepted until PIR hides them:
    - `oss_tachygrams` gets a note's nullifiers for every thin epoch since
      its creation. A published one tells the OSS which transaction spent the
      note, so a spend made elsewhere (a restored wallet, a second copy) is
      linked to this wallet.
    - The run of `oss_block` heights shows where the wallet's history starts
      and when it is online.
    - Receipts' blocks come from the node, which learns them (as before).
  - Devtool prints a `privacy warning:` line on stderr for every OSS request
    that carries wallet-specific values. `-v` logs every OSS request and its
    response time.

  Future mitigations: Tor, requesting the bucket and the segments in separate
  sessions, and self-prove mode for sensitive users.
- **What the OSS never learns:** keys, `psi`, `mk`, `cm`↔`nf` links, values,
  and balances.
- **DoS.** A request costs at most 64 proof steps. The prototype caps
  concurrent jobs and offers optional cookie auth.

### 4.5 Spend detection

The wallet detects spends of its own notes locally. It derives each note's
per-epoch nullifier and looks it up in its synced tachygram index
(`refresh_note_nullifiers`).

The OSS answers `Spent` on `oss_unspent`, from its archive index. That also
covers a spend in epoch `e−1` that published `nf_e` as its "next" nullifier.
For an epoch synced thin, the index lacks the tachygrams, so the wallet asks
`oss_tachygrams` instead (§4.3); `nf_e` is published only in epochs `e−1` and
`e`, so one lookup per complete thin epoch is final.

### 4.6 Self-prove mode: devtool does not require an OSS

**Crate split.**

- **`oss-core`** is a pure library with no tokio, axum or SQLite. Both the OSS
  server and devtool use it. It contains:
  - the protocol types and `pcd_to_hex`/`pcd_from_hex`
  - `validate` (block checks)
  - `profile_of`
  - `build_epoch` (QR routing, merging and sealing)
  - `prove_unspent(epoch_start, nfs, bucket_lookup)`
  - `SpanBuilder` (seed and fuse spans, return chunks)

  Most of this code moves out of devtool's `proofs/epoch.rs`, `proofs.rs` and
  `chain.rs`.
- **`oss`** is the server: store, follower, builder and RPC.

**Devtool.** A `Prover` value selects the backend. The flow in §4.3 is written
once against three operations:

```text
unspent(epoch_start, nfs) -> Pcd<ArbitraryUnspent> | Spent
bucket_for(epoch, value)  -> Pcd<QrBucket>
spans(from)               -> (target, Vec<Pcd<AnchorSpan>>)
```

| | Local (no OSS; default when no OSS URL is set) | Remote (`--oss-url`) |
| --- | --- | --- |
| Block data for sync | node `gettachyonblock` (as today) | `oss_block` within the OSS archive, the node below it (§4.3), so it keeps working after the node prunes |
| Buckets | `oss_core::build_epoch` with `R0 = 1` on first need. All of the epoch's buckets are written to the wallet's own `epoch_proofs` table. A cached row is used only if `verify_pcd` passes and its header epoch, `anchor_start` and `anchor_next` equal the current chain's `entry(e)` and `entry(e+1)`; otherwise it is deleted and rebuilt. | `oss_epoch` + `oss_bucket` |
| Unspent segment | `oss_core::prove_unspent` with the local bucket lookup | `oss_unspent`. On `Unavailable` (below the OSS archive), buckets and segments are proven locally, as in the Local column |
| Spans | `oss_core::SpanBuilder::new(epoch, start)`, started at the earliest spend anchor (not the epoch entry), over the wallet's blocks, on demand (as today) | `oss_anchorspans` (one call) when every spend anchor is the epoch entry; local otherwise (§4.4) |
| Spent detection | local tachygram index | local tachygram index; `oss_tachygrams` for epochs synced thin (§4.3) |

**Deleted in both modes:** the cross-process evidence sharing (`evidence.rs`)
and the prewarm commands. Sharing across wallets is the OSS's job. Many
self-proving wallets on one box would each build their own evidence, as before
the shared-cache fix. That is acceptable for a fallback.

## 5. Audit: devtool and workload code the OSS should own

### 5.1 zcash-devtool (`repos/zcash-devtool/crates/tachyon-wallet/src`)

| Code | What it does today | Disposition |
| --- | --- | --- |
| `evidence.rs` (all 463 lines) | Cross-process SQLite single-flight cache for bucket proofs, with leases, heartbeat thread, generation fencing and `Cargo.lock`-hash namespace | **Delete.** Cross-wallet sharing is now the OSS. |
| `proofs/epoch.rs` (all 1087 lines): `routing_path` :30, `profiles` :267, `Epoch::read/chunks/build/get` :297–640, `bucket` :640, `prewarm` :677 | Every wallet routes, merges and seals every closed epoch's QR buckets. Fixed `R0 = 1`. | **Move to `oss-core::build_epoch`**, used by the OSS (random `R0`) and by local mode (`R0 = 1`). Drop the intake-level cache (`epoch_nodes`) and `prewarm`. |
| `proofs.rs::spendable` :74 and `lift_closed_epoch` :241 | One `QrUnspentInit` plus `UnspentBind` plus `SpendableLift` per closed epoch per note | **Rewrite once** against `Prover` (§4.3): one composed segment per page, then one bind and one lift. `QrUnspentInit` moves into `oss-core::prove_unspent`. |
| `proofs.rs::anchor_spans` :298 | Each payment re-proves `AnchorSpanSeed`/`Fuse` for every stamp from the lineage anchor to the tip | **Move to `oss-core::SpanBuilder`.** The OSS maintains spans incrementally; local mode builds them on demand. `lift_stamp` :372 is deleted, because the stamp is proven at the spans target. |
| `chain.rs` `Block::validate*` :89–200 | Each wallet validates every block's anchor chain | **Move to `oss-core`.** The OSS and devtool use one copy; devtool's `Block` becomes a re-export. |
| `store.rs` tables `epoch_proofs` / `epoch_nodes`, `check_evidence_identity` :418 | Per-wallet proof caches, plus the shared-cache identity check | Keep `epoch_proofs` as the local-mode bucket cache. Stop using `epoch_nodes`. Delete `check_evidence_identity`. |
| `store.rs` tables `blocks`, `tachygrams`, `block_transactions`; `wallet/sync.rs` | Per-wallet chain history and contiguous sync | **Keep**: local mode needs it, and it drives spent detection. In remote mode, block data comes from `oss_block`. A thin sync for remote mode is deferred (T8). |
| `wallet.rs::refresh_note_nullifiers` :629 | Wallet-side spend detection | **Keep, unchanged** |
| `wallet.rs::SyncManyCommand` :486 | In-process shared block cache across wallets | Keep, unchanged; it is orthogonal |
| `wallet.rs` `Operation::{PrewarmEpochs, EvidenceGeneration}` :271–295 | CLI hooks for the workload's prewarm | **Delete** |
| `tests.rs` :723–880 (prewarm, `seventeen_wallet_processes_share_verified_epoch_proofs`), `evidence.rs` tests | Cross-process evidence tests | **Delete.** The remaining closed-epoch tests keep running in local mode and gain remote-mode twins. |

**Stays in the wallet:**

- Keys, receipts and notes.
- `NoteSeed`, `NullifierDerive` and `NullifierFuse`.
- `SpendableInit`, `QrSpendableInit`, `UnspentBind` and `SpendableLift`.
- Spend, output, merge and fee proving, `ActionLift` and `StampLift`.
- Signing, transparent funding (node RPC), submission (node RPC) and the
  operation journal.
- Block sync and spent detection.
- PCD verification of everything it receives.

### 5.2 tachyon-workload (`repos/tachyon-workload/tachyon_workload`)

| Code | What it does | Disposition |
| --- | --- | --- |
| `runner.py::evidence_ready` :245, `prewarm` :222, `evidence_token` :242, and the prewarm-round and defer logic in the mining loop | At each epoch boundary, pauses payments while every wallet syncs and runs `prewarm-epochs`, then checks the evidence generation | **Delete.** Replace with one gate before planning payments: pass when `epoch == 0`, otherwise require `evidenceReadyThrough` to be non-null and `>= epoch − 1`. Devtool also retries `NotReady`. |
| `runtime.py::Devtool.prewarm` :139, `prewarm_slots` :132, `TACHYON_EVIDENCE_DIR` :147 | Shared evidence directory and builder concurrency | **Delete.** Pass `TACHYON_OSS_URL` instead. |
| `test_prewarm.py` (318 lines), README "Epoch-close prewarm" | — | **Delete or replace** |
| config | — | Add `oss_url` |

tachyon-faucet: its devtool wallet gains an optional `oss_url`. Without it, the
wallet self-proves. Zakura, tachyon, ragu, common and tachyscan need no changes
for the prototype.

## 6. Size estimate

| Component | Estimate |
| --- | --- |
| `oss-core` | About 1.2k lines, mostly moved from devtool |
| `oss` server | About 1.5k lines |
| Devtool | About −1.7k lines (evidence sharing, prewarm and the per-epoch loop removed; `epoch.rs` moved out) and about +0.4k lines (the `Prover` choice and the remote client) |
| Workload | About −0.5k lines |
