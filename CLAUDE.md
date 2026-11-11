# Tachyon OSS prototype

We are building a prototype Oblivious Synchronization Service (OSS) for
Tachyon, and moving zcash-devtool and the workload onto it.

- `docs/oss-design.md` is the contract: architecture, wire protocol, and the
  audit. Read it before you code.
- `docs/oss-tasks.md` is the task list. Do only your task.

## Layout

- `oss/`: new Cargo workspace.
  - `crates/oss-core` is the pure proving and protocol library, shared with
    devtool.
  - `crates/oss` is the server.
- `repos/`: reference clones (zakura, tachyon, ragu, zcash-devtool,
  tachyon-workload, tachyon-faucet, tachyscan, common-*).
  - They are **read-only**, except the branches that T7–T9 create in
    `zcash-devtool`, `tachyon-workload` and `tachyon-faucet`.
  - Never push. Never commit to an existing branch of a reference repo.

## Rules

- **Port, don't reinvent.** Most OSS logic already exists in
  `repos/zcash-devtool/crates/tachyon-wallet/src`: `proofs/epoch.rs`,
  `proofs.rs` and `chain.rs`. Move it, and keep its checks and error handling.
- **Pin to devtool's revs.** Use the same `zcash_tachyon`, ragu and zakura-*
  revisions as `repos/zcash-devtool/crates/tachyon-wallet/Cargo.toml`. A
  different rev gives a different PCD application, and proofs will not verify
  across it.
- **Verify every PCD you receive across a process boundary**: compress, then
  `verify_compressed`, as devtool's `verify_proof` does. Check its header
  fields against what you asked for.
- **The OSS never receives** notes, `cm`, `psi`, `mk` or keys. It receives only
  opaque nullifier values for epochs that are already closed (the retroactive
  model). It keeps no per-wallet state.
- **Devtool must keep working without an OSS.** Self-prove (local) mode uses
  the same `oss-core` functions.
- **Toolchain** is Rust 1.97.1. Devtool builds and tests need
  `RUSTFLAGS='--cfg zcash_unstable="nutachyon"'` and `--features tachyon`.
- **Slow tests.** Proof steps are CPU-heavy. If debug tests are too slow, use
  `cargo test --release` and say so in your report.
- **Do not modify** zakura, tachyon, ragu, common or tachyscan.
