# Backroom — server

The server is a single voice room. It checks the password, keeps the list of people and relays encrypted
voice packets from each player to everyone else. It never decodes or mixes audio, so very weak hardware is
enough. Measured in a container: **2.5 MB RAM, 0.03% CPU**.

It can run next to Mumble: Backroom uses port **7777/UDP**, Mumble uses 64738.

The server is a command-line program with no window. In Docker it runs in the background and starts
again by itself after a reboot.

There are three ways to run it — pick one:

| | When to use it |
|---|---|
| [A. Docker with the ready-made image](#a-docker-with-the-ready-made-image-recommended) | Recommended. Raspberry Pi, home PC or VPS with Docker. |
| [B. Ready-made binary, no Docker](#b-ready-made-binary-no-docker) | You do not want Docker. |
| [C. Build from source](#c-build-from-source) | You changed the code. |

Supported architectures: x86-64 (PC, VPS), arm64 (Raspberry Pi with a 64-bit OS) and armv7 (Raspberry Pi
with a 32-bit OS). Check yours with `uname -m`: `x86_64`, `aarch64` or `armv7l`.

## A. Docker with the ready-made image (recommended)

Docker picks the right image for your architecture automatically. Install Docker with the compose plugin
first (`curl -fsSL https://get.docker.com | sh`), then on the machine that will run the server:

```bash
mkdir backroom && cd backroom
curl -fsSLO https://raw.githubusercontent.com/kamilrosa/backroom/main/docker-compose.yml
curl -fsSL https://raw.githubusercontent.com/kamilrosa/backroom/main/.env.example -o .env
nano .env                   # set BACKROOM_PASSWORD
docker compose up -d
docker compose logs         # you should see: listening on 0.0.0.0:7777 (UDP)
```

The image is about 6 MB.

| What | Command (in `~/backroom`) |
|---|---|
| Who connected, errors | `docker compose logs -f` |
| Change the password | edit `.env`, then `docker compose up -d` |
| Update to the latest version | `docker compose pull && docker compose up -d` |
| Stop the server | `docker compose down` |

The server certificate lives in the `backroom-data` volume, so it survives updates.

## B. Ready-made binary, no Docker

Download the file for your architecture from the [Releases page](../../../releases/latest):

| Architecture | File |
|---|---|
| x86-64 (PC, VPS) | `backroom-server-linux-x64.tar.gz` |
| arm64 (Raspberry Pi, 64-bit OS) | `backroom-server-linux-arm64.tar.gz` |
| armv7 (Raspberry Pi, 32-bit OS) | `backroom-server-linux-armv7.tar.gz` |

The binary is statically linked, so it runs on any Linux of that architecture, old or new.

```bash
tar xzf backroom-server-linux-armv7.tar.gz
BACKROOM_PASSWORD=secret ./backroom-server --data-dir ./data
```

To keep it running in the background and start it on boot, install it as a systemd service:

```bash
sudo install backroom-server /usr/local/bin/
sudo nano /etc/systemd/system/backroom.service      # paste the unit below
sudo systemctl enable --now backroom
journalctl -u backroom -f                           # logs
```

```ini
[Unit]
Description=Backroom voice server
After=network-online.target

[Service]
ExecStart=/usr/local/bin/backroom-server --data-dir /var/lib/backroom
Environment=BACKROOM_PASSWORD=change-me
DynamicUser=yes
StateDirectory=backroom
Restart=always

[Install]
WantedBy=multi-user.target
```

## C. Build from source

From a Mac/PC with the project source and Docker, straight onto a Raspberry Pi over SSH:

```bash
scripts/deploy-pi.sh pi@raspberrypi.local          # 32-bit Pi (armv7l)
scripts/deploy-pi.sh pi@raspberrypi.local arm64    # 64-bit Pi (aarch64)
```

The script builds the image, sends it to the Pi and copies `docker-compose.yml`. On the first run it
creates `~/backroom/.env` and stops so you can set the password; after that,
`cd ~/backroom && docker compose up -d`. On later runs it restarts the server by itself.

Other options:

```bash
docker build -t ghcr.io/kamilrosa/backroom-server .    # on the machine itself; then docker compose up -d
cargo build --release -p backroom-server              # plain binary in target/release/
```

Building on a Pi 4 takes about 10–15 minutes.

## Router

Forward **UDP 7777** to the server's address, the same way you did for Mumble. Give your friends the
public address (or your DDNS name) and the password. To use a different port with Docker, change the left
side in `docker-compose.yml`, e.g. `"9000:7777/udp"`, and give out the address as `address:9000`.

## Check without audio

From any computer with Rust and the project source:

```bash
cargo run --release -p backroom --example probe -- your.address:7777 password
# OK: "Backroom", 1 in the room:
#   - probe (ping 12 ms)
```

## Configuration

Every option works as a command-line argument or an environment variable (in Docker you set it in `.env`).

| Argument | Variable | Default | Meaning |
|---|---|---|---|
| `--bind` | `BACKROOM_BIND` | `0.0.0.0:7777` | UDP address and port |
| `--password` | `BACKROOM_PASSWORD` | *(empty = no password)* | Shared room password |
| `--name` | `BACKROOM_NAME` | `Backroom` | Room name shown in the client |
| `--data-dir` | `BACKROOM_DATA` | `./data` (`/data` in Docker) | Where the server certificate is stored |
| `--max-users` | `BACKROOM_MAX_USERS` | `16` | Maximum number of people |

Set the log level with `RUST_LOG`, e.g. `RUST_LOG=debug`.

## Certificate

On first start the server generates its own certificate and prints its fingerprint in the log. Clients
remember it on the first connection. Keep the data directory (the `backroom-data` volume in Docker) — if
it is lost, the server generates a new certificate and clients will see the warning "The server presented
a different certificate than usual" with an *It's my server — trust and connect* button.
