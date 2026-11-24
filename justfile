set shell := ["bash", "-euo", "pipefail", "-c"]
target := `rustc -vV | sed -n 's/^host: //p'`
version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`

default: check

build:
    cargo build --locked --all-features

release:
    cargo build --locked --release --bin tempo

check:
    cargo fmt --all --check
    cargo check --locked --all-targets --all-features
    cargo clippy --locked --all-targets --all-features -- -D warnings

test:
    cargo test --locked --all-features

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps

fmt:
    cargo fmt --all

smoke mesh_bin output_dir:
    ./target/debug/host-smoke --mesh-bin "{{mesh_bin}}" --output-dir "{{output_dir}}"

# Explicitly creates testnet wallets, requests faucet coins, and sends Moderato transactions.
testnet-smoke mesh_bin output_dir:
    ./target/debug/host-smoke --mesh-bin "{{mesh_bin}}" --output-dir "{{output_dir}}" --tempo-testnet

package target_triple=target package_version=version: release
    bash scripts/package.sh target/release "{{target_triple}}" "{{package_version}}"
    python3 scripts/verify-packages.py "{{target_triple}}" "{{package_version}}"

# Local functional demos can package the already-built debug binaries.
package-debug target_triple=target package_version=version: build
    bash scripts/package.sh target/debug "{{target_triple}}" "{{package_version}}"
    python3 scripts/verify-packages.py "{{target_triple}}" "{{package_version}}"

# Tests paid/bond-only inference, delayed exits and refunds against the Moderato escrow, or the one in TEMPO_BOND_ESCROW.
testnet-bonds mesh_bin output_dir:
    ./target/debug/host-smoke --mesh-bin "{{mesh_bin}}" --output-dir "{{output_dir}}" --tempo-testnet --tempo-bonds

contracts-check:
    forge fmt --root contracts --check
    forge test --root contracts --offline
    test "$(forge inspect --root contracts TempoBondEscrow deployedBytecode)" = "$(cat contracts/runtime.hex)"
    test "$(forge inspect --root contracts TempoBondEscrow bytecode)" = "$(cat contracts/creation.hex)"
