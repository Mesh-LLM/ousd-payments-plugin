#!/usr/bin/env bash
# Package only the production plugin. No wallet state, credentials or fixtures.
set -euo pipefail
if [[ $# != 3 ]]; then
  echo 'Usage: package.sh BINARY_DIRECTORY TARGET_TRIPLE VERSION' >&2
  exit 2
fi
binary_directory=$(cd -- "$1" && pwd)
target_triple=$2
package_version=$3
[[ "$target_triple" =~ ^[a-zA-Z0-9_-]+$ && "$package_version" =~ ^[a-zA-Z0-9._+-]+$ ]] || exit 2
workspace_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
host_target=$(rustc -vV | sed -n 's/^host: //p')
if [[ "$target_triple" != "$host_target" ]]; then
  echo "Package native binaries on their target platform ($host_target)." >&2
  exit 1
fi
stage_root=$(mktemp -d)
trap 'rm -rf -- "$stage_root"' EXIT
plugin_name=tempo
actual_version=$("$binary_directory/$plugin_name" --version)
if [[ "$actual_version" != "$plugin_name $package_version" ]]; then
  echo "Version mismatch for $plugin_name: expected $package_version, got $actual_version" >&2
  exit 1
fi
plugin_stage="$stage_root/$plugin_name"
mkdir -- "$plugin_stage"
cp -- "$binary_directory/$plugin_name" "$plugin_stage/$plugin_name"
chmod 755 "$plugin_stage/$plugin_name"
printf 'name = "%s"\nversion = "%s"\n' "$plugin_name" "$package_version" > "$plugin_stage/plugin.toml"
"$plugin_stage/$plugin_name" --manifest > "$plugin_stage/plugin-manifest.json"
cp -- "$workspace_root/README.md" "$plugin_stage/README.md"
cp -- "$workspace_root/LICENSE" "$workspace_root/ORIGIN.md" "$workspace_root/REFERENCE.md" "$plugin_stage/"
mkdir -- "$plugin_stage/docs" "$plugin_stage/demo" "$plugin_stage/contracts"
mkdir -- "$plugin_stage/contracts/src"
cp -- "$workspace_root/contracts/src/TempoBondEscrow.sol" "$plugin_stage/contracts/src/"
cp -- "$workspace_root/contracts/foundry.toml" "$workspace_root/contracts/runtime.hex" "$workspace_root/contracts/creation.hex" "$plugin_stage/contracts/"
cp -- "$workspace_root/docs/"*.md "$plugin_stage/docs/"
cp -- "$workspace_root/docs/tempo-bonds-testnet-trial.json" "$workspace_root/docs/tempo-exit-testnet-trial.json" "$plugin_stage/docs/"
cp -- "$workspace_root/demo/"*.example "$plugin_stage/demo/"
mkdir -p -- "$workspace_root/dist"
archive="$workspace_root/dist/$plugin_name-v$package_version-$target_triple.tar.gz"
COPYFILE_DISABLE=1 tar -C "$stage_root" -czf "$archive" "$plugin_name"
if command -v sha256sum >/dev/null; then
  (cd -- "$workspace_root/dist" && sha256sum "$(basename -- "$archive")") > "$archive.sha256"
else
  (cd -- "$workspace_root/dist" && shasum -a 256 "$(basename -- "$archive")") > "$archive.sha256"
fi
printf '%s\n' "$archive"
