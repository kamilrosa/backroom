#!/bin/sh
# Builds client packages to hand out to friends (run on macOS):
#   dist/backroom-macos.zip        — Backroom.app (Intel + Apple Silicon)
#   dist/backroom-windows-x64.zip  — backroom.exe (cross-compiled with MinGW)
#   dist/backroom-linux-x64.tar.gz — only with --linux (built in Docker, see linux-package.sh)
#
# Requirements: Rust, CMake, `brew install mingw-w64`; Docker running for --linux.
set -e
cd "$(dirname "$0")/.."
mkdir -p dist

echo "== macOS"
scripts/macos-app.sh universal
rm -f dist/backroom-macos.zip
ditto -c -k --keepParent target/Backroom.app dist/backroom-macos.zip

echo "== Windows"
rustup target add x86_64-pc-windows-gnu >/dev/null
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++ \
AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar \
  cargo build --release --target x86_64-pc-windows-gnu -p backroom
rm -f dist/backroom-windows-x64.zip
(cd target/x86_64-pc-windows-gnu/release && zip -q -j ../../../dist/backroom-windows-x64.zip backroom.exe)

if [ "$1" = "--linux" ]; then
  echo "== Linux x64 (Docker)"
  scripts/linux-package.sh
fi

ls -lh dist
