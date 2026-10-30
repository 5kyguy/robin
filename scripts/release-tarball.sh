#!/bin/sh
# Build a release tarball and SHA256SUMS. Version comes from Cargo.toml.
set -eu
cd "$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="robin-v${version}"
stage=$(mktemp -d)
mkdir -p "$stage/$name"
# The tarball is the git tree without the checkout metadata.
git archive --format=tar HEAD | tar -C "$stage/$name" -xf -
tar -C "$stage" -czf "${name}.tar.gz" "$name"
sha256sum "${name}.tar.gz" > SHA256SUMS
echo "${name}.tar.gz"
rm -rf "$stage"
