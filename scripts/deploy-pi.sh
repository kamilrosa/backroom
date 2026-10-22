#!/bin/sh
# Builds the server image FROM SOURCE on this machine and runs it on a Raspberry Pi over SSH.
# Use it to deploy local changes. To install the released version, the Pi only needs
# docker-compose.yml and .env — see server/README.md.
#
#   scripts/deploy-pi.sh pi@raspberrypi.local            # Pi with a 32-bit OS (armv7l)
#   scripts/deploy-pi.sh pi@raspberrypi.local arm64      # Pi with a 64-bit OS (aarch64)
#
# The Pi needs Docker with the compose plugin. On the first run the script creates
# ~/backroom/.env from .env.example and asks you to set the password.
set -e
cd "$(dirname "$0")/.."
IMAGE=ghcr.io/kamilrosa/backroom-server:latest

HOST=$1
ARCH=${2:-armv7}
if [ -z "$HOST" ]; then
  echo "usage: scripts/deploy-pi.sh user@pi-address [armv7|arm64]" >&2
  exit 2
fi
case "$ARCH" in
  armv7) PLATFORM=linux/arm/v7 ;;
  arm64) PLATFORM=linux/arm64 ;;
  *) echo "unknown architecture: $ARCH (armv7 or arm64)" >&2; exit 2 ;;
esac

echo "== Building the image ($PLATFORM)"
docker buildx build --platform "$PLATFORM" -t "$IMAGE" --load .

echo "== Sending the image to $HOST"
docker save "$IMAGE" | gzip | ssh "$HOST" 'gunzip | docker load'

echo "== Configuring $HOST"
ssh "$HOST" mkdir -p backroom
scp -q docker-compose.yml .env.example "$HOST":backroom/
if ! ssh "$HOST" test -f backroom/.env; then
  ssh "$HOST" cp backroom/.env.example backroom/.env
  echo
  echo "Created ~/backroom/.env on $HOST. Set the password and start the server:"
  echo "  ssh $HOST"
  echo "  nano ~/backroom/.env                  # BACKROOM_PASSWORD=..."
  echo "  cd ~/backroom && docker compose up -d"
  exit 0
fi

echo "== Starting"
ssh "$HOST" 'cd backroom && docker compose up -d && sleep 2 && docker compose logs --tail 3'
