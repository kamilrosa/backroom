#!/bin/sh
# Builds the server image for amd64, arm64 and armv7 and pushes it to the GitHub Container Registry:
#   ghcr.io/kamilrosa/backroom-server:<version> and :latest
#
# One-time setup: a GitHub token with the write:packages scope, then log Docker in:
#   gh auth refresh -h github.com -s write:packages
#   gh auth token | docker login ghcr.io -u <github-user> --password-stdin
set -e
cd "$(dirname "$0")/.."
IMAGE=ghcr.io/kamilrosa/backroom-server
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

# Multi-platform builds need a builder with the docker-container driver.
docker buildx inspect backroom-builder >/dev/null 2>&1 \
  || docker buildx create --name backroom-builder --driver docker-container >/dev/null

docker buildx build --builder backroom-builder \
  --platform linux/amd64,linux/arm64,linux/arm/v7 \
  -t "$IMAGE:$VERSION" -t "$IMAGE:latest" --push .
echo "Pushed $IMAGE:$VERSION and :latest"
