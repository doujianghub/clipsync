# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.1.0] - 2026-08-15

Wire-compatible with 1.0.0 — the protocol version is unchanged, so a 1.1.0
device syncs with a 1.0.0 device without reconnect loops.

### Added

- **File transfers now yield to interactive traffic on Windows too.** Sync
  connections are declared as background flows through qWAVE
  (`QOSTrafficTypeBackground`, DSCP CS1) and their kernel send backlog is
  capped, so copying a large file no longer degrades a Remote Desktop session
  or a video call running over the same link. macOS has done this since 1.0.0;
  Windows was a no-op, which is exactly the platform most likely to be on the
  far end of a remote desktop connection.
- **Dead connections are now detected in about a minute** instead of the
  kernel's default retransmission timeout of roughly fifteen. TCP keepalive is
  enabled (30 s idle, 10 s interval, 3 probes) along with a retransmission
  deadline. A peer that vanishes without closing — lid closed, Wi-Fi switched,
  VPN dropped — used to leave a zombie connection registered, and the new
  connection it opened on waking was rejected as a duplicate; both devices then
  sat there online but not syncing.

### Changed

- **Broadcasting to several devices now compresses once**, not once per
  connection. Serialisation and compression are shared across peers; only
  encryption stays per-connection, since each Noise session has its own keys
  and nonce sequence. Previously a 4K screenshot cost an extra 15 ms of
  serialisation, 40 ms of deflate, and a 33 MB memory copy for every additional
  device — the last device in a group waited longest.
- Per-stage timings (encode, send, receive, decompress) are logged when they
  exceed 200 ms, with throughput included for the byte-moving stages.

### Fixed

- **A `settings.json` containing only some fields is no longer treated as
  corrupt.** Four keys lacked defaults, so omitting any one of them made the
  whole file fail to parse; it was then renamed `.bad` and silently replaced by
  defaults. The README documents this file as hand-editable, which it was not.
- `scripts/package-macos.sh` no longer runs backticked words inside the
  `Info.plist` XML comments as shell commands.

## [1.0.0] - 2026-08-08

First public release.

### Added

- **Clipboard sync** for text, images, and files between Windows and macOS,
  with loop prevention, deduplication, and sensitive-content skipping.
- **One-time pairing codes.** A 4-digit SPAKE2 code establishes mutual trust;
  the code never crosses the wire and online guessing is capped at 5 attempts
  per session. The tray shows a live countdown and can regenerate the code.
- **End-to-end encryption** with `Noise_IK` (Curve25519 + ChaCha20-Poly1305 +
  BLAKE2s). Devices are authenticated by static public key.
- **Vendor-neutral path selection.** Local interfaces are enumerated directly
  and ranked by topological distance, so Tailscale, ZeroTier, NetBird, Nebula,
  WireGuard, and port forwarding work without any integration code.
- **Peer introduction.** Connected devices introduce each other, so a group of
  three or more does not require pairing every combination, and any single
  device can go offline without breaking the others.
- **File transfer** with manifest-first negotiation, 256 KB streaming chunks,
  resume after interruption, content-hash verification, and an LRU cache.
  Transfers are superseded immediately when the clipboard changes, and never
  block text sync.
- **Deferred fetch.** Received files above `auto_fetch_bytes` wait for an
  explicit click instead of downloading automatically, surfaced through a tray
  menu entry, an icon badge, and the tooltip.
- **Adaptive compression** for both file chunks and frame-level payloads.
  Images travel as raw RGBA on the clipboard — a 4K screenshot is 33 MB, and
  roughly 6% of that after compression.
- **Background traffic classification on macOS.** Sync connections are marked
  `NET_SERVICE_TYPE_BK` so they yield to interactive traffic such as screen
  sharing, and `TCP_NOTSENT_LOWAT` bounds local queueing.
- **System tray** with connection status, transfer progress, pause, launch at
  login, per-type switches, and an advanced submenu for limits, port,
  compression, and logging.
- **Diagnostics.** `clipsync clipdiag` reports per-path clipboard and permission
  verdicts; startup logs local addresses; slow clipboard reads and writes are
  logged when they exceed 200 ms.
- **English and Simplified Chinese interface.** Follows the system language by
  default (`NSLocale.preferredLanguages` on macOS, `GetUserDefaultLocaleName`
  on Windows) and is switchable from the tray without restarting. Log output
  stays in Chinese by design — it is diagnostic rather than interface text.
- **macOS file permission handling.** Access denials are detected at copy time
  rather than silently producing stale content on the peer, with a one-time
  dialog naming the exact Settings pane to fix.

### Security

- Private keys are stored with `0600` permissions and never leave the device.
- Pairing sessions end on timeout, success, or 5 failed attempts.
- Discovery beacons are unauthenticated by design; they only hint at addresses,
  and all authentication happens in the `Noise_IK` handshake.

### Known limitations

- Log messages are Chinese-only; the interface itself is fully localised.
- Linux compiles but is non-functional: clipboard change detection, dialogs, and
  launch-at-login are no-ops.
- Promised files (`com.apple.pasteboard.promised-file-url`) are detected and
  explained but not transferred.
- Some Windows dialog and tray details were verified on macOS only.

[1.1.0]: https://github.com/doujianghub/clipsync/releases/tag/v1.1.0
[1.0.0]: https://github.com/doujianghub/clipsync/releases/tag/v1.0.0
