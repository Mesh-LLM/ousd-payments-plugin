# Tempo Plugin

`tempo` provides OUSD payments and refundable identity bonds on Tempo through one wallet and one plugin process. Sellers may set prices and publish a bond independently. Buyers use free inference by default and may separately authorize paid inference or require provider bonds. Choose test OUSD on Moderato or OUSD on mainnet through `wallet.network`; testnet is the default. One Rust crate and one plugin process own payment channels, request agreements, buyer policy, admission, wallet keys, native transactions, settlement, and recovery. Lightning lives in the independent [LN Payments Plugin](https://github.com/Mesh-LLM/ln-payments-plugin), whose configuration and controls this plugin mirrors where they apply.

Payments use one long-lived native payment channel per buyer and seller. The first paid request opens it, later requests reuse it, and the seller captures only what it billed when the channel closes. See [payment channels](docs/mpp-demo.md).

This is a developer preview. Two companion Mesh hosts have completed faucet-funded payments with no bonds, the provider bonded and the buyer unbonded, plus delayed exits and full bond refunds on Moderato. Buffered and streaming requests and buyer restart also passed. Those trials used the earlier design with one reserve per request; the long-lived channels have offline validation only. Inference used a canned backend. See the [trial record](docs/tempo-exit-testnet-trial.md) and [release boundaries](docs/production-readiness.md).

See [bond setup and policies](docs/bonds.md) for origin-signed gossip evidence, provider eligibility, indefinite capital locks, delayed exits, and escrow refunds. The plugin requires the companion Mesh host changes.

## Build

Requires Rust 1.95.0, a C compiler, Python 3.12+, and `just`. Contract checks additionally use Foundry 1.9.0 and the pinned Solidity 0.8.30 compiler. Dependency revisions and the Cargo lockfile are pinned.

```sh
just check
just test
just doc
just package
```

`just package` builds only the production `tempo` binary and produces one native archive and SHA-256 sidecar in `dist/`. The package validator checks contents, metadata, the extracted executable, and its manifest. No release is published automatically.

`just build` also builds the explicitly enabled offline fixture executables. Production startup always opens the native wallet; settings and environment variables cannot select the fabricated backend. Fixture executables are excluded from release archives.

## Install and configure

Use the current companion Mesh `peer-evidence` branch for this plugin. The [recorded Moderato trial](docs/tempo-exit-testnet-trial.md) used a historical Mesh 0.80.0 revision and the preceding plugin API; its recorded binary is incompatible with this checkout. The SDK dependency is pinned to `46da729c7102108f71e41a4998f10d498b9d42dd`, the companion stack's additive verifier-contract preparation commit. Other host versions may have a different plugin ABI.

The SDK revision currently exists in the local Mesh stack and Cargo cache. Fresh machines and CI need that commit published to the configured Git source before they can fetch the dependency.

Once release archives named `tempo-v<version>-<target>.tar.gz` are published, install by repository name; until then, install a local archive:

```sh
mesh-llm plugins install Mesh-LLM/ousd-payments-plugin
mesh-llm plugins install --archive dist/tempo-v0.1.0-<target>.tar.gz \
  --name tempo --version 0.1.0
```

### Quick start on testnet

A buyer that only sees and relays bonded providers needs nothing beyond the install:

```sh
mesh-llm client --join <token>
```

A seller posts the default bond of 1 OUSD with a one-minute exit delay in two commands. `fund` creates the wallet and asks the Moderato faucet for test coins; once they arrive, run `bond_lock` once. The plugin follows the lock until it is finalized and then publishes the bond; `bond_context` shows the published reference:

```sh
mesh-llm --model <model>
mesh-llm plugins call tempo control --input '{"command":"fund"}'
mesh-llm plugins call tempo control --input '{"command":"bond_lock"}'
```

To get the bond back, run `bond_exit`. It needs no arguments, and once the exit delay has passed, the plugin withdraws the bond into the wallet.

As in the Lightning plugin, a seller prices a model and a buyer turns on paid buying with one command each; no configuration file edit is needed. The seller's `fund` creates its wallet; a buyer creates one with `tempo wallet init` before it starts, then funds it:

```sh
mesh-llm plugins call tempo control --input \
  '{"command":"set_pricing","model":"<model>","value":{"input_per_million":"1000000","output_per_million":"1000000"}}'
mesh-llm plugins call tempo control --input '{"command":"fund"}'
mesh-llm plugins call tempo control --input \
  '{"command":"policy","value":{"mode":"automatic","daily_budget":"100000"}}'
```

The buyer then pays any seller at its quoted rates, within the daily budget, from a channel with a 1 OUSD deposit.

### Configuration

A package install needs no `[[plugin]]` entry: installing it grants the plugin's requested exchange hook, and the plugin keeps its configuration and state in `~/.mesh-llm/tempo/`, writing `config.json` there on first start with every setting at its default. Edit that file, or point a `[[plugin]]` entry's `config_path` at another one, as in [the Mesh template](demo/mesh.toml.example); the [buyer](demo/buyer-payments.json.example) and [seller](demo/seller-payments.json.example) examples show paid configurations. A combined configuration looks like this:

```json
{
  "data_dir": "/absolute/private/node/payments",
  "wallet": {
    "network": "testnet",
    "password_file": "/absolute/private/node/password",
    "max_payment": "100000",
    "max_fee": "1000"
  },
  "prices": {
    "SELLER_MODEL_ID": {"input_per_million": "1000000", "output_per_million": "10000000", "cache_discount_percent": 50}
  },
  "admission_deposit": "10000",
  "policy": {"mode": "automatic", "daily_budget": "50000"},
  "cap": "10000",
  "buy_peers": ["SELLER_64_CHARACTER_HEX_PEER_ID"],
  "buy_prices": {
    "BOUGHT_MODEL_ID": {"input_per_million": "1000000", "output_per_million": "10000000"}
  },
  "channel_topup": "50000"
}
```

All amounts are integer strings in millionths of OUSD on the selected network.

| Setting | Meaning |
| --- | --- |
| `prices` | Seller rates per exact model ID, per million input and output tokens. `cache_discount_percent` (0..100) is taken off reported cached input tokens only. An absent or zero price means free inference. |
| `admission_deposit` | As a seller, the upfront a buyer pays on a new channel unless it has paid this seller since the seller started. As a buyer, the most unbilled credit it lets a seller hold on a channel, so at most one upfront. Unset means 0. |
| `policy` | `mode` is `free_only` (the default, no paid buying) or `automatic`. Automatic needs a positive `daily_budget`, which counts the voucher amounts the buyer signs per UTC day. |
| `cap` | The most one request may cost the buyer: its upfront plus the extra voucher amount its bill needs, checked for the worst case before dispatch and for the actual bill after. `0` means no cap; the remaining daily budget still applies. |
| `buy_peers` | Sellers the buyer may pay. Empty, the default, means any seller, as in the Lightning plugin. |
| `buy_prices` | Optional rate ceilings per model. A quote must not exceed the rates and must give at least the cache discount. A model without an entry is bought at the seller's rates, within `cap` and the daily budget, as in the Lightning plugin. |
| `channel_topup` | The deposit of each new channel and the amount of each top-up; 1 OUSD unless set. Automatic mode needs it to be no more than `wallet.max_payment`, and at least `cap`, or at least the daily budget when there is no cap; the `policy` control checks the same. Whatever the settings, the buyer refuses a request whose worst-case bill one top-up could not pay. |
| `wallet.max_payment` | The largest single channel open or top-up deposit the wallet signs. |
| `wallet.max_fee` | The largest fee of any one transaction attempt. |

Startup writes `policy`, `prices`, and a set `admission_deposit` into the payment ledger, where the [controls](#inspect-and-control) can change them at runtime. Every start re-applies them, so a runtime switch to `free_only`, a removed price or a changed deposit reverts at the next start unless the configuration file is changed too. Startup only adds or updates prices: removing a model from `prices` does not unprice it, so use `set_pricing` without a value, or a zero price. Per-model admission deposits are set with controls only. `buy_peers`, `buy_prices`, and `cap` are read from the configuration file. Optional `bonds.providers` requirements are independent of pricing and seller bond publication. Payment and gas both use OUSD. Test coins have no monetary value; mainnet uses real OUSD. This plugin provides no card processing, bank transfers, or fiat conversion.

For payments, both peers must select the same network and have unlocked wallets with OUSD for gas. The `fund` control creates the wallet on first use, with a random password in the state directory unless `wallet.password_file` names one, and on testnet asks the faucet for test coins; `bond_lock` creates it too. Payments start after a restart once the wallet exists. The local CLI can create it ahead of time, with the default configuration unless `--config` names another:

```sh
tempo wallet init
tempo wallet address
```

On testnet a buyer only sees sellers with at least the default bond of 1 OUSD and a one-minute exit delay, unless its config sets `bonds.providers` to another requirement; installing the plugin always filters providers by bond. See [bonds](docs/bonds.md). A node without a wallet still verifies other providers' bonds through its RPC and serves free inference; configured prices or automatic buying make startup refuse until a wallet exists. Installation and startup never create or fund a wallet; only `fund`, `bond_lock` and `tempo wallet init` do. On mainnet, fund the address `fund` reports with OUSD on Tempo. Follow the operator setup for permissions, stable Mesh identities, and node startup.

## Select testnet or mainnet

| `wallet.network` | Chain ID | Default RPC | Payment and gas asset |
| --- | ---: | --- | --- |
| `"testnet"` (default) | 42431 | `https://rpc.moderato.tempo.xyz` | Test OUSD |
| `"mainnet"` | 4217 | `https://rpc.tempo.xyz` | Mainnet OUSD |

Both networks use six-decimal OUSD at `0x20c0000000000000000000006a37da5c996874be` and the canonical native reserve. Omitting `wallet.rpc_url` selects the network's default; an explicit URL overrides it and must report the selected chain. The plugin never infers or changes the network from the RPC response.

To switch, let open channels close or recover them on the current network, stop the node, and choose a config with the target `wallet.network` and its own `data_dir`. Initialize the target wallet once, fund it on that network, and restart. Use [the mainnet example](demo/mainnet-payments.json.example) as a starting point. Switching back uses the original config and state directories. The wallet profile and the state database's wallet pin reject reuse across networks; never edit a profile's chain ID or delete state to bypass those checks. The switch does not transfer or convert tokens. Bonds stay on their original chain; exit and refund them using the original config and state directory. `control fund` reports the active network and wallet asset.

## Requests and settlement

Clients send ordinary requests without payment fields; whether a request is paid is the buyer's decision, as in the Lightning plugin. The buyer pays when `policy.mode` is `automatic` and `buy_peers` is empty or lists the selected seller, and mints a request ID for it. Every other request is sent free, and a priced provider rejects it before execution. A seller that does not price the model serves it free. A paid request needs an exact model and a finite token limit at `/v1/chat/completions`:

```json
{
  "model": "EXACT_ADVERTISED_MODEL_ID",
  "messages": [{"role": "user", "content": "Reply OK"}],
  "max_tokens": 4
}
```

For streaming, include `"stream": true` and `"stream_options": {"include_usage": true}` in the body. Bodies must fit within 64 KiB and output limits must be 1..4096. Requests to one seller run at once. A request waits only while one with the same body is still in flight, while the buyer's first request to that seller opens the channel, or while the seller answers busy because the buyer has three unpaid bills. The buyer waits up to 25 seconds in all for these and for a new channel or a top-up to finalize, which normally suffices. A 402 while it is still pending is a rare fallback, for example when the wallet's transaction slot is busy, many new sellers are opened at once, or the RPC is slow: the buyer cancels the request with the seller, the channel keeps opening, and the client may send the request again as a new request.

1. The seller quotes its rates, its view of the channel, and an upfront: the voucher increase over its view it requires before work. This seller asks its `admission_deposit` on a new channel from a buyer that has not paid it since the seller started, and 0 otherwise.
2. The buyer checks the quote against its policy and refuses an upfront that would leave the seller more than the buyer's `admission_deposit` of unbilled credit. Before opening or signing anything, it also checks the worst case: the upfront plus the extra voucher amount a bill would need if every body byte were an input token and the full output limit were used must fit `cap` and the remaining daily budget, and one top-up must be able to pay it. It then opens the channel with `channel_topup` if none exists and signs a cumulative voucher covering the seller's view of the channel plus the upfront. Vouchers are off-chain signatures, and a seller that missed one gets the buyer's latest voucher again.
3. After inference, the seller bills the request at the quoted rates. Output beyond the request's token limit is not billed.
4. The buyer signs a larger voucher only when the bill matches the usage it observed itself, the quoted rates capped at the worst case checked before dispatch (the seller absorbs any prompt longer than the body bound), the earlier bills, and fits `cap`. As in the Lightning plugin, the bills of work already dispatched are paid even when concurrent requests overshoot the daily budget, and a switch to `free_only` sends new requests free but still pays them. Upfront credit pays bills first. Bills are numbered per channel, and the buyer accepts them in order, holding one that arrives early. When a voucher would exceed the deposit, the buyer tops up once by `channel_topup`; later bills wait for that top-up.
5. When the channel closes, the seller captures the billed total, at most the best voucher, and the rest returns to the buyer.

As in the Lightning plugin, a buyer that has paid a seller, by an upfront or a bill, has standing with it: its later requests and channels need no upfront, and it can run many requests at once. The seller keeps standing in memory for at most 1,024 buyers, and a restart clears it. The seller answers busy while a buyer has three unpaid bills: bills on its open channel that no voucher covers yet, and bills that a closed channel lost, until the operator runs `unblock`. A buyer with standing can still leave bills unpaid; the seller's loss is bounded only by how many requests the buyer starts before three of its bills are unpaid. Usage that arrives after a channel began closing is never billed and is not counted.

The seller closes a channel when a bill stays unpaid 120 seconds after it was sent, after one hour without requests, or soon after the buyer requests close, and always within the buyer's 900-second grace. The buyer recovers a channel it has not used for two hours by requesting close and withdrawing after the grace. A dishonest seller can keep upfront credit by billing more than was used: vouchers do not prove computation, so choose sellers and `admission_deposit` accordingly. Nothing is settled before close, so a seller that stays unavailable for the buyer's whole 900-second grace after a close request loses everything the channel earned; see the [trust model](REFERENCE.md). Missing usage is never billed as zero, and it does not block the pair: the seller tells the buyer it never bills the request, the buyer closes it unpaid, and its next request is served on the same channel. See [payment channels](docs/mpp-demo.md).

## Inspect and control

```sh
mesh-llm plugins call tempo control --input '{"command":"pending"}'
mesh-llm plugins call tempo control --input '{"command":"transactions","limit":20}'
mesh-llm plugins call tempo control --input '{"command":"balance"}'
mesh-llm plugins call tempo control --input '{"command":"fund"}'
mesh-llm plugins call tempo control --input '{"command":"policy","value":{"mode":"automatic","daily_budget":"50000"}}'
mesh-llm plugins call tempo control --input '{"command":"set_pricing","model":"MODEL","value":{"input_per_million":"1000000","output_per_million":"10000000"}}'
mesh-llm plugins call tempo control --input '{"command":"send","operation_id":"UUID","to":"0xRECIPIENT","amount":"1000000","max_fee":"1000"}'
```

| Command | Effect |
| --- | --- |
| `balance` | Wallet balance, amounts reserved by unresolved transactions, and spendable balance. |
| `transactions {limit}` | The most recent requests, channels and wallet transactions, newest first, at most `limit` (1..100) of each. Signed bytes and voucher signatures are never shown. |
| `pending` | Active requests and channels, and unresolved wallet transactions, without signatures. |
| `fund` | The wallet address, network and asset to send OUSD to. |
| `send {operation_id, to, amount, max_fee}` | A journaled OUSD transfer from the wallet's own balance. Repeating the same operation UUID returns the same transaction. Sends to the wallet itself, the OUSD token or the reserve contract are refused. Channel deposits are never touched. |
| `policy {value?}` | Show or set the buyer policy, with today's spending and remaining budget. |
| `pricing` | Seller prices and admission deposits. |
| `set_pricing {model, value?}` | Set or, without `value`, remove a model's price. Removal also removes its admission deposit. The host is told to require or stop requiring the payment channel for that model; if that call fails, it is retried on the next reconciliation tick. |
| `set_cache_discount {model, percent}` | Change only the cache discount of a priced model. |
| `admission_deposit {value?, reset}` | Show, set, or reset the node-wide admission deposit. |
| `set_model_admission_deposit {model, value?}` | Set or clear one priced model's own admission deposit. |
| `retry {request_id?}` | Reconcile pending wallet transactions and active requests and channels, or one request. |
| `recover {peer?}` | Buyer: close the channel with a seller, named by its node ID or a unique prefix of at least eight characters, or without `peer` the only open channel. It requests close on chain; the seller then closes, or the buyer withdraws after the grace. |
| `cancel {request_id}` | Buyer: cancel a request that has not started. |
| `unblock {peer}` | Seller: forgive the bills a buyer's closed channels lost, so they no longer count toward the three unpaid bills that make the seller answer busy. The bills stay recorded. |
| `waive {request_id}` | Seller bookkeeping for a request with unknown usage: it only closes the request once the buyer was told that it is never billed or a later request was quoted, and otherwise sends a zero bill. |
| `bond_*` | Bond controls; see [operator actions](docs/bonds.md#operator-actions). |

Preserve the payment ledger, wallet journal, keys, and persistent Mesh identity across restarts. Never delete state to clear pending work. Recovery and sends require gas and an unlocked wallet.

The plugin exposes `control` and the read-only `verify_peer_evidence` operation. The companion Mesh host restricts plugin HTTP and MCP calls to trusted loopback clients. Those local clients can invoke wallet-moving `control` commands, including `send`, bond locking and withdrawal. Raw signed transaction submission is not exposed. While the plugin is stopped, the CLI only offers `wallet init` and `wallet address`.

## Offline integration

```sh
just build
just smoke /absolute/path/to/matching/mesh-llm /absolute/private/fresh-smoke-directory
```

The harness installs an explicitly built fixture entry point under the plugin name, then uses real hosts, plugin IPC, peer transport, an in-process fabricated wallet, and canned inference. It sends a buffered and a streaming request over one channel, restarts the buyer between them, and checks that both used the same open channel. It does not execute Tempo contracts.

For an explicitly authorized faucet-funded trial, run `just testnet-smoke /absolute/path/to/mesh-llm /absolute/private/fresh-trial-directory` after `just build`. This installs the normal native payment binary on two temporary hosts, creates fresh encrypted wallets, requests Moderato faucet coins, and pays two canned inference requests from one channel opened with 100 atomic test OUSD units: an upfront of 10 covers both bills of 5. Gas is capped at 1000 atomic units per transaction. It preserves wallets, journals, logs, and `result.json` in the private trial directory, then stops both hosts. This command uses the public testnet and must be invoked deliberately. For the payment-and-bond matrix, follow the [trial reproduction steps](docs/tempo-exit-testnet-trial.md#reproduction-and-limits). See [validation](docs/validation.md), [architecture](REFERENCE.md), and [source provenance](ORIGIN.md).
