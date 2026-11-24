# Tempo payments and bonds testnet trial

This trial was recorded with the earlier payment design, which opened one native reserve per request; current payments reuse one channel per buyer and seller.

This is a historical trial of the maturity-and-renewal escrow. The current source uses indefinite locks and delayed exits, requires a fresh deployment, and has different bytecode. The transactions below do not validate that newer contract.

On 2026-11-19, two local Mesh hosts ran the production `tempo` plugin against Tempo Moderato, chain 42431, using fresh faucet-funded OUSD wallets. Inference used a canned OpenAI backend with one input and four output tokens. Mesh ran from `codex/tempo-bonds`, based on upstream `dc8a8fafb8bd51efc287840a84b764c35346b21b`. The immutable plugin SDK pin was unchanged.

The escrow is `0x4dA347a95C1b49b8FC90Da083058682ac7613bf2`. Its deployed runtime matched the source and `contracts/runtime.hex` at the time of this historical trial, compiled with Solidity 0.8.30; it does not match the current exit-delay artifacts. Each host posted a separate 1 OUSD deposit, or 1,000,000 atomic units, binding its own host key to the same mesh. Automatic renewal extended each existing deposit without changing its amount, mesh, host key or refund wallet. Renewal was then disabled and both principals were refunded at maturity. OUSD Transfer logs confirmed the exact principal returned to each original funding wallet.

## Observed behavior

| Configuration or event | Result |
|---|---|
| Payments on, bonds off | Buffered and streaming requests succeeded; each reserved 10, captured 5 and refunded 5 atomic units. |
| Buyer restart | The explicit client node-key path preserved identity and payment state. |
| Payments and bonds on | Paid inference succeeded; both sides required the configured bond evidence. |
| In-place automatic renewal | Both on-chain deadlines advanced, preserving principal and identity bindings. |
| Bonds on, payments off | Inference succeeded without MPP request metadata. |
| Buyer omitted its bond reference | Seller returned HTTP 402 before inference dispatch. |
| Seller withdrew its bond | A fresh buyer did not admit or route to the seller; inference returned HTTP 404. |
| Payments and bonds off | Free inference succeeded. |
| Final journals and ledgers | Zero pending signed attempts and zero unresolved payment requests on both hosts. |

## Finalized bond transactions

| Wallet | Operation | Transaction |
|---|---|---|
| seller | deploy | [`0x9101a77db8a0e86380aa42876b962cb47120578ce3dc3e6d341ee42035216a83`](https://explore.tempo.xyz/tx/0x9101a77db8a0e86380aa42876b962cb47120578ce3dc3e6d341ee42035216a83) |
| seller | lock | [`0xf1585d39856231d552f8e14661e5e78a163cc52da43b62c43261c4ae6a8ced1b`](https://explore.tempo.xyz/tx/0xf1585d39856231d552f8e14661e5e78a163cc52da43b62c43261c4ae6a8ced1b) |
| seller | renew | [`0x8d96330ee33eb1f2193aa1ed449456f66d3afa371cc1468d3eb90276986a97db`](https://explore.tempo.xyz/tx/0x8d96330ee33eb1f2193aa1ed449456f66d3afa371cc1468d3eb90276986a97db) |
| seller | withdraw | [`0xd6cdbc4361a906785d128508ac3fed993b2b97f2846955ce7d84b770b2014cf2`](https://explore.tempo.xyz/tx/0xd6cdbc4361a906785d128508ac3fed993b2b97f2846955ce7d84b770b2014cf2) |
| buyer | lock | [`0xb72d17156f1600e50a1bbf22e77e36e2b48e9509b510e6c0d0500b378e8a6768`](https://explore.tempo.xyz/tx/0xb72d17156f1600e50a1bbf22e77e36e2b48e9509b510e6c0d0500b378e8a6768) |
| buyer | renew | [`0x150515d2de8448823c32cfe7e3511a672a714d3786addc1d262515c14d83d7b4`](https://explore.tempo.xyz/tx/0x150515d2de8448823c32cfe7e3511a672a714d3786addc1d262515c14d83d7b4) |
| buyer | withdraw | [`0x9c4306ba585ab15ee91758d15bd1a19dc3c88d534c3af7c64350421fbbac757c`](https://explore.tempo.xyz/tx/0x9c4306ba585ab15ee91758d15bd1a19dc3c88d534c3af7c64350421fbbac757c) |

[Machine-readable public evidence](tempo-bonds-testnet-trial.json) includes initial and renewed state, outcomes and payment transaction hashes. Private wallet keys, passwords, signed transaction bytes, invitations and full host logs remain outside the tracked report.

## Historical reproduction

The maturity-and-renewal harness used for this trial has been replaced. Current `just testnet-bonds` and `--resume-bonds` commands exercise indefinite locks and delayed exits, and cannot reproduce or resume this historical escrow. Follow the [current trial instructions](tempo-exit-testnet-trial.md#reproduction-and-limits) with a fresh directory instead. Preserve historical wallets and journals separately.

The trial found and corrected integration issues in explicit client identity persistence, constrained-stack peer ingress, host-owned grant configuration, deployment gas budgeting and private-mesh bootstrap order. Earlier setup attempts and their wallets were preserved. Every reserve created during those attempts finalized its settlement; no bond principal remains locked from the completed trial. Routine tests remain deterministic and offline.
