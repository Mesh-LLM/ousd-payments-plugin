#!/usr/bin/env python3
"""Verify and exercise native plugin archives without creating payment wallets."""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import tomllib


def run(*args):
    return subprocess.check_output(args, text=True, timeout=30)


def verify(dist, target, version):
    name = "tempo"
    archive = dist / f"{name}-v{version}-{target}.tar.gz"
    checksum, filename = archive.with_suffix(".gz.sha256").read_text().split()
    with archive.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if filename != archive.name or actual != checksum:
        raise ValueError(f"Checksum mismatch: {archive.name}")
    with tempfile.TemporaryDirectory(prefix="payments-package-") as temporary:
        root = Path(temporary)
        with tarfile.open(archive) as package:
            allowed = {name, "plugin.toml", "plugin-manifest.json", "README.md", "LICENSE", "ORIGIN.md", "REFERENCE.md", "docs", "demo", "contracts"}
            for member in package.getmembers():
                parts = Path(member.name).parts
                if not parts or parts[0] != name or (len(parts) > 1 and parts[1] not in allowed):
                    raise ValueError(f"Unexpected package content: {member.name}")
            package.extractall(root, filter="data")
        plugin = root / name
        with (plugin / "plugin.toml").open("rb") as source:
            metadata = tomllib.load(source)
        if metadata != {"name": name, "version": version}:
            raise ValueError(f"Invalid package metadata: {name}")
        binary = str(plugin / name)
        if run(binary, "--version").strip() != f"{name} {version}":
            raise ValueError(f"Executable version mismatch: {name}")
        manifest = json.loads((plugin / "plugin-manifest.json").read_text())
        if json.loads(run(binary, "--manifest")) != manifest:
            raise ValueError(f"Manifest mismatch: {name}")
        hook = manifest["openai_exchange_hook"]
        if not (hook["required"] and hook["admission"] and hook["effective_request_body"]):
            raise ValueError("Payment admission hook is missing")
        if manifest.get("operation_bindings"):
            raise ValueError("Unexpected external wallet binding")
        print(f"Verified {archive.name}")

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target")
    parser.add_argument("version")
    parser.add_argument("--dist", type=Path, default=Path(__file__).resolve().parent.parent / "dist")
    args = parser.parse_args()
    verify(args.dist, args.target, args.version)
