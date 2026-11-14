# OSS deploy runbook (T10)

Status: draft, 2026-11-11. Nothing below has run on the box yet. Every step that
publishes code or changes the box needs the operator.

## 0. What ships

| Repo | Branch | Head | Notes |
| --- | --- | --- | --- |
| S1nus/oss-tachyon (`oss/`) | `main` | see `git log` | `tachyon-oss` binary, `oss-core` library |
| cnode-agent/zcash-devtool | `c/oss-client` | `aa2909e` | 12 commits on `1716bff`; takes `oss-core` from S1nus/oss-tachyon at `467da1f`, and needs an OSS with 1000-block `oss_blocks` and `oss_transactions`: upgrade the OSS first |
| cnode-agent/tachyon-workload | `oss` | `625f428` | `oss_url` is **required**; `DEVTOOL_REVISION` is `aa2909e` |
| cnode-agent/tachyon-faucet | `oss` | `2a18150` | `oss_url` is optional |

Zakura, tachyon, ragu, common and tachyscan are unchanged.

## 1. Publish

1. **Done:** this repo is at https://github.com/S1nus/oss-tachyon (`oss/` and
   `docs/`; `repos/` is gitignored).
2. **Done:** devtool takes `oss-core`, and `oss` for its tests, from that
   repo by git rev. When the OSS code changes, bump both `rev`s in
   `crates/tachyon-wallet/Cargo.toml`, rebuild, and rerun devtool's tests.
3. **Done:** devtool `c/oss-client`, workload `oss` and faucet `oss` are
   pushed to the cnode-agent forks.
4. **Done:** the workload's `DEVTOOL_REVISION` is the new devtool head.

## 2. Start the OSS (next to Zakura)

1. **Check the node's storage mode.**
   - In `archive` mode, the OSS backfills from NuTachyon activation.
   - In `pruned` mode, the node keeps only 10,000 blocks on Testnet (1,001 on
     Regtest). Either:
     - pass `--backfill-url` pointing at an archive node, or
     - start with `--start-height <epoch boundary inside the retention window>`.

     The OSS answers `Unavailable` for older epochs. Remote-mode devtool then
     proves those epochs itself from the wallet's own blocks. That is slower,
     and it needs blocks the wallet stored before the node pruned them.
2. **Build and install.**
   ```
   cd oss && cargo build --release --bin tachyon-oss
   install target/release/tachyon-oss /opt/tachyon-oss/
   ```
   Edit `oss/deploy/tachyon-oss.service`: the node port, the cookie path, the
   database path and the user. Install it, then:
   ```
   systemctl daemon-reload
   systemctl enable --now tachyon-oss
   ```
3. **Wait until it is ready.** Ready means the OSS tip equals the node tip and
   `evidenceReadyThrough == tip.epoch − 1`.
   ```
   curl -s 127.0.0.1:8240 -d '{"jsonrpc":"2.0","id":1,"method":"oss_info","params":{}}'
   ```
   It has to ingest every block since its start height and seal each closed
   epoch, which takes about 16 s for a full epoch with mock proofs.

## 3. Switch the wallets

1. **Stop the workload between rounds.** Do not stop it while an
   `epoch_prewarm` is running: the new journal refuses unfinished prewarm
   rounds. Stop the faucet too.
2. **Build devtool** at the new commit:
   ```
   RUSTFLAGS='--cfg zcash_unstable="nutachyon"' cargo build --release --locked --features tachyon
   ```
3. **Update the workload and faucet configs.**
   - Workload: `oss_url = "http://127.0.0.1:8240"`, plus `oss_cookie_file` if
     the OSS was started with `--cookie-file`.
   - Faucet: the same keys, optional. Without them, the faucet wallet proves
     locally.
4. **Check out** workload `oss` and faucet `oss`, then restart both services.
   - The workload passes `TACHYON_OSS_URL` to every devtool call, so all 33
     wallets switch to remote mode at once. They need no migration.
   - The old `wallet_root/.evidence` directory is no longer used. Delete it
     once you are happy.

## 4. Verify

- **Payments and logs.**
  - The workload logs show payments resuming.
  - An epoch boundary pauses payments only until `evidenceReadyThrough`
    catches up, about 16 s for a full epoch, instead of every wallet
    prewarming.
- **Measure these** (the old `TACHYON_TIMING` logs are the baseline):
  - epoch-close to ready latency
  - `oss_unspent` time per epoch covered
  - payment latency before and after
  - OSS disk use per epoch
- **Local mode still works.** Run one devtool command without `TACHYON_OSS_URL`
  against a spare wallet.
- **Thin sync and privacy output** (design §4.3, §4.4). Remote-mode devtool
  prints `privacy warning:` lines on stderr for every OSS request that carries
  wallet-specific values; `-v` adds `contacting OSS (…)` / `got OSS response
  (…, 1.2s)` lines. Neither changes the JSON on stdout.

## 5. Roll back

- **Workload, faucet and devtool:** stop them, check out the previous commits,
  rebuild devtool, and restart.
  - The workload journal stays compatible both ways. The new code only stops
    writing the `expected` column.
  - Wallet databases also stay compatible. The new devtool only adds bucket
    rows to `epoch_proofs`, and the old devtool would rebuild its shared
    evidence cache from scratch.
- **OSS:** `systemctl disable --now tachyon-oss`. Its database can be deleted.

## 6. Known limits (prototype)

- **Proofs:**
  - Mock proofs only.
  - The OSS publishes its tachyon and ragu revs in `oss_info.proofSystem`, and a
    mismatch shows up as failed verification.
- **Pruned-node gaps** (both marked with `shortcut:` comments in devtool):
  - **Transparent-only transactions:** remote-mode sync takes a pruned
    block's transaction list from its stamps, so a transparent-only
    transaction in a pruned block never shows as "mined".
  - **Stale receipts:** `locate` still asks the node's `getblock` for a stale
    height hint, which fails on a pruned node.
- **The OSS's single builder thread:**
  - It seals epochs inline, so spans answer `NotReady` for about 16 s at each
    full-epoch close.
  - A stop in the middle of a seal restarts that seal.
- **Disk:** stale bucket rows from reorged epochs are never deleted, about
  32 MiB per full epoch per reorg.
- **Thin sync:** a wallet that synced epochs thin needs the OSS to check
  spends in them, and cannot prove them locally.
- **Sync speed:** remote sync reads pages of 1000 blocks (one `oss_blocks`
  call and one node `getblockhash` batch, sent together), one page ahead of
  validation. Devtool never calls `getblock`. Local mode, and heights below
  the OSS archive, still fetch one block per request.
- **Upgrading to revision 7:**
  - Upgrade the OSS before devtool. A new devtool asks for 1000-block pages,
    which an older OSS rejects.
  - On its first start, the OSS indexes the transaction IDs of every block it
    already archived: one `getblock` per block to its node. Until that
    finishes, `oss_transactions` answers `NotReady`; nothing else waits.
- **Privacy:** see design §4.4. The OSS sees the nullifier runs and the
  creation-bucket requests of remote wallets. Spends of notes created in the
  current epoch prove their anchor spans locally, so the OSS is not told which
  transaction created them. Those spends cost what they did before the OSS.
