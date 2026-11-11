# OSS deploy runbook (T10)

Status: draft, 2026-11-11. Nothing below has run on the box yet. Every step that
publishes code or changes the box needs the operator.

## 0. What ships

| Repo | Branch | Head | Notes |
| --- | --- | --- | --- |
| this repo (`oss/`) | `main` | see `git log` | `tachyon-oss` binary, `oss-core` library |
| cnode-agent/zcash-devtool | `c/oss-client` | `660f770` | 6 commits on `1716bff`; depends on `oss-core` **by path** |
| cnode-agent/tachyon-workload | `oss` | `42de24a` | `oss_url` is **required** |
| cnode-agent/tachyon-faucet | `oss` | `2a18150` | `oss_url` is optional |

Zakura, tachyon, ragu, common and tachyscan are unchanged.

## 1. Publish (operator decision: nothing has been pushed)

1. **Host this repo** somewhere the box can fetch, for example a new
   `cnode-agent/tachyon-oss`. It holds `oss/` and `docs/`. `repos/` is
   gitignored.
2. **Switch devtool's path dependencies** to that host. In
   `crates/tachyon-wallet/Cargo.toml`:
   ```toml
   oss-core = { git = "https://github.com/<org>/tachyon-oss", rev = "<main sha>" }
   # dev-dependency, only needed for devtool's tests:
   oss = { git = "https://github.com/<org>/tachyon-oss", rev = "<main sha>", features = ["testing"] }
   ```
   Run `cargo update -p oss-core -p oss`, run devtool's test suite again, and
   commit on `c/oss-client`.
3. **Push** devtool `c/oss-client`, workload `oss` and faucet `oss` to the
   cnode-agent forks. Open PRs if wanted.
4. **Bump the devtool pin.** In the workload, set `DEVTOOL_REVISION` to the new
   devtool head and commit.

## 2. Start the OSS (next to Zakura)

1. **Check the node's storage mode.**
   - In `archive` mode, the OSS backfills from NuTachyon activation.
   - In `pruned` mode, the node keeps only 10,000 blocks on Testnet (1,001 on
     Regtest). Either:
     - pass `--backfill-url` pointing at an archive node, or
     - start with `--start-height <epoch boundary inside the retention window>`.

     Notes with older creation epochs then get `Unavailable` in remote mode,
     and need local mode.
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
- **Privacy:** see design §4.4. The OSS sees the nullifier runs and the
  creation-bucket requests of remote wallets.
