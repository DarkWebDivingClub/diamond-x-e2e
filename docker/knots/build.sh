#!/usr/bin/env bash
# Build the Bitcoin Knots regtest image used by the Knots-backed scenarios.
#
# Stages bitcoind and bitcoin-cli from a local Knots build into a temporary
# context, then builds. Override KNOTS_BIN to point at a different build.
set -euo pipefail

KNOTS_BIN="${KNOTS_BIN:-$HOME/git/bitcoin-knots/build/bin}"
IMAGE="${IMAGE:-dwdc/bitcoin-knots}"
TAG="${TAG:-29}"

for b in bitcoind bitcoin-cli; do
    [ -x "$KNOTS_BIN/$b" ] || { echo "missing $KNOTS_BIN/$b — build Knots first" >&2; exit 1; }
done

ctx="$(mktemp -d)"
trap 'rm -rf "$ctx"' EXIT
cp "$KNOTS_BIN/bitcoind" "$KNOTS_BIN/bitcoin-cli" "$ctx/"
cp "$(dirname "$0")/Dockerfile" "$ctx/"

echo "building $IMAGE:$TAG from $KNOTS_BIN"
docker build -t "$IMAGE:$TAG" "$ctx"
echo "done: $IMAGE:$TAG"
