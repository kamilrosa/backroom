# Backroom — development

Building, testing and how the code works. For using the program see [client/README.md](../client/README.md),
for running the server see [server/README.md](../server/README.md).

## Repository layout

| Path | Contents |
|---|---|
| `proto/` | Shared protocol: control messages, voice packet format |
| `server/` | Server — a library (`start()`, also used by tests) plus a thin command-line `main.rs` |
| `client/` | Client with the window — a library (`backroom_client`) plus `main.rs` |
| `client/tests/` | End-to-end test: real server + clients over QUIC on localhost |
| `client/examples/` | `mic_eval` and `probe` tools |
| `scripts/` | `dist.sh` (client packages), `linux-package.sh` (Linux client), `server-packages.sh` (server binaries), `publish-image.sh` (server image to ghcr.io), `deploy-pi.sh` (server from source on a Pi), `macos-app.sh` (`Backroom.app`) |
| `Dockerfile`, `docker-compose.yml` | Server image (amd64, arm64, armv7; static musl binary on `scratch`) |

## Requirements

- Rust (stable) and CMake (to build Opus).
- Linux additionally: `sudo apt install cmake pkg-config libasound2-dev libx11-dev libxi-dev libxtst-dev`.
- Windows `.exe` built on macOS: `brew install mingw-w64`.
- Server images and the Linux client package: Docker.

Always build the client with `--release`: in a debug build noise suppression cannot keep up in real time
(dependencies are optimized even in dev builds, but the client itself is not).

## Building

```bash
cargo build --release -p backroom          # client
cargo build --release -p backroom-server   # server

scripts/dist.sh            # dist/backroom-macos.zip (universal) + dist/backroom-windows-x64.zip
scripts/dist.sh --linux    # + dist/backroom-linux-x64.tar.gz (Docker, Ubuntu 22.04 base)
scripts/linux-package.sh   # only the Linux package

scripts/macos-app.sh             # Backroom.app for this Mac's architecture
scripts/macos-app.sh universal   # Intel + Apple Silicon

# Windows .exe on macOS, by hand
rustup target add x86_64-pc-windows-gnu
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  cargo build --release --target x86_64-pc-windows-gnu -p backroom
```

## Making a release

Everything is built locally; there is no CI pipeline.

```bash
scripts/dist.sh --linux          # client: dist/backroom-{macos,windows-x64,linux-x64}.*
scripts/server-packages.sh       # server: dist/backroom-server-linux-{x64,arm64,armv7}.tar.gz
scripts/publish-image.sh         # server image: ghcr.io/kamilrosa/backroom-server:<version> + :latest
gh release create v0.1.0 dist/* --title "Backroom 0.1.0" --notes "..."
```

Bump `version` in the root `Cargo.toml` before a new release. `publish-image.sh` needs a one-time
`gh auth refresh -h github.com -s write:packages` and `gh auth token | docker login ghcr.io -u <user> --password-stdin`.
New GitHub packages are private by default — make the package public once in its settings on GitHub.

On macOS the client needs the `.app` bundle: without the microphone entry in `Info.plist` the system only
grants microphone access when the binary is started from a terminal.

## Running locally

```bash
cargo run -p backroom-server -- --password test
cargo run --release -p backroom
BACKROOM_CONFIG_DIR=/tmp/second cargo run --release -p backroom   # a second client on the same machine
```

## Tests and tools

```bash
cargo test --workspace     # unit tests + end-to-end test
cargo clippy --workspace
```

The end-to-end test (`client/tests/e2e_net.rs`) starts a real server and several network clients in one
process and checks: voice relaying (no echo back to the sender), mute and deafen, wrong password, full
room, duplicate nickname (no kick loop), automatic reconnection after a server restart and detection of a
changed certificate.

```bash
# Microphone chain on WAV files (48 kHz mono 16-bit): what share of noise-only and of speech
# frames would be transmitted, plus out.wav with what others would hear
cargo run --release -p backroom --example mic_eval -- speech.wav noise.wav [noise_gain_dB] [out.wav]

# Server check without audio
cargo run --release -p backroom --example probe -- address[:port] password [nickname]
```

## How it works

```
microphone ─► watchdog ─► 48 kHz ─► DeepFilterNet3 ─► "is this speech" gate ─► auto gain
           ─► Opus 32 kb/s + FEC ─► QUIC datagram ─► server (relay only) ─► everyone else

others ─► adaptive jitter buffer (FEC/PLC on loss) ─► mix ─► watchdog ─► speakers
```

### Protocol (`proto/`)

- **QUIC** (`quinn`), one connection per client, ALPN `backroom/1`.
- **Control:** a single bidirectional stream opened by the client with `\n`-terminated JSON messages
  (`Hello`, `State` → `Welcome`, `Reject`, `Joined`, `Left`, `State`, `Pings`).
- **Voice:** QUIC datagrams. Client → server: `[kind][flags][seq u32]` + Opus; server → client additionally
  carries the sender id. Flags `START`/`END` mark the boundaries of an utterance.
- A late voice packet is useless, so there are no retransmissions; Opus FEC and PLC conceal losses.

### Server (`server/src/lib.rs`)

- Decodes nothing: it rewrites each datagram with the sender id and relays it to the others.
- **Encryption:** TLS 1.3, preferring ChaCha20-Poly1305 because the Raspberry Pi 4 has no hardware AES.
- **Certificate:** self-signed, generated on first start; clients pin its SHA-256 fingerprint (TOFU).
- **Same nickname** from a new connection replaces the old session (a "ghost" after a dropped connection).
  The kicked client gets close code `2` and does **not** reconnect by itself, so two people with the same
  nickname do not keep kicking each other out.
- Muted users are not relayed; deafened users receive nothing, which saves bandwidth.
- Every 2 s the server broadcasts each user's ping (QUIC RTT).

### Client (`client/src/`)

| Module | Role |
|---|---|
| `audio.rs` | Capture and playback threads, the **device watchdog**, the mic test |
| `mic.rs` | Microphone chain: denoising → speech gate → auto gain → Opus, with a 40 ms pre-roll |
| `dsp.rs` | Resampler, DeepFilterNet wrapper, AGC, gate, fallback SNR estimator |
| `jitter.rs` | Adaptive jitter buffer: FEC for a single loss, PLC for gaps, latency trimming |
| `net.rs` | QUIC, automatic reconnection (1 → 2 → 4 → 5 s), certificate TOFU |
| `hotkey.rs` | Global push-to-talk by polling the keyboard and mouse every 10 ms |
| `state.rs` | State shared between threads (atomics for audio, a mutex for display state) |
| `config.rs` | Settings persisted as JSON |
| `ui.rs` | The window (egui) |

**Device watchdog.** Every 250 ms it checks each audio stream and reopens the device when:

- the driver reports an error;
- the callback has not been called for more than 1.5 s;
- the system default device changes;
- the microphone delivers digital silence for 3 s (retried every 15 s).

**Speech gate.** It decides from the SNR that DeepFilterNet returns (speech about +20…+35 dB, barking about
−15…−3 dB), not from loudness. The threshold depends on the sensitivity: 15 / 8 / 3 dB. After the last
speech frame the gate stays open for another 300 ms; when it opens, the previous 40 ms are sent as well so
word onsets are not clipped.

**Mic test.** Records 5 s of the packets that would be sent, decodes them and plays them back. Nothing is
transmitted during recording or playback, so there is no feedback loop even on speakers.
