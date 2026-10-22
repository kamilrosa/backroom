# Backroom

A simple voice chat for a group of friends who play games together. Self-hosted server (runs even on a
Raspberry Pi), clients for Windows, macOS and Linux. One goal: **it should just work**, without fiddling
with sliders.

| Common voice chat problem | What Backroom does |
|---|---|
| You can hear the dog outside, the keyboard, doors slamming | Neural-network noise suppression (DeepFilterNet3), always on. The microphone opens when the model recognizes **speech**, not when it gets loud. On a recording of real barking, not a single bark-only frame was transmitted, while 97–99% of speech got through. |
| Hunting for the "sweet spot" in the settings | One setting: sensitivity *Low / Normal / High*. Speaker volume is levelled automatically. |
| The microphone or speakers "drop out" and only reconnecting helps | An audio watchdog checks every 250 ms that the sound card is alive. When a device freezes, gets unplugged or the system default device changes, it reopens it in about a second — without leaving the server. |
| Dropped connection | The client gets back into the conversation by itself. The microphone and speakers are not touched in the meantime. |

## Quick start

Backroom has two parts: a **server** that one person in the group runs, and a **client** that everyone
installs.

1. **Run the server** (once, for the whole group) on a Raspberry Pi, a home PC or a VPS — a ready-made
   Docker image (`ghcr.io/kamilrosa/backroom-server`) or a single static binary from the
   [Releases page](../../releases/latest). See [server/README.md](server/README.md).
2. **Download the client** from the [Releases page](../../releases/latest):

   | System | File |
   |---|---|
   | Windows 10/11 | `backroom-windows-x64.zip` |
   | macOS 11+ (Intel and Apple Silicon) | `backroom-macos.zip` |
   | Linux x86-64 (Ubuntu 22.04+, Debian 12+, Fedora 36+) | `backroom-linux-x64.tar.gz` |

3. **Connect:** enter the server address, a nickname and the room password.

Installation details, usage and troubleshooting: [client/README.md](client/README.md).

## Build from source

You need [Rust](https://rustup.rs) (stable) and CMake. Always build with `--release` — noise suppression
cannot keep up in real time in a debug build. The first build downloads about 300 MB of dependencies
(including the DeepFilterNet model) and takes a few minutes.

```bash
git clone <this repository> backroom && cd backroom
```

**macOS**

```bash
brew install cmake
scripts/macos-app.sh            # builds target/Backroom.app
open target/Backroom.app
```

**Linux (Debian/Ubuntu)**

```bash
sudo apt install cmake pkg-config libasound2-dev libx11-dev libxi-dev libxtst-dev
cargo build --release -p backroom
./target/release/backroom
```

**Windows**

Install the [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
("Desktop development with C++") and [CMake](https://cmake.org/download/), then:

```powershell
cargo build --release -p backroom
.\target\release\backroom.exe
```

The released Windows `.exe` is cross-compiled on macOS with MinGW; a native MSVC build has not been
tested yet — reports are welcome.

**Server**

```bash
cargo run --release -p backroom-server -- --password secret
```

More on building, tests, tools and the architecture: [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Known limitations

- A single room, no channels.
- No echo cancellation: when someone plays on speakers, others may hear themselves echoed back.
- Barking **while you are talking** is heavily attenuated, but does not disappear 100%.
- The program is not signed with a publisher certificate, so SmartScreen and Gatekeeper show warnings.

## License

[MIT](LICENSE). All dependencies are open source (mostly MIT/Apache-2.0), notably:

- [DeepFilterNet](https://github.com/Rikorose/DeepFilterNet) — code and bundled model, MIT/Apache-2.0;
- [Opus](https://opus-codec.org/) — BSD-3-Clause;
- `ring` — Apache-2.0 + ISC;
- egui fonts — OFL-1.1 / Ubuntu Font Licence;
- `option-ext` — MPL-2.0, used unmodified.

`cargo metadata` lists them all.
