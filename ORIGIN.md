# Source provenance

This repository started from the standalone stablecoin workspace in [Mesh-LLM/mesh-llm](https://github.com/Mesh-LLM/mesh-llm), directory `mesh/docs/plugins/exemplars/stablecoin-payments`, at commit `ef827f66c9fbbc835db00e5d038bd24340e54b64` (`feat(payments): use request-scoped MPP for Tempo`). The original Apache 2.0 license is preserved in [LICENSE](LICENSE). The original manifests declared `MIT OR Apache-2.0`; this distribution includes the Apache license supplied by the source repository.

The current implementation consolidates the Tempo engine, wallet, shared types, and fixtures into one Cargo package. It ships one plugin process, with direct wallet calls and one combined configuration. Other payment rails, provider IPC, separate wallet packaging, and prototype migration code are omitted.

The Mesh SDK dependency is pinned to `46da729c7102108f71e41a4998f10d498b9d42dd`. The current companion host is the `peer-evidence` branch based on `dc8a8fafb8bd51efc287840a84b764c35346b21b`. The [recorded exit-delay trial](docs/tempo-exit-testnet-trial.md) used the preceding host/plugin API on the former `codex/tempo-bonds` branch; its preserved revisions and binaries are historical. Tempo is pinned to `tempo-alloy@1.10.1`, commit `3e95ed3d80be0fad2973f1abd2fb403d6c33011a`. The Cargo lockfile preserves compatible dependency resolution.

Private configuration, credentials, keys, wallet state, source repository Git metadata, fixtures, and build caches are excluded from the production archive. See [architecture](REFERENCE.md) and [release boundaries](docs/production-readiness.md).
