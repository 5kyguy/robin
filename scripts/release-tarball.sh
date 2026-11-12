#!/bin/sh
# Build the release binary and pack it the way r2-d2-install-robin
# expects: a top-level robin-v<version>/ directory holding the
# executable, and SHA256SUMS beside the tarball. Version comes from
# Cargo.toml.
set -eu
cd "$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="robin-v${version}"

cargo build --release --locked

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
install -Dm755 target/release/robin "$stage/$name/robin"
tar -C "$stage" -czf "${name}.tar.gz" "$name"
sha256sum "${name}.tar.gz" > SHA256SUMS
echo "${name}.tar.gz"
