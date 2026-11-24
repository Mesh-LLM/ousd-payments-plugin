# Provider bond testnet trial

This trial was recorded with the earlier payment design, which opened one native reserve per request; current payments reuse one channel per buyer and seller.

On 2026-11-19, two local Mesh hosts ran the production `tempo` plugin on Tempo Moderato, chain 42431, using fresh faucet-funded wallets and canned inference. The provider posted one refundable bond. The buyer posted no bond and its complete bond journal remained empty. No mainnet transactions were sent.

The host was rebuilt from clean `codex/tempo-bonds` commit `7b2b4568de768d893fe67977685626c6f8c1e032`, reporting `mesh-llm 0.80.0+g7B2B45`. The Mesh SHA-256 was `6b874481e4c6345d267882179960f7c7da17a0e639559c493b10d53b245ad9fd`; the production Tempo SHA-256 was `775c7a3d3d7387c6f09f37757ea323a41d5761ffa8765a75a00e49a578738cae`. The SDK dependency was pinned to the companion verifier-contract preparation commit `c0d31fd2453478dd2f4ebf3514acdfed5bb16aee`. A separate two-host offline smoke passed buffered/SSE inference, partial refunds and buyer restart with preserved identity, sending zero network transactions.

The escrow is `0xd210B0356b3e45B37F29Ec763106dc23BF1cC3aa`. Its deployed runtime exactly matched `contracts/runtime.hex`, with Keccak-256 `0x2a5a7148b45dba623a798c19f910130cce6afafc651ef15534b59daaa8d1feb4`. The provider deposited 1 test OUSD, or 1,000,000 atomic units, with a 30-second exit delay for this explicit trial. Production policies can require longer delays. Funds remained locked beyond that delay without renewal; the countdown started only when the provider requested exit.

## Observed behavior

| Scenario | Result |
| --- | --- |
| Payments without bonds | Buffered and SSE requests succeeded before escrow deployment. |
| Payments with provider eligibility enabled | Succeeded with a bonded provider and an unbonded buyer. |
| Provider bond with payments disabled | Free inference succeeded. |
| Provider omits its published proof | Provider inference became unavailable. |
| Restart and indefinite funding | Bond remained active without renewal transactions, including beyond its exit delay. |
| Exit countdown | Verification accepted the provider until the withdrawal deadline; early withdrawal reverted. |
| Deadline reached, principal still escrowed | Verification rejected the provider as withdrawable. |
| Withdrawal | Finalized and returned the full 1,000,000 atomic units to the original funding wallet. |
| Withdrawn provider | Verification rejected the proof and inference was unavailable. |
| Payments and bonds disabled | Free inference succeeded. |

Three paid requests settled in total. Each reserved 10 atomic units, captured 5 and refunded 5. Buyer and seller ledgers agreed with finalized closure evidence. Mesh owned the only provider eligibility cache. Tempo returned deadlines from each bond's exit delay or known withdrawal deadline, with no core validity cap; failed verification used a test-only 1-second retry deadline. Admission callbacks received no bond evidence and performed no bond verification. Separate Mesh regression tests confirm that unrelated plugin stops preserve cached and in-flight verification, while a verifier stop invalidates only its own decisions.

## Transactions and final state

| Wallet | Bond operation | Finalized transaction |
| --- | --- | --- |
| seller | deploy | [`0x430675a399c3f1b4ac44ff61e59fa8480606611278e88e30bc11939c5c3aff02`](https://explore.tempo.xyz/tx/0x430675a399c3f1b4ac44ff61e59fa8480606611278e88e30bc11939c5c3aff02) |
| seller | lock | [`0x950688e9a2988c4b9fc4f3e3c68a0ed0bb9c7724c7f94e48cd7b382899818ce9`](https://explore.tempo.xyz/tx/0x950688e9a2988c4b9fc4f3e3c68a0ed0bb9c7724c7f94e48cd7b382899818ce9) |
| seller | request_exit | [`0xf6e7bc06031309e974f564569179911608b03f7b661f21c804150ecb6f857359`](https://explore.tempo.xyz/tx/0xf6e7bc06031309e974f564569179911608b03f7b661f21c804150ecb6f857359) |
| seller | withdraw | [`0x0157fcc4c618dab270329a07fcdb9923ae2926b8c95961862b8f7f3261750fd1`](https://explore.tempo.xyz/tx/0x0157fcc4c618dab270329a07fcdb9923ae2926b8c95961862b8f7f3261750fd1) |

| Wallet | Address | Gas, atomic test OUSD | Unresolved transactions | Active payment obligations |
| --- | --- | --- | --- | --- |
| seller | `0x4e5a3223889A301363a5ad9F05baFBF648de47eE` | 3613 | 0 | 0 |
| buyer | `0x86F9D0d3c10dE691e77f7Cc51bfCD14Ed6D4630e` | 395 | 0 | 0 |

All 10 signed transactions had successful canonical receipts below finalized block 44954791. Independent RPC checks confirmed the exact principal refund, a zero escrow balance, withdrawal at or after the exit deadline, and reconciled final wallet balances. The buyer sent zero bond transactions. Repeating completed-trial resume preserved all 105 private state and evidence files without sending transactions.

[Public machine-readable evidence](tempo-exit-testnet-trial.json) includes transaction hashes and receipt summaries, deadlines, active/exiting/final bond state, rejection results, executable hashes and wallet reconciliation. Full receipts and transfer logs can be retrieved using the transaction hashes. Private keys, passwords, raw signed transaction bytes, invitations and host logs are excluded.

## Reproduction and limits

The recorded results above belong to the historical executable hashes and Mesh revision in the evidence file. Exact reproduction requires those preserved staged binaries and their matching configuration. Do not pair the recorded Mesh binary with the current checkout: its evidence-grant API differs.

The commands below start a new trial of the current checkout against a newly built current Mesh `peer-evidence` host. This combination has offline validation only; the commands are not evidence that the current revision passed testnet. Build the fixture tools and production release, then stage the harness and canned backend beside the release binary:

```sh
just build
just release
trial_bin_dir=$(mktemp -d "${TMPDIR:-/tmp}/tempo-trial-bin.XXXXXX")
cp target/debug/host-smoke target/debug/demo-backend-fixture "$trial_bin_dir/"
cp target/release/tempo "$trial_bin_dir/"
"$trial_bin_dir/host-smoke" --mesh-bin /absolute/path/to/companion/mesh-llm \
  --output-dir /absolute/fresh/private/trial-directory --tempo-testnet --tempo-bonds
```

This deliberately creates encrypted Moderato wallets, requests faucet OUSD, deploys the pinned escrow and refunds the provider's principal. Preserve the private directory, which contains wallet state, passwords, operation inputs, invitations, journals and logs. Resume an interrupted trial with the same staged binaries, directory and `--tempo-testnet --tempo-bonds --resume-bonds`. Saved transaction inputs and release checkpoints preserve the original operations. A completed run resumes without modifying its files.

The recorded run validated native testnet payments and provider-bond integration using canned inference. It does not establish real model execution guarantees, mainnet acceptance, throughput or economic viability. Revert, expiry, missing-receipt and crash recovery paths retain deterministic offline regression coverage. The earlier maturity-and-renewal trial is historical and uses a different escrow runtime.
