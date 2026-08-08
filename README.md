<h1 align="center">ClipSync</h1>

<p align="center">
  <strong>Encrypted peer-to-peer clipboard sync for Windows and macOS.</strong><br>
  Copy on one machine, paste on another. Text, images, and files.
</p>

<p align="center">
  <a href="https://github.com/doujianghub/clipsync/actions/workflows/ci.yml"><img src="https://github.com/doujianghub/clipsync/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/doujianghub/clipsync/releases/latest"><img src="https://img.shields.io/github/v/release/doujianghub/clipsync" alt="Release"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg" alt="License"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS-lightgrey" alt="Platform">
  <img src="https://img.shields.io/badge/rust-1.75%2B-orange.svg" alt="Rust 1.75+">
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.zh-CN.md">简体中文</a>
</p>

---

ClipSync runs quietly in your tray. Pair two devices once with a 4-digit code —
no accounts, no servers, no IP addresses to type — and from then on anything you
copy shows up on the other machine. Traffic is end-to-end encrypted and travels
directly between your devices; nothing passes through a third party.

Think of it as **Apple's Universal Clipboard, except it works between Windows
and macOS** — and without an Apple ID, an account of any kind, or a cloud
service in the middle.

> [!NOTE]
> **The application interface is currently Chinese-only.** Menus, dialogs, and
> log messages are in Simplified Chinese. Everything works identically
> regardless of your system language, but you will be reading Chinese labels.
> Localisation is on the roadmap; this documentation is in English so you can
> evaluate, build, and contribute to the project in the meantime.

## Use cases

**You work across a Mac and a PC.** This is the case Universal Clipboard leaves
out entirely: the moment one of your machines runs Windows, Apple's clipboard
sharing stops being an option. Copy a URL, a paragraph, a screenshot, or a
`.zip` on either side and paste it on the other.

**You keep sending yourself files through a chat app.** Messaging yourself a
file to move it between your own two computers works, but it uploads your data
to somebody's server, takes several clicks, and mangles image quality. Copy →
paste is faster and the bytes never leave your machines.

**You copy things out of a remote session.** Working on a desktop over screen
sharing or Remote Desktop, you often want a command, a log excerpt, or an API
token on your local machine. ClipSync runs independently of the remote session,
so the clipboard keeps working even where the session's own clipboard
integration does not — and on macOS transfers are marked as background traffic,
so a large file will not degrade the screen sharing you are looking at.

**Your workplace does not allow cloud sync.** There is no server component, no
account, and no telemetry. Traffic goes directly between your devices over your
own LAN or your own VPN, so it stays inside whatever network boundary you
already have.

**You have more than two machines.** A desktop, a laptop, and a work machine can
form one group without pairing every combination — devices introduce each other,
and any one of them can be offline without breaking the rest.

**You move files, not just text.** Copying a file in Explorer or Finder and
pasting it on the other machine transfers the actual file — with resume,
integrity verification, and no size limit beyond what you configure.

## Features

- **Zero configuration after pairing.** One 4-digit code establishes mutual
  trust permanently. No account, no cloud service, no config file to edit.
- **Text, images, and files.** Large files transfer in chunks with resume
  support; files above a configurable size wait for you to click *Fetch*
  instead of downloading automatically.
- **End-to-end encrypted.** Noise protocol (`Noise_IK`) with per-device static
  keys. Pairing uses SPAKE2, so the 4-digit code never crosses the wire.
- **Finds the best path by itself.** Same-subnet direct connection is preferred,
  then overlay networks, then public addresses. **No vendor integration:** it
  enumerates your network interfaces, so Tailscale, ZeroTier, NetBird, Nebula,
  WireGuard, and plain port forwarding all work without configuration.
- **Stays out of the way.** ~11 MB idle memory, ~0.03% idle CPU, a 1.4 MB
  binary, and a single tray icon. File transfers are marked as background
  traffic on macOS so they yield to screen sharing and video calls.
- **Respects sensitive content.** Clipboard entries marked confidential by
  password managers are skipped automatically.

## How it compares

| | Works Windows ↔ macOS | Data stays local | Automatic | Files |
|---|:---:|:---:|:---:|:---:|
| **ClipSync** | ✅ | ✅ | ✅ | ✅ |
| Apple Universal Clipboard | ❌ Apple devices only | ✅ | ✅ | ✅ |
| Messaging yourself (WeChat, Telegram…) | ✅ | ❌ via their servers | ❌ manual | ✅ |
| Cloud clipboard managers | ✅ | ❌ via their servers | ✅ | varies |
| Windows Cloud Clipboard | ❌ Windows only | ❌ via Microsoft | ✅ | ❌ text only |
| KDE Connect | ⚠️ Linux/Android focused | ✅ | ✅ | ✅ |
| Remote Desktop / VNC clipboard | ✅ | ✅ | ⚠️ session-bound | ⚠️ limited |
| Shared folder or USB drive | ✅ | ✅ | ❌ manual | ✅ |

ClipSync's niche is the combination: **cross-platform, peer-to-peer, automatic,
and handles files** — with nothing to sign up for.

It is deliberately *not* a clipboard history manager. There is no searchable
archive of everything you have ever copied; it syncs the current clipboard and
nothing more. Pair it with a local history tool if you want both.

## Installation

### Download a release

Grab the latest build for your platform from
[Releases](https://github.com/doujianghub/clipsync/releases/latest).

| Platform | Asset |
|---|---|
| macOS (Apple Silicon + Intel) | `ClipSync-<version>-macos-universal.zip` |
| Windows (x64) | `ClipSync-<version>-windows-x64.zip` |

**macOS.** Drag `ClipSync.app` into *Applications*. Because releases are signed
ad-hoc rather than with a paid Developer ID certificate, macOS blocks the first
launch: **right-click the icon → Open** (double-clicking gives you no override
button). On macOS 15 and later, go to *System Settings › Privacy & Security* and
click *Open Anyway*. If you see "damaged or can't be opened", the signature was
mangled in transit — run `xattr -cr /Applications/ClipSync.app`.

**Windows.** Unzip anywhere and run `clipsync.exe`. SmartScreen may warn on
first run for the same reason; choose *More info → Run anyway*.

### Build from source

Requires Rust 1.75 or later.

```bash
git clone https://github.com/doujianghub/clipsync
cd clipsync
cargo build --release
```

The binary lands at `target/release/clipsync`. **Use a release build** — debug
builds are roughly 25× slower for transfers because encryption and hashing are
unoptimised.

To produce a macOS `.app` bundle:

```bash
scripts/package-macos.sh                    # current architecture
scripts/package-macos.sh --universal        # Intel + Apple Silicon
scripts/package-macos.sh --universal --dmg  # also build a .dmg
```

## Quick start

1. Launch ClipSync on both machines. A tray icon appears.
2. On machine A: tray menu → **显示配对码… / Show pairing code**. A 4-digit code
   appears with a live countdown.
3. On machine B: tray menu → **输入配对码… / Enter pairing code**. Type those
   four digits.
4. Done. Copy something on either machine.

You never type an IP address — machine B finds machine A on its own. The code is
valid for 3 minutes, pairs exactly one device, and dies on first success.

> **Why only 4 digits?** The code has to be read aloud or typed by hand — using
> the clipboard to transfer it would be circular. 10,000 combinations is held up
> by a **5-guess-per-session limit**: SPAKE2 makes offline brute force
> impossible, and online guessing was measured at 818 attempts/second, which
> would exhaust the space in seconds without a cap. With the cap, the chance of
> a session being guessed is 5/10000 regardless of how long the code lives.

### Three or more devices

You do not need to pair every pair of machines. Connected devices **introduce
each other**: pair A↔B and A↔C, and then B and C discover one another through A
and connect directly. After that, **A can go offline** without affecting B↔C.

Introduction means trust is transitive — you trust B, B trusts C, so you trust C.
The device list marks these entries as *introduced by …* so you can tell them
apart from the ones you added yourself.

## Command line

Everyday use needs no commands at all; the tray covers everything. These exist
for scripting and troubleshooting:

```bash
clipsync                       # run sync + tray (default)
clipsync pair --host           # host a pairing session, show the code
clipsync pair <code>           # join — finds the host automatically
clipsync pair <host-ip> <code> # manual fallback if discovery fails
clipsync list                  # list paired devices and known addresses
clipsync addrs                 # show this machine's reachable addresses
clipsync clipdiag              # diagnose clipboard/permission problems
clipsync autostart [on|off]    # query or set launch-at-login
```

## Configuration

Settings live in `settings.json` inside the config directory and are all
editable from the tray menu — you rarely need to touch the file.

| Key | Default | Meaning |
|---|---|---|
| `listen_port` | `47684` | TCP port for sync connections |
| `auto_fetch_bytes` | 100 MiB | **Received** files up to this size download automatically; larger ones wait for you to click *Fetch*. Does not affect sending, text, or images |
| `allow_image` / `allow_files` | `true` | Content type switches (apply both ways) |
| `file_cache_bytes` | 1 GiB | Resume-cache ceiling, evicted least-recently-used |
| `upload_limit_bytes_per_sec` | `0` | File upload rate limit; `0` means unlimited |
| `compress_transfers` | `true` | Adaptive compression before sending files |
| `verbose_log` | `false` | Record debug-level detail |

Config directory:

- **Windows** — `%APPDATA%\ClipSync\`
- **macOS** — `~/Library/Application Support/ClipSync/`

A corrupted config never prevents startup: it is moved aside as `.bad`, defaults
are restored, and the event is logged.

### Environment variables

| Variable | Purpose |
|---|---|
| `CLIPSYNC_LOG` | Log level, e.g. `debug` |
| `CLIPSYNC_CONFIG_DIR` | Alternate config directory (useful for running two instances on one machine) |
| `CLIPSYNC_PEERS` | Extra peer addresses, `ip:port` comma-separated, for cases discovery cannot reach |
| `CLIPSYNC_NO_WATCH` | Disable local clipboard monitoring (receive-only device) |
| `CLIPSYNC_NO_TRAY` | Run headless, no tray |
| `CLIPSYNC_DEVICE_NAME` | Override this machine's display name |

## How it works

### Finding the other machine

Addresses come from three general-purpose sources, and no code anywhere knows
what "Tailscale" is:

1. **Exchanged during pairing** — both sides learn every address the other has,
   including virtual interfaces.
2. **LAN multicast beacons** — same-subnet devices announce periodically, so
   sync survives DHCP changes.
3. **Told over the encrypted channel** — connected peers keep each other
   updated. As long as one path works, addresses for the others propagate.

Candidates are then ranked by topological distance: same-subnet direct, then
overlay/VPN, then public. Run `clipsync addrs` to see what your machine
advertises.

> **Why are beacons plaintext and unauthenticated?** They only *hint* at
> addresses; authentication is the transport layer's job. Forging a beacon
> achieves nothing beyond one failed connection attempt, because the `Noise_IK`
> handshake requires the peer's static private key.

### Ports

| Port | Purpose | Protocol |
|---|---|---|
| 47684 | Sync connections (`listen_port`) | TCP |
| 47685 | Pairing (listening only during `pair`) | TCP |
| 47690 | Device discovery beacon | UDP multicast |
| 47691 | Pairing discovery beacon (during `pair` only) | UDP multicast |

The multicast group is `239.255.71.83`, an administratively-scoped address that
does not leak past your router.

### File sync

A clipboard is **last-value-wins**, not a queue. File sync is built around that:

**Copying a file sends a manifest, not the bytes.** The receiver learns what
exists, answers which byte ranges it needs, and only then does data move.

**Copy something else mid-transfer and the transfer is superseded immediately.**
Waiting for the old transfer would block the content you actually want now;
finishing it would leave the peer's clipboard holding the wrong thing. Instead
the sender stops at the next chunk boundary and already-received bytes are kept
for resume.

**Re-copying the same file resumes or completes instantly** — a complete cached
copy syncs with zero transfer; a partial one requests only the remainder. Files
are identified by size and modification time, so an edited file invalidates its
cache.

**Memory stays flat.** Everything is streamed in 256 KB chunks; a 90 MB transfer
holds about 13 MB of memory. Content hashes are verified on completion, so a
file modified mid-transfer is detected and re-sent rather than silently
corrupted.

**Compression is adaptive.** A sample of each file is test-compressed; text,
code, and logs compress and gain roughly 40% effective throughput, while JPEG,
MP4, and ZIP skip it and waste no CPU.

### Performance

Measured on release builds, 90 MB file over loopback:

| Scenario | Time | Throughput |
|---|---|---|
| Incompressible (random data) | 0.61 s | 146 MB/s |
| Compressible (log-like text) | 0.49 s | 191 MB/s effective |
| Rate-limited to 10 MB/s | 8.36 s | matches setting |
| Fully cached repeat | ~1 ms | zero transfer |

Images are compressed at the frame layer before transmission — a 4K screenshot
is 33 MB as raw RGBA and about 6% of that after compression, which matters a
great deal on links slower than gigabit.

## Security

- **Transport:** `Noise_IK` (Curve25519 + ChaCha20-Poly1305 + BLAKE2s). Each
  device holds a static keypair; the private key never leaves the machine and is
  stored with `0600` permissions.
- **Pairing:** SPAKE2 password-authenticated key exchange. The 4-digit code is
  never transmitted, and offline brute force against a captured handshake is not
  possible. Online guessing is capped at 5 attempts per session.
- **Authentication:** peers are identified by static public key. A device that
  is not paired cannot complete a handshake, so unauthenticated traffic is
  rejected before any clipboard data exists.
- **No third party:** there is no relay, no account system, and no telemetry.
  Data goes directly between your devices.

**Threat model.** ClipSync protects clipboard contents in transit against
network observers and against unpaired devices on the same network. It does
**not** protect against a compromised endpoint — anything that can read your
clipboard locally can read what ClipSync syncs.

To report a vulnerability, see [SECURITY.md](SECURITY.md).

## Troubleshooting

Logs live at `logs/clipsync.log` in the config directory (4 MB rotating, one
generation kept). Enable **详细日志 / Verbose logging** in the tray for
debug-level detail — it takes effect without restarting.

Useful signals in the log:

- `本机地址: …` on startup — compare with the peer's line to see whether the two
  machines are even on the same subnet.
- `写入剪贴板耗时 … ms` — only logged when it exceeds 200 ms, so its presence
  means local encoding is a bottleneck.
- `系统拒绝读取 …` — macOS denied file access; the message names the exact
  Settings pane to fix it. `clipsync clipdiag` lists every path with a verdict.

**All devices must run the same protocol version.** The wire format changed in
1.0.0; mixing it with older builds causes repeated reconnects.

## Known limitations

- **The interface is Chinese-only.** Roughly 450 user-facing strings — tray
  menu, dialogs, error messages, and CLI output — are currently hard-coded in
  Simplified Chinese. Localisation is planned but not scheduled; contributions
  are welcome.
- **Linux is not supported.** The code compiles, but clipboard change detection,
  dialogs, and launch-at-login are all no-ops, which makes it non-functional
  rather than merely degraded.
- **Windows visual verification is incomplete.** Dialog DPI/font handling and
  some newer tray items were verified on macOS but not on Windows hardware.
- **Password manager behaviour is simulated in tests.** Sensitive-content
  detection follows the documented platform conventions, but whether a specific
  password manager honours them has not been verified against real software.
- **Promised files** (`com.apple.pasteboard.promised-file-url`) are detected and
  explained by `clipdiag` but not yet transferred.

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the
development setup, testing expectations, and commit conventions.
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) is the fastest way to understand
the codebase: four crates, with all platform-specific code confined to a handful
of files.

Note that source comments are written in Chinese, matching the existing
codebase.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
