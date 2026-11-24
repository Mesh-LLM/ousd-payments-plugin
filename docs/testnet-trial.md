# Faucet-funded buyer-seller trial

This trial was recorded with the earlier payment design, which opened one native reserve per request; current payments reuse one channel per buyer and seller.

On 2026-11-13, two local Mesh 0.79.0 hosts completed a native Tempo Moderato payment trial using faucet test OUSD only. The Homebrew release included its native runtime. Each host ran the normal `pay-stablecoin` executable. A separate canned OpenAI backend returned one input token and four output tokens, so this validates the payment and host lifecycle rather than model execution or metering accuracy.

The buyer used an explicit seller allowlist and request approval. Each request reserved 10 atomic test OUSD units, captured 5, and refunded 5. One atomic unit is 0.000001 test OUSD. Payment and gas both used token `0x20c0000000000000000000006a37da5c996874be` on chain `42431`.

| Request | Inference | Reserved | Captured | Refunded |
| --- | --- | ---: | ---: | ---: |
| 1 | Buffered | 10 | 5 | 5 |
| 2 | Streaming with terminal usage | 10 | 5 | 5 |

The buyer restarted after the first settled request and retained its node identity, wallet, and accounting state. Both peers ended without active obligations. Wallet signing was unavailable through the public tool surface.

## Finalized transaction evidence

Buffered request `00000000-0000-4000-8000-000000000000`:

- [Open transaction](https://explore.tempo.xyz/tx/0xe3e9fa3f76aa9b0770ec69a9decd91b3d5793d410083ba02be0fc75194813313)
- [Close transaction](https://explore.tempo.xyz/tx/0xee2d46edd93401e5f0f7fec6d48b74cfaa8970634f2b6c825fd5d1916cf90883)
- Channel: `0x0f0f5dc03ecfba4416cf5841e4c628432d3d10408019503e9ba498e92fa06b4a`.

Streaming request `00000000-0000-4000-8000-000000000001`:

- [Open transaction](https://explore.tempo.xyz/tx/0xa8b2d5e10afa8c454887f7d202dbc4c9784aea0d7e841621b679c7a9ee2ac51f)
- [Close transaction](https://explore.tempo.xyz/tx/0x000eca990ed44e962315374a9a623c20d7de1bbf7644cc80fb44eaefcc19074d)
- Channel: `0xdb551e952caa0f84bb0722229a9df164d6fec44392ed3d9cb3e255ffd662d321`.

Independent fresh RPC reads confirmed all four successful transactions were finalized native type `0x76`, targeted the canonical reserve, and used test OUSD as the fee token. The close events paid 5 to the seller and returned 5 to the buyer for each channel. Both channels had deleted, zero state at finalized block `43970414`.

## Balance and gas reconciliation

| Wallet | Initial balance | Final balance | Payment delta | Gas |
| --- | ---: | ---: | ---: | ---: |
| Buyer | 1000000000000 | 999999999629 | -10 | 361 |
| Seller | 1000000000000 | 999999999778 | +10 | 232 |

All table amounts are atomic test OUSD units. Each wallet received 1,000,000 test OUSD from the faucet. The final balance of each wallet equals its initial balance plus payment delta minus gas. Total gas was 593 atomic units, or 0.000593 test OUSD. First-use account/storage costs differ from subsequent transactions; these values are observations from this trial, not fee benchmarks.

## Repeat the payment-only scenario

This records the earlier `pay-stablecoin` executable, now named `tempo`. Current full payment-and-bond validation is recorded in the [exit-delay trial](tempo-exit-testnet-trial.md). To repeat this payment-only scenario with the current plugin, build the fixture tools and native plugin, then deliberately invoke testnet mode with a fresh private directory:

```sh
just build
just testnet-smoke /opt/homebrew/bin/mesh-llm /absolute/private/fresh-trial-directory
```

Without `just`, use:

```sh
cargo build --locked --all-features
target/debug/host-smoke --mesh-bin /opt/homebrew/bin/mesh-llm \
  --output-dir /absolute/private/fresh-trial-directory --tempo-testnet
```

The command creates encrypted wallets and private password files, requests faucet coins, runs buffered/SSE requests through both hosts, checks settlement receipts, and stops the hosts. Preserve the output directory: it contains wallet keys, credentials, journals, configs, private invitations, logs, and `result.json`. The completed local run is stored under `private/tempo-079-trial/`, which is ignored by Git and excluded from packages. Its `verification.json` records the independent RPC audit.

This trial did not exercise seller death during inference, missing terminal usage, or the 900-second native payer withdrawal path. Those remain covered by local fixtures and require separate testnet acceptance.
