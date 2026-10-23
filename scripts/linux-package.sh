#!/bin/sh
# Builds the Linux client package in Docker:
#   dist/backroom-linux-x64.tar.gz
#
# Built on Ubuntu 22.04 (glibc 2.35), so it runs on Ubuntu 22.04+, Debian 12+, Fedora 36+,
# Linux Mint 21+ and similar. On an ARM machine (e.g. Apple Silicon) Docker emulates x86-64,
# which takes 15–30 minutes. Called by `scripts/dist.sh --linux`, or on its own.
set -e
cd "$(dirname "$0")/.."
mkdir -p dist

docker run --rm --platform linux/amd64 \
  -v "$PWD":/src:ro -v "$PWD/dist":/dist \
  -v backroom-linux-x64-target:/target -v backroom-linux-x64-cargo:/root/.cargo \
  -v backroom-linux-x64-rustup:/root/.rustup \
  -w /src -e CARGO_TARGET_DIR=/target ubuntu:22.04 sh -c '
    set -e
    export DEBIAN_FRONTEND=noninteractive PATH=/root/.cargo/bin:$PATH
    apt-get update -qq
    apt-get install -y -qq curl ca-certificates build-essential cmake pkg-config \
      libasound2-dev libx11-dev libxi-dev libxtst-dev >/dev/null
    # The cargo shim can be cached without a toolchain — check that it really runs.
    cargo --version >/dev/null 2>&1 || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    cargo build --release -p backroom
    tar -czf /dist/backroom-linux-x64.tar.gz -C /target/release backroom
  '
ls -lh dist/backroom-linux-x64.tar.gz
