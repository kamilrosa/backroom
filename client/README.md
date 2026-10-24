# Backroom — client

Everything you need to join the conversation.

Download the package for your system from the [Releases page](../../../releases/latest) (or get it from
whoever runs your server). To build it yourself, see [Build from source](../README.md#build-from-source).
You also need a server to connect to — the address and password come from whoever runs it
([how to run one](../server/README.md)).

## Installation

| System | File | How to run |
|---|---|---|
| Windows 10/11 | `backroom-windows-x64.zip` | Unzip and run `backroom.exe`. No installation needed. If SmartScreen warns you: *More info → Run anyway*. |
| macOS 11+ | `backroom-macos.zip` | Drag `Backroom.app` to *Applications*. The first time: right-click → *Open* → *Open*, then allow microphone access. |
| Linux x86-64 (Ubuntu 22.04+, Debian 12+, Fedora 36+) | `backroom-linux-x64.tar.gz` | `tar xzf backroom-linux-x64.tar.gz && ./backroom`. Needs ALSA or PipeWire/PulseAudio (standard on desktops). |

## Using it

1. Enter the server address, your nickname and the password, then **Connect**. Next time the program
   connects by itself.
2. **Voice activation** is the default: just talk. If quiet words get cut off, set the sensitivity to
   *High*. If it picks up things it should not, set *Low*.
3. **Push-to-talk:** switch the mode, click *Change* and press a key or a side mouse button.
   The key is not "swallowed" — your game still sees it.
4. **🎙 Mic test** records 5 seconds and plays back exactly what others would hear: after noise
   suppression, the speech gate and compression. The best way to check whether the dog still gets through.
   Nobody hears you during the test.
5. The slider next to a person sets their volume for you only. 🔈 mutes them for you only.
6. **🎤 Microphone** mutes you for everyone. **🔊 Listening / Deafened**: when deafened you hear nothing
   and transmit nothing. **☕ Away** shows "away" next to your nickname until you click it again.
7. **Chat** at the bottom of the list: type and press Enter. People who join later see the last 50
   messages; each person's latest message also shows under their nickname for 5 minutes. While Backroom
   is in the background, the window title counts unread messages (`Backroom (2)`); *Settings → Sound on
   new chat message* adds a short sound. Push-to-talk is ignored while you type, in case the key is a
   letter. Chat needs a server from version 0.2.0.

The bottom of the window shows `🎤 … : OK` and `🔊 … : OK`. If the program fixed a device by itself
(headphones unplugged, a stuck driver, a new default device), hover over the line to see how many times
it did so and why.

**Headphones are recommended.** The client has no echo cancellation yet.

### macOS: push-to-talk

Reading keys while another window (your game) is active needs permission in *System Settings →
Privacy & Security → Accessibility → Backroom*. Without it only voice activation works.

### Linux: push-to-talk

Works in an X11 session. Under Wayland, global key reading is not possible, so use voice activation.

## When something does not work

| Symptom | What to do |
|---|---|
| "The microphone is sending pure silence" | The microphone is muted in the system, or the program has no microphone permission (macOS: *Privacy → Microphone*; Windows: *Privacy → Microphone → Let desktop apps access your microphone*). |
| I hear my own echo | The echo is produced by **someone else**: their microphone picks you up from their speakers or overly loud headphones. Say something short and watch whose dot lights up together with the echo. |
| Others hear me quietly or my words get cut off | Sensitivity *High*, then 🎙 Mic test. |
| "Connection lost — reconnecting…" | The client reconnects by itself. If it takes long, the server is down or the port is not forwarded. |
| "The server presented a different certificate than usual" | Normal after the server was reinstalled. If nobody touched it, ask whoever runs it before you click *trust*. |
| "Someone connected with the same nickname" | You have two windows open, or someone is using your nickname. |

*⚙ Settings → Show log* opens the folder with `backroom.log`. Send it to whoever runs the server.

Settings and the log live in:

- Windows: `%APPDATA%\backroom\Backroom\config\`
- macOS: `~/Library/Application Support/pl.backroom.Backroom/`
- Linux: `~/.config/backroom/`
