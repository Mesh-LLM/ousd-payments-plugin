# Operator setup

The plugin supports test OUSD on Tempo Moderato (chain 42431) and OUSD on Tempo mainnet (chain 4217). `wallet.network` selects `testnet` or `mainnet`, with testnet as the default. Wallet creation, funding, and network spending are explicit operator actions. Development tests use deterministic temporary keys and local fixtures.

## Build and install

Use the companion Mesh `peer-evidence` branch for the current plugin. Earlier payment-and-bond trials used Mesh 0.80.0, and payment-only operation passed on Mesh 0.79.0; those binaries predate the current manifest-based verifier contract. Even a model-free buyer needs the native runtime. Older Mesh 0.77.0 is incompatible.

```sh
just check
just test
just doc
just package
mesh-llm plugins install --archive dist/tempo-v0.1.0-<target>.tar.gz \
  --name tempo --version 0.1.0
```

Install the same archive on buyer and seller using their appropriate native architecture. Use the plugin store that each running Mesh node will use. There is no second wallet plugin to install.

## Configure and initialize

A package install needs no configuration: the plugin writes `~/.mesh-llm/tempo/config.json` on first start, with its state in the same owner-only directory. For paid inference, edit that file following the [buyer](../demo/buyer-payments.json.example) or [seller](../demo/seller-payments.json.example) example, or copy [the Mesh template](../demo/mesh.toml.example) and point `config_path` at your own file. All state and credential paths must be absolute.

The `data_dir` holds the encrypted key, the payment ledger (requests, channels, daily spending and runtime settings) and the transaction journal. Keep it across restarts. The `fund` control creates the wallet when there is none, and so does `bond_lock`; the CLI creates it ahead of time:

```sh
tempo wallet init
tempo wallet address
```

Pass `--config` to use another config file. Without `wallet.password_file` the wallet gets a random password in `data_dir/wallet.password`, so the key at rest is only as safe as that directory; name a private password file of at least 16 bytes with mode 600 to keep the password elsewhere. Creation refuses to replace an existing wallet and records the configured network in its profile. Startup never creates keys. Without a profile the node runs without a wallet: it verifies bonds and serves free inference, and refuses to start when prices or automatic buying are configured. Payments start after a restart once the wallet exists. A profile must match the configured network. A wallet with neither a configured nor a generated password stays locked; both buyer funding and seller capture require an unlocked wallet.

## Switch networks

Select `"network": "testnet"` or `"network": "mainnet"` inside `wallet`. Omit `rpc_url` to use the matching public RPC, or supply an HTTPS RPC for the selected chain. An RPC on the other chain is rejected before signing or broadcast. Both peers must select the same network.

Maintain a config and a separate `data_dir` for each network. For example, testnet may use `/absolute/private/NODE/testnet` while mainnet uses `/absolute/private/NODE/mainnet`. See [the mainnet example](../demo/mainnet-payments.json.example). Initialize and fund each wallet once using its own config.

Let open channels close, or recover them, before stopping the current node. To switch, point its `config_path` at the other network's config and restart. Use the local CLI with that same config to initialize the wallet. To return, restore the original config. The ledger and journal stay bound to their original network; do not edit `wallet.json`, copy a journal between networks, or delete pending state. `control fund` reports the active network. Bonds remain on their original chain. To release one, run `bond_exit` with that chain's original config and wallet, and keep that node running until the plugin has withdrawn it after the delay.

Mainnet wallets must receive real OUSD on Tempo mainnet for deposits and gas. The Moderato faucet supplies test coins only. Network selection does not bridge or convert funds, and spending still requires an automatic buyer policy that lists the seller and the model.

## Fund with test coins

On testnet, `control fund` asks Tempo's Moderato faucet for test coins for the wallet's address. To request them by hand instead, replace `YOUR_WALLET_ADDRESS` with the address from `wallet address`:

```sh
curl --fail-with-body https://rpc.moderato.tempo.xyz \
  -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"tempo_fundAddress","params":["YOUR_WALLET_ADDRESS"]}'

```

Wait for faucet funding to finalize before checking the balance again. The plugin uses test OUSD at `0x20c0000000000000000000006a37da5c996874be` for both payments and gas. Buyers need each channel's deposit (`channel_topup`, 1 OUSD by default) plus gas; sellers need closing gas. No ETH or real funds are required. Never fund a published fixture address. The faucet is an explicit setup step and is never called by plugin startup. See the official [Tempo testnet instructions](https://github.com/tempoxyz/tempo#testnet) and [MPP session configuration](https://mpp.dev/payment-methods/tempo/session).

## Set payment policy

Sellers optionally configure exact model IDs, OUSD rates per million input and output tokens, and a cache discount under `prices`, and the upfront a new channel pays under `admission_deposit`. No price, or a zero price, means free inference. Buyers set `policy` to `{"mode": "automatic", "daily_budget": "..."}`, in the config or with the `policy` control. They may also narrow buying to sellers in `buy_peers` and to rate ceilings per model in `buy_prices`, set a per-request `cap`, the most unbilled credit a seller may hold in `admission_deposit`, and the channel deposit in `channel_topup`. The examples start in `free_only` mode, so spending stays off until that change. Amounts use the selected network's OUSD. Clients send no payment fields; the buyer's policy decides which requests are paid. The [README](../README.md#install-and-configure) explains each setting.

Startup writes `policy`, `prices`, and a set `admission_deposit` into the payment ledger, and controls such as `policy`, `set_pricing` and `admission_deposit` change them while the node runs. Removing a model from `prices` does not unprice it at the next start; use `set_pricing` without a value. Provider bond requirements and seller bond publication are independent. On testnet a buyer that omits `bonds.providers` only sees sellers with at least the default bond of 1 OUSD and a one-minute exit delay, which `bond_lock` locks by default. Installing the plugin always filters sellers by bond. Wallet limits independently bound each deposit and transaction fee. Lowering them does not invalidate previously signed transactions.

## Start nodes

Use one persistent Mesh identity per node. Set `MESH_LLM_NODE_KEY_PATH` to a stable absolute file and leave `MESH_LLM_EPHEMERAL_KEY` unset. The template uses `[runtime] mode = "on_demand"` and `mesh-llm serve` without a model for a buyer. On the companion branch, a client-only buyer can instead use `[runtime] mode = "client"` and `mesh-llm client --join-file <private-invitation-path>` with an explicit `MESH_LLM_NODE_KEY_PATH`, as exercised by the latest trial. The earlier Mesh 0.79.0 setup used `serve` because its ordinary client mode did not preserve the required identity.

Set `MESH_LLM_CONFIG` to the node's template copy and `MESH_LLM_PLUGIN_DIR` to its installed plugin store. Start the seller with its chosen model using the matching host's normal `serve` command. Start the buyer with `serve --join-file <private-invitation-path>`. Keep invitations private. The plugin obtains the authenticated node ID from the host and pins it before recovering payment state.

The template grants bounded request bodies, admission, and terminal lifecycle events to the single plugin. Keep the installed executable path, and avoid command overrides that bypass the host's identity grant. Do not enable another payment engine on these dedicated nodes.

## Request and inspect

Send the [README request example](../README.md#requests-and-settlement) to the buyer's `/v1/chat/completions` endpoint with the exact advertised model ID; the buyer pays for it under its policy. For streaming, include terminal usage in the original body. A 402 while the first channel opens is rare: the buyer cancelled that request, so send it again.

Inspect both nodes through `mesh-llm plugins call tempo control`. `pending` shows active requests and channels; `transactions` shows recent requests, channels and wallet transactions, including each channel's voucher, billed total and, once closed, its capture and refund; `balance` and `fund` show wallet state. A paid request finishes when the buyer has signed its bill; the channel stays open for later requests until a close. Captured plus refunded equals the channel's deposit.

The local CLI only initializes the wallet and reads its address; inspect wallet state through these `control` commands. Trusted local clients can also invoke `send` and the explicit bond commands, which move wallet funds; raw signed transaction submission is not exposed. See the [command list](../README.md#inspect-and-control) and [bond setup and controls](bonds.md).

See [the recovery guide](mpp-demo.md#failure-and-recovery) for cancellation, unknown usage, waiver, and native withdrawal. Deleting state does not resolve a payment. Native payer recovery can beat long inference or outages, so a funded deployment needs the host work described in [release boundaries](production-readiness.md).
