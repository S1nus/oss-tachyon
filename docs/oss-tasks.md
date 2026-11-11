# OSS prototype: task list for subagents

Who runs it: Claude Code Opus 5.5 subagents with the ponytail plugin at the
`full` level. Contract: [`oss-design.md`](oss-design.md) (revision 2:
retroactive model, devtool self-prove kept). If a task here disagrees with the
design, the design wins. If you must change it, edit the design doc in the same
change and say so in your report. Standing rules are in
[`../CLAUDE.md`](../CLAUDE.md).

## Waves

```
Wave 1  T0 scaffold: oss-core protocol types + store schema    (1 agent, blocking)
Wave 2  T1 validate + follower/archive   T2 build_epoch + evidence store   T3 SpanBuilder + span store
                                                                           (3 agents, parallel worktrees)
Wave 3  T4 prove_unspent (needs T2)      T5 server + binary (needs T1–T3; finish after T4)
Wave 4  T7 devtool Prover local+remote (needs T4, T5)   T6 in-flight intakes (optional)
Wave 5  T9 workload + faucet (needs T7)                 T8 remote-mode thin sync (optional, deferred)
Wave 6  T10 deploy + measure on the box (human in the loop)
```

Layout: `oss/crates/oss-core` is the pure library, with no tokio, axum or
SQLite; devtool depends on it too. `oss/crates/oss` is the server.

File ownership keeps the parallel tasks from colliding. Shared files
(`lib.rs`, `Cargo.toml`) only get `mod` and dependency lines appended.

| Task | `oss-core/src` | `oss/src` |
| --- | --- | --- |
| T0 | `lib.rs`, `protocol.rs` | `store.rs` (schema) |
| T1 | `chain.rs` | `follower.rs`, `archive.rs`, `testing.rs` |
| T2 | `evidence.rs` | `epochs.rs` |
| T3 | `spans.rs` | `span_store.rs` |
| T4 | `unspent.rs` | — |
| T5 | — | `server.rs`, `main.rs`, `config.rs`, `unspent_service.rs` |

Every task finishes with:

- `cargo fmt`, `cargo clippy --all-targets` (no new warnings) and its tests
  passing.
- A report of the files changed, the tests added, and what was skipped.
- One closing line on the risks the next task must know about.

---

## T0: Scaffold, protocol types, store schema

**Goal.** A building workspace that freezes the wire contract and the
database schema, so wave 2 can run in parallel.

1. **Repo setup.** Run `git init` at the repo root.
   Add a `.gitignore` covering `repos/`, `target/`, `*.sqlite*` and
   `.claude/settings.local.json`. Commit `CLAUDE.md` and `docs/`.
2. **Workspace.** Create `oss/` with `rust-toolchain.toml` (`1.97.1`, clippy,
   rustfmt) and edition 2024. It has two crates:
   - `crates/oss-core` (lib).
   - `crates/oss` (lib + bin `tachyon-oss`).
3. **Dependencies.** Copy them **exactly** from
   `repos/zcash-devtool/crates/tachyon-wallet/Cargo.toml`: `zcash_tachyon` (git
   rev `088f4e1…`), `ragu` (`features=["mock"]`), `ragu_circuits`, `ragu_core`
   (rev `839cf50…`), `zakura-pasta-curves`, `zakura-udon`, `serde`,
   `serde_json`, `hex`, `anyhow` and `rand 0.10`.
   - Only `oss` adds: `rusqlite 0.37`, `tokio` (rt-multi-thread, macros,
     signal, sync, time), `axum 0.8`, `tower-http 0.6` (`compression-gzip`),
     `reqwest 0.12` (json, rustls-tls, gzip) and `clap 4` (derive).
   - Add nothing else.
4. **`oss-core/src/protocol.rs`.** Contains:
   - Every request and result struct in design §4.2, using serde `camelCase`.
   - `TachyonBlock` and `TachyonStamp`, matching the `gettachyonblock` JSON.
     Move them from `repos/zcash-devtool/.../chain.rs` (`Block`/`Stamp`)
     **without** the validation logic, plus the helpers `members()`,
     `commitment()`, `entry()`, `bytes32()`, `anchor()` and `anchor_hex()`.
   - An `ErrorCode` enum with the codes in design §4.2.
   - `pcd_to_hex` and `pcd_from_hex`, generic over the header.
   - `verify_pcd`, the devtool `verify_proof`: compress, then
     `verify_compressed`.
   - `profile_of(value, discriminant, profiles: &[QrProfile]) -> Option<QrProfile>`.
     Port it from devtool `proofs/epoch.rs::routing_path` and `profiles`.
5. **`oss/src/store.rs`.** `open(path)` with WAL mode and this schema:

   ```sql
   CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS blocks(height INTEGER PRIMARY KEY, hash TEXT NOT NULL UNIQUE,
     prev_hash TEXT NOT NULL, epoch INTEGER NOT NULL, json TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS tachygrams(tg BLOB NOT NULL, height INTEGER NOT NULL,
     stamp INTEGER NOT NULL, PRIMARY KEY(tg, height));
   CREATE INDEX IF NOT EXISTS tachygrams_by_height ON tachygrams(height);
   CREATE TABLE IF NOT EXISTS epochs(epoch INTEGER PRIMARY KEY, discriminant BLOB NOT NULL);
   CREATE TABLE IF NOT EXISTS evidence(epoch INTEGER NOT NULL, closing_hash TEXT NOT NULL,
     profiles TEXT NOT NULL, PRIMARY KEY(epoch, closing_hash));
   CREATE TABLE IF NOT EXISTS buckets(epoch INTEGER NOT NULL, closing_hash TEXT NOT NULL,
     depth INTEGER NOT NULL, bits INTEGER NOT NULL, members INTEGER NOT NULL, pcd BLOB NOT NULL,
     PRIMARY KEY(epoch, closing_hash, depth, bits));
   CREATE TABLE IF NOT EXISTS spans(epoch INTEGER NOT NULL, chunk INTEGER NOT NULL,
     start_anchor BLOB NOT NULL, end_height INTEGER NOT NULL, members BLOB NOT NULL,
     pcd BLOB NOT NULL, PRIMARY KEY(epoch, chunk));
   ```

**Accept when:**

- `cargo build` and `cargo test` pass.
- Every design §4.2 message passes a serde round-trip test against a
  hand-written JSON sample.
- The ported devtool test `profiles_include_empty_siblings_and_follow_query_rules`
  passes against `profile_of`.
- `pcd_from_hex(pcd_to_hex(p))` round-trips a real seeded PCD that passes
  `verify_pcd`.

---

## T1: Block validation, node follower, Tachyon archive

**Goal.** Follow Zakura, validate each block once, and keep the canonical
Tachyon history with its indexes.

1. **`oss-core/src/chain.rs`.** Port `Block::validate`, `validate_checkpoint`
   and `validate_inner` from devtool `chain.rs` as functions over
   `TachyonBlock`. Keep every check. Devtool will re-export these in T7.
2. **`oss/src/follower.rs`.** A node JSON-RPC client using reqwest and optional
   cookie Basic auth. Model it on devtool `rpc.rs`, including the bounded body
   and the rejection of credentials in the URL. The loop:
   - Poll `getbestblockhash` every 1 s.
   - Fetch `gettachyonblock` for each missing height and validate it against
     its parent.
   - In one transaction per block, insert into `blocks` (the verbatim JSON) and
     `tachygrams`.
   - On a parent mismatch, walk back with `getblockhash` and delete rows above
     the fork. In the **same transaction**, set `meta.rewind_from` to the
     lower of its current value and `fork_height + 1`.
   - After each commit, publish the archive tip height on a
     `tokio::sync::watch<u32>`. It is only a wake-up; there are no
     per-block events to lose (design §2.4).
   - Store network identity in `meta`: the genesis hash and the activation
     height and hash. Refuse to start on a mismatch.
   - Start from the activation height. Alternatively, `--start-height` must be
     an epoch boundary and uses `validate_checkpoint`.
   - With `--backfill-url`, read heights below the node's retained range from
     that node.
   - If the node answers "pruned" for a needed height and no backfill is
     configured, stop with an actionable error. Never skip a height.
3. **`oss/src/archive.rs`.** Read API:
   - `tip()`, `block(height)`, `block_json(height)` (the raw stored text, for
     `oss_block`), `hash(height)`.
   - `epoch_blocks(epoch)`: the epoch's blocks plus the next epoch's first
     block, if present.
   - `closing_hash(epoch)`, `entry_anchor(epoch)`.
   - `tachygram_heights(&[Fp]) -> Vec<(Fp, height, stamp)>`.
4. **`oss/src/testing.rs`** (a `testing` feature, also enabled under
   `cfg(test)`):
   - `FakeNode`: port the JSON-RPC fake from
     `repos/zcash-devtool/crates/tachyon-wallet/src/wallet/tests.rs` (`Node`,
     `NodeData`, including `pruned_heights`). Keep only `getbestblockhash`,
     `getblockhash`, `getblockcount`, `getblockchaininfo`, `gettachyonblock`,
     plus `reorg(from, new_blocks)` and `prune_below(h)`.
   - Synthetic chain builders: port `genesis()`, `mine()` and `populate_epoch()`
     from devtool `src/tests.rs`.

**Tests:**

- Ingest more than 2 epochs.
- Reorg inside an epoch, and across an epoch boundary.
- A pruned gap fails closed, and succeeds with a backfill node.
- Checkpoint start.
- Restart resumes without re-fetching.
- Network-identity mismatch is refused.

---

## T2: Epoch evidence (QR buckets)

**Goal.** Seal every `QrBucket` of a closed epoch. The same pure function
serves the OSS (random `R0`) and devtool's local mode (`R0 = 1`).

1. **`oss-core/src/evidence.rs`.** Port
   `repos/zcash-devtool/crates/tachyon-wallet/src/proofs/epoch.rs`:
   `Intake::{route, merge}`, `profiles`, `Epoch::{read, chunks, build}`.
   - Keep the empty-span merging and the capacity checks.
   - Drop `evidence::Cache` and its leases, `prewarm`, every intake-cache
     helper (`load_intake`, `put_intake`, `merge_cached`, `cached_route`),
     `Control`, and the legacy `bucket_proof` promotion.
   - Signature:
     `build_epoch(blocks: &[TachyonBlock], predecessor: Anchor, r0: QrDiscriminant)
     -> Result<Vec<(QrProfile, Pcd<QrBucket>, usize /*members*/)>>`.
     `blocks` is the epoch's blocks plus the next epoch's first block.
2. **`oss/src/epochs.rs`.** `seal_epoch(store, archive, epoch)`:
   - Read or create `epochs.discriminant` with `QrDiscriminant::random`. It is
     never exposed before the epoch is ready.
   - Build, then write `buckets` rows and an `evidence` row (the profiles as
     JSON) under the current `closing_hash`.
   - Make it idempotent.
   - Readers `evidence(epoch)` and `bucket(epoch, profile)` return only rows
     whose `closing_hash` is canonical.

**Tests:**

- Port the devtool `epoch.rs` unit tests that still apply:
  `empty_chunk_spans_survive_multiple_routing_levels`,
  `empty_routed_stamps_still_cover_their_anchor_transitions`, and
  `routing_stops_at_capacity_and_descends_further_for_skewed_members`.
- **New:** for a synthetic epoch over polynomial capacity (devtool
  `populate_epoch`), `profile_of(tg)` finds a bucket containing every published
  tachygram.
- Each bucket verifies, and its anchors are `entry(e)` and `entry(e+1)`.
- An empty epoch yields one `ROOT` bucket.
- `R0 = 1` and a random `R0` both give valid evidence.
- A reorg of the closing block hides the stale rows.

---

## T3: Anchor spans

**Goal.** One span builder, used incrementally by the OSS and on demand by
devtool's local mode.

1. **`oss-core/src/spans.rs`.** `SpanBuilder::new(epoch, start: Anchor,
   chunk_capacity)`. The OSS starts it at the epoch's entry anchor; devtool's
   local mode starts it at the earliest spend anchor, so it never proves stamps
   it does not need.
   - `push_stamp(before, members, after)`: `AnchorSpanSeed`, then
     `AnchorSpanFuse` into the tail. Roll over at `chunk_capacity`, which
     defaults to `max_polynomial_members()`.
   - `spans_from(&[anchor]) -> Option<(target, chunks: Vec<Pcd<AnchorSpan>>,
     start: Vec<Option<usize>>)>`, all from one snapshot. It returns `None` if
     any anchor is unknown.

   Port the seed and fuse calls from devtool `proofs.rs::anchor_spans`. Chunk
   `i` covers `[a_i, a_{i+1}]`. Chunk choice for `x`:
   - If `x == target`, its start is `None`.
   - Otherwise, the chunk whose start anchor is `x`.
   - Otherwise, the chunk that holds `x` as a member.

   `chunks` begins at the smallest chosen index; each `start` is relative to
   it.
2. **`oss/src/span_store.rs`.** The builder (T5) drives it.
   - `connect(block)`: push the block's stamps. Start a new epoch's builder at
     its entry anchor.
   - `rewind(height)`: delete chunks with `end_height >= height` and rebuild
     forward from the archive.
   - Persist the chunks, keeping only epochs `current` and `current − 1`.
   - A read `spans_from(&[anchor])` loads one epoch's chunks inside a single
     read transaction.

**Tests:**

- With `chunk_capacity = 3`, `spans_from` for the entry anchor and for every
  member ends at the tip, and every PCD verifies.
- An anchor at a chunk edge starts at the later chunk.
- `x == target` gives a `None` start.
- Several anchors share one `target` and one `chunks` list.
- A builder started at a mid-epoch anchor works.
- Rollover at capacity works.
- Rewind and rebuild works.
- An unknown anchor returns `None`.
- Lifting a real stamp works. Create it with `FeeSeed` at a member anchor; see
  `repos/tachyon/crates/tachyon/tests/integration/proof.rs::stamp_lift_accepts_only_anchors_the_span_holds`
  and `fixtures.rs`. The stamp must lift along the returned spans to `target`.

---

## T4: Retroactive unspent proving

**Goal.** Given a note's nullifiers for consecutive closed epochs, return one
`ArbitraryUnspent` segment.

1. **`oss-core/src/unspent.rs`.**
   `prove_unspent(epoch_start, nfs: &[Nullifier], bucket: impl FnMut(epoch, Fp) -> Result<Pcd<QrBucket>>)
   -> Result<Pcd<ArbitraryUnspent>>`.
   - Run `QrUnspentInit` on the first epoch, using
     `witness::qr_unspent_init`.
   - Run `UnspentLift` on each later epoch, using
     `witness::unspent_lift(…, value, elapsed_so_far)`.
   - Port the step calls from devtool `proofs.rs`. The result is `n` steps.
2. **`oss/src/unspent_service.rs`.** *(Reassigned to T5: it needs T1's
   archive for the `Spent` check. T4 delivers only the pure part and the
   tests.)* Validate the request:
   - 1–64 values, nonzero canonical field elements.
   - All epochs closed and ready, or `NotReady`.
   - Inside the archive, or `Unavailable`.

   Then:
   - Return `Spent{epoch,height,txid}` if `archive.tachygram_heights(nfs)` is
     non-empty.
   - Otherwise call `prove_unspent` with a lookup of
     `bucket(e, profile_of(nf, R0_e, profiles_e))`.

**Tests.** These are the soundness end-to-end tests that matter most:

- **Simulated wallet.** Follow devtool `proofs.rs` step for step:
  `NoteSeed`; `NullifierDerive` per window and `NullifierFuse`; close several
  epochs (populated and empty); then `unspent(c, [nf_c])`, `UnspentBind`,
  `QrSpendableInit` (bucket by `profile_of(cm)`), `unspent(c+1, nfs…)`,
  `UnspentBind` and `SpendableLift`. The lineage lands on `entry(t)`.
- A page boundary works: 2 calls, each followed by its own `UnspentBind` and
  `SpendableLift`.
- **Negative cases:**
  - A note whose `nf_e` was published returns `Spent`.
  - A note whose `nf_e` was published as the "next" nullifier in `e−1` returns
    `Spent`.
  - An open epoch returns `NotReady`.
  - A value placed in the wrong bucket fails to prove.

---

## T5: RPC server and binary

**Goal.** Expose design §4.2 over HTTP and run everything as one process. You
can start it after T0 and finish it after T4.

1. **`server.rs`.** axum, `POST /`, JSON-RPC 2.0:
   - Dispatch with one `match` on the method.
   - Map errors to design §4.2 codes. `NotReady` carries `retryAfterMs`.
   - Use tower-http `CompressionLayer`.
   - Request body limit 1 MiB.
   - Optional cookie file: HTTP Basic, the same format as Zakura.
   - Reads use a fresh read-only SQLite connection, or a small pool.
   - `oss_unspent` runs under `spawn_blocking`, behind a semaphore
     (`--max-unspent-jobs`, default 4).
   - `oss_block` returns `archive.block_json(height)` with `finalized` set to
     true when `tip − height >= 1000`.
   - `oss_anchorspans` returns `NotReady` when an unknown anchor's block is in
     the archive but above `meta.builder_height`. Otherwise an unknown anchor
     is `Unavailable`.
2. **`main.rs` and `config.rs`.** CLI flags:
   - `--node-url`, `--node-cookie`, `--db`, `--listen` (default
     `127.0.0.1:8240`).
   - `--start-height`, `--backfill-url`, `--cookie-file`,
     `--max-unspent-jobs`.

   Runtime layout:
   - Spawn the follower on tokio.
   - Run **one** blocking builder thread. On each wake-up from the tip
     `watch`:
     1. If `meta.rewind_from` is set, call `span_store.rewind(h)`, move
        `meta.builder_height` below `h`, and clear `rewind_from` with a
        compare-and-clear.
     2. Then process each archived height above `builder_height` in order:
        `span_store.connect`, and `seal_epoch` when that block opens a new
        epoch.
     3. Advance `builder_height` after each block.

     Missed wake-ups are harmless (design §2.4).
   - Publish `evidenceReadyThrough` and the tip on a `watch` channel.
   - Shut down cleanly on Ctrl-C.
3. **`testing.rs` addition.** `spawn_oss(&FakeNode) -> (url, handle)`, so T7
   can run real end-to-end tests.

**Tests.** Over real HTTP:

- Every method succeeds and every error code is produced.
- gzip is negotiated.
- A 3-epoch run with `FakeNode`, including one reorg.
- Data persists across a restart.
- A builder held busy while 50 blocks arrive catches up with correct spans.
- `oss_anchorspans` answers `NotReady` while the builder is behind.
- `oss_block` reports `finalized` correctly for deep and shallow blocks.

---

## T6 (optional): in-flight intake seeding

**Goal.** Cut close-time latency by seeding `QrStampIntakeSeed` as each block
arrives, using the epoch's private `R0`. Persist the seeds in a new `intakes`
table (a schema change; call it out).

**Accept when:** the bucket output is identical to T2's for the same `R0`, and
close-to-ready latency is measured before and after.

---

## T7: Devtool, local and remote `Prover`

**Where.** In `repos/zcash-devtool`, a new local branch `c/oss-client` from the
current HEAD. Do not push.

**Goal.** Devtool works **with or without** an OSS (design §4.3, §4.6, §5.1).
Its proving core moves to `oss-core`, and its cross-process evidence machinery
goes away.

1. **Dependency.** Add `oss-core` by path
   (`../../../../oss/crates/oss-core`); T10 switches it to a git rev.
   - Devtool's `chain::Block`/`Stamp` and `validate` become re-exports of
     `oss-core`.
   - `proofs/epoch.rs` is deleted in favour of `oss_core::build_epoch`.
   - `anchor_spans` uses `oss_core::SpanBuilder`.
2. **`Prover` choice.** Add a `Prover` enum, `Local(&ChainData)` or
   `Remote(Rpc)`, with `unspent`, `bucket_for` and `spans` (design §4.6).
   - Remote mode needs a global `--oss-url` / `--oss-cookie-file`, with env
     fallbacks `TACHYON_OSS_URL` and `TACHYON_OSS_COOKIE`. When neither is set,
     the wallet runs in local mode. There is no config migration.
   - **Local** uses `build_epoch` with `R0 = 1` and calls
     `oss_core::prove_unspent`. It caches all of the epoch's buckets:
     - Add a writer for the `epoch_proofs` table on the **wallet's own**
       connection; nothing writes it today, and the shared evidence connection
       is deleted.
     - On load, use a row only if `verify_pcd` passes and its header epoch,
       `anchor_start` and `anchor_next` equal the current chain's `entry(e)`
       and `entry(e+1)`. Otherwise delete it and rebuild (design §4.6).
   - Local spans use `SpanBuilder::new(epoch, earliest_spend_anchor, …)`, not
     the epoch entry.
   - **Remote** reuses `rpc::Rpc`: add reqwest `gzip`, and make the 16 MiB
     body cap a field (64 MiB for the OSS). It verifies every PCD and its
     header fields. Sync follows design §4.3, "Sync in remote mode":
     1. Wait up to 10 s for the OSS tip to reach the node tip.
     2. Sync to the lower of the two tips.
     3. Use `oss_block` from `archiveFromHeight` to the OSS tip, and the node
        below that.
   - **Spends in both modes:** one spans request for all spends. Its `target`
     is the plan anchor.
3. **Rewrite `proofs::spendable`** once against `Prover`, following the design
   §4.3 table (pages of at most 64 epochs). Delete `lift_closed_epoch` and the
   per-epoch loop.
4. **Delete:**
   - `evidence.rs`.
   - `Operation::{PrewarmEpochs, EvidenceGeneration}`.
   - `check_evidence_identity`, `TACHYON_EVIDENCE_DIR`, `open_for_prewarm`.
   - The `epoch_nodes` code paths.
   - The prewarm and multi-process evidence tests.

   Keep sync, `refresh_note_nullifiers` and `SyncManyCommand` unchanged.
5. **Tests.** The existing closed-epoch wallet tests keep passing in local
   mode. Add remote-mode twins against
   `oss::testing::{FakeNode, spawn_oss}` (a dev-dependency by path) for:
   - `old_note_spends_after_populated_and_empty_closed_epochs`
   - `checkpoint_history_supports_spends_before_and_after_creation_epoch_closes`
   - `spends_after_epoch_with_more_tachygrams_than_polynomial_capacity`
   - `cached_spendability_advances_only_across_new_epochs`
   - `spent_note_cannot_produce_closed_epoch_absence_evidence`
   - `spent_note_cannot_hide_in_another_routed_bucket`

   Also add these remote-mode tests:
   - The node has pruned the note's creation block.
   - A 2-note send.
   - Sync while the OSS is one block behind the node.
   - A local-mode cached bucket is rejected and rebuilt after a reorg of its
     epoch.

**Accept when:**

- `RUSTFLAGS='--cfg zcash_unstable="nutachyon"' cargo test --features tachyon -p tachyon-wallet`
  passes.
- `rg 'prewarm|evidence::' crates/tachyon-wallet/src` finds nothing.
- The diff is net-negative.

---

## T8 (optional, deferred): remote-mode thin sync

In remote mode only, stop storing all blocks:

- Tip and entry-anchor cross-check against the node.
- Creation blocks fetched on demand with `oss_block`.
- Pending status from node `getrawtransaction`.
- Spent detection through `oss_unspent`'s `Spent`, or a new tachygram lookup
  method.

Only do this if wallet disk use or sync time is measured as a problem after
T10.

---

## T9: Workload and faucet

**Where.** `repos/tachyon-workload` and `repos/tachyon-faucet`, on local branch
`oss`. Follow design §5.2.

**Workload:**

- Delete `evidence_ready`, `prewarm`, `evidence_token`, the prewarm round and
  defer paths, `Devtool.prewarm`, `prewarm_slots`, `TACHYON_EVIDENCE_DIR`,
  `test_prewarm.py`, and the README section.
- Add `oss_url` to the config. Pass it as `TACHYON_OSS_URL` in `Devtool._run`'s
  env.
- Before planning payments, gate on
  `current_epoch == 0 or (ready is not None and ready >= current_epoch − 1)`,
  where `ready` is `oss_info.evidenceReadyThrough`. One JSON-RPC call through
  the existing node-RPC client class pointed at `oss_url`. Keep mining while
  not ready.
- Treat devtool's `NotReady` error as a deferral, not a failure.

**Faucet:** add an optional `oss_url` and pass it the same way. Update the
README.

**Tests:** the existing unit tests pass, plus:
- readiness-gate tests covering epoch 0, `null`, and lagging and ready
  values;
- a test for the env passthrough.

---

## T10: Deploy and measure

This needs the box, so it is human in the loop. The agent only drafts.

- **Draft:** a systemd unit and example config for `tachyon-oss` (localhost,
  next to Zakura), and switch the `oss-core` and `oss` path dependencies to git
  revs.
- **Runbook:**
  1. Start the OSS with `--node-url`. Use `--backfill-url` if the node is
     pruned below the history you need.
  2. Wait until `oss_info.evidenceReadyThrough == tip.epoch − 1`.
  3. Deploy devtool `c/oss-client`.
  4. Set `oss_url` in the workload and faucet configs.
  5. Restart both.
- **Measure:**
  - Epoch-close to evidence-ready latency.
  - `oss_unspent` latency per epoch covered.
  - Payment latency before and after (`TACHYON_TIMING` logs).
  - OSS disk use per epoch.
  - That the workload crosses an epoch boundary without pausing payments.
  - That one wallet without `oss_url` still pays (local mode).
