#!/bin/sh
# Builds static server binaries for the releases (needs Docker with buildx):
#   dist/backroom-server-linux-x64.tar.gz    — PC / VPS
#   dist/backroom-server-linux-arm64.tar.gz  — Raspberry Pi with a 64-bit OS
#   dist/backroom-server-linux-armv7.tar.gz  — Raspberry Pi with a 32-bit OS
#
# The binaries are linked statically (musl), so they run on any Linux of that architecture.
set -e
cd "$(dirname "$0")/.."
mkdir -p dist
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

for pair in amd64:x64 arm64:arm64 arm/v7:armv7; do
  PLATFORM=linux/${pair%%:*}
  NAME=${pair##*:}
  echo "== $PLATFORM"
  docker buildx build --platform "$PLATFORM" --target binary --output "type=local,dest=$OUT/$NAME" .
  tar -czf "dist/backroom-server-linux-$NAME.tar.gz" -C "$OUT/$NAME" backroom-server
done
ls -lh dist/backroom-server-*
