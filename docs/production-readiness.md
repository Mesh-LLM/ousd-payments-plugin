# Release boundaries

This is a single-crate, single-process implementation of long-lived Tempo payment channels, one per buyer and seller. `wallet.network` selects Moderato testnet (default) or mainnet OUSD, with separate state directories. Basic two-host integration and faucet-funded native capture and refund passed on Moderato with Mesh 0.79.0, and the recorded companion-host trial passed paid inference with no bonds and with provider-only bonding, followed by delayed bond exits and full refunds. Those trials used the earlier design with one reserve per request; the long-lived channels, the Lightning-style configuration and the new controls have deterministic offline coverage only. Mainnet signing and settlement also have offline coverage only; no funded mainnet trial was run. It remains a developer preview with the host durability and deadline dependencies below.

The manifest-only verifier contract, optional provider requirement and always-available capabilities have also changed since those trials.

## Implemented boundaries

The plugin owns channels, request agreements, admission, policy, usage billing, keys, signing, evidence verification, the wallet journal, and request accounting. Calls to the wallet are typed and stay in process. The operations are `control` and the read-only peer-evidence verifier; there is no separate wallet capability, host binding, or child process.

Implemented: native channel opens, top-ups when a bill needs them, off-chain cumulative vouchers, per-request bills checked against the buyer's own usage and accepted in order, concurrent requests per pair, upfront credit with Lightning-style standing and strikes, a single capture of the billed total at close with an atomic refund, unpaid-bill and idle closes, closing within the close-request grace, payer recovery with `requestClose` and withdrawal, pending transaction recovery, unknown usage with explicit waiver, journaled wallet sends, and runtime settings with re-registration of model channel requirements. Immutable request identities prevent duplicate dispatch; uncertain transactions reuse exact signed bytes. Retries after proven expiry or a finalized revert preserve the original intent and retain all earlier attempts. Engine and wallet locks and identity pins protect current state during restart.

This is new code without a migration layer for earlier versions. The state database rejects unsupported schema versions. These checks protect the current version's durable state, rather than upgrading older formats.

Production startup always loads the native wallet. The fabricated backend is reachable only through the separate `tempo-fixture` executable, built with `offline-fixture`. Production packaging builds the normal binary without that feature and includes one executable, metadata, licenses, documentation and pinned escrow source/bytecode.

## Required Mesh work

The SDK is pinned to `46da729c7102108f71e41a4998f10d498b9d42dd`, which adds the versioned verifier declaration. Stronger payment guarantees require further host changes:

1. Persist terminal usage and support acknowledged, repeatable delivery. The plugin saves received usage before acknowledging it, but host death before delivery can still lose a bill; the plugin preserves unknown usage and refuses execution replay.
2. Enforce execution deadlines and cancellation with settlement margin. A payer may independently start Tempo's 900-second withdrawal grace period. Admission alone cannot stop resource use beyond that deadline.
3. Preserve attempt identity through routing and failover. An agreement belongs to its seller and request; work already performed must remain distinguishable from a new attempt elsewhere.
4. Deliver each peer's payment messages in order. The plugin handles a peer's messages one at a time in the order it receives them, but the SDK may hand two closely spaced messages over in either order. Bills no longer depend on it, since the buyer accepts them by their number; other reorderings fall back to retries.

## Acceptance still required

The long-lived channels need an authorized Moderato trial: channel reuse across requests, top-ups, unpaid-bill and idle closes, a buyer close request during inference, payer withdrawal, seller restart during work, missing usage, and wallet sends. These cases currently have deterministic local coverage only. The earlier [trial evidence](testnet-trial.md) establishes the exercised host lifecycle and plugin IPC, not real model inference, all host failure paths, or production fee and latency benchmarks.

Settling vouchers before close, a prepay mode in which sellers never serve on credit (the wire already carries what it needs), standard HTTP MPP interoperability, and simultaneous Lightning admission coordination are outside this implementation. A future streaming payment flow would need continuous metering and enforceable execution control.

Bond-specific configuration, RPC trust, lease expiration and recovery guarantees are documented in [the bond guide](bonds.md). Escrow code and delayed exits are developer preview functionality. The [exit-delay trial](tempo-exit-testnet-trial.md) validates the escrow and admission lifecycle on Moderato using canned inference.
