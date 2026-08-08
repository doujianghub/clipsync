# Contributing to ClipSync

Thanks for taking an interest. This document covers what you need to know to
work on the codebase productively.

## Getting oriented

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) first — it is short and maps
out the four crates and the data flow between them.

| Crate | Responsibility | Platform-specific? |
|---|---|---|
| `clipsync-core` | Message types, sync engine (loop prevention, dedup, filtering). Pure logic, fully unit-testable | No |
| `clipsync-clip` | Clipboard read/write, change detection, sensitive-content probing | Yes |
| `clipsync-net` | Discovery, `Noise_IK` transport, SPAKE2 pairing, frame layer | Partly |
| `clipsync-app` | Tray UI, wiring, config, autostart. Produces the `clipsync` binary | Yes |

All platform-specific code is confined to a handful of files in `clipsync-clip`
and `clipsync-app`. If you find yourself adding `#[cfg(target_os = ...)]`
elsewhere, that is a sign the abstraction is in the wrong place.

## Development setup

Requires Rust 1.82 or later.

```bash
cargo build
cargo test --workspace
```

**Always use `--release` when measuring anything.** Debug builds are roughly 25×
slower for transfers because encryption and hashing dominate and neither is
optimised.

Running two instances on one machine is the fastest way to test sync end to end:

```bash
CLIPSYNC_CONFIG_DIR=/tmp/clipsync-a cargo run --release
CLIPSYNC_CONFIG_DIR=/tmp/clipsync-b cargo run --release
```

## Before you open a pull request

Run all three. The first two are enforced by CI:

```bash
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Plus a cross-check for the platform you are *not* on, since most contributors
only have one:

```bash
# from macOS
rustup target add x86_64-pc-windows-msvc
cargo check --workspace --all-targets --target x86_64-pc-windows-msvc

# from Windows
rustup target add aarch64-apple-darwin
cargo check --workspace --all-targets --target aarch64-apple-darwin
```

This catches the most common cross-platform breakage: a test module gated behind
`#[cfg(target_os = "macos")]` that leaves symbols undefined elsewhere.

## Code conventions

**Comments are written in Chinese**, matching the existing codebase. Please stay
consistent rather than mixing languages within a file.

**User-facing text must be bilingual.** Wrap it in `t!("中文", "English")` (or
`tf!` / `tprintln!` for formatted output) so both languages sit on the same
line — see `clipsync-core/src/i18n.rs`. Log messages are the exception and stay
in Chinese: they are diagnostic output, and one language keeps bug reports
comparable.

**Explain *why*, not *what*.** The code already says what it does. Comments earn
their place by recording the reasoning that is not recoverable from reading it —
the alternative that was rejected, the bug that motivated a guard, the measured
number behind a constant. A comment that restates the line below it is noise.

**Keep files under 400 lines**, ideally 200–300. When a file grows past that,
split it along cohesion lines. The established pattern for splitting out tests
is a sibling file included as a submodule:

```rust
#[cfg(test)]
#[path = "foo_tests.rs"]
mod tests;
```

**Magic numbers need a justification.** Timeouts, buffer sizes, and retry counts
should carry a comment explaining how the value was chosen — ideally with the
measurement that produced it.

## Testing expectations

Every behavioural change needs a test. Beyond that, a few things this project
cares about:

- **Test the behaviour, not the implementation.** Assert on the outcome a user
  would notice, not on internal call sequences.
- **Name tests as claims.** `a_failed_apply_releases_the_echo_slot` beats
  `test_apply_2`. The name should say what breaks if it fails.
- **Regression tests should record the incident.** If you are fixing a bug, the
  test's doc comment is the right place for the log line, error message, or
  reproduction that motivated it.
- **Do not use real network addresses in tests.** Machines running TUN-mode
  proxies can successfully connect to addresses that should be unreachable
  (including RFC 5737 documentation ranges), which makes such tests fail in
  confusing ways. Inject a fake connect function instead.
- **Clipboard tests must take the shared lock.** The system clipboard is a
  global singleton and `cargo test` runs in parallel; see
  `clipsync_clip::clipboard_test_lock`.

## Protocol changes

The wire format is `postcard`-encoded and **not self-describing**. Two rules
follow from that:

1. **New enum variants must be appended last.** Variants are encoded by index,
   so inserting one in the middle silently reinterprets existing messages.
2. **Adding a field to an existing variant is a breaking change.** There is no
   field-name metadata to skip over.

Any incompatible change requires bumping `PROTOCOL_VERSION` in
`clipsync-core/src/message.rs` and documenting it in the version history comment
there. Note that frame-layer changes break *below* the `Hello` exchange, so
version negotiation cannot help — mixed versions will simply reconnect forever.
Call this out prominently in the changelog when it happens.

## Commit messages

Look at `git log` for the house style. In short:

- Write in Chinese, subject line under ~50 characters.
- **Describe the problem, not the diff.** `已连接台数偶现 2/1：托盘存了第二份计数`
  is useful; `修改 tray_status.rs` is not.
- Use the body to record evidence — the log line, the measurement, the reasoning
  behind a tradeoff. Future readers need the why.
- One logical change per commit where practical.

## Reporting bugs

Use the issue templates. For anything sync-related, logs are essential:

1. Enable **Verbose logging** in the tray (takes effect immediately).
2. Reproduce.
3. Attach `logs/clipsync.log` from the config directory **on both machines** —
   one side alone almost never contains enough to diagnose a connection problem.
4. Include `clipsync addrs` output from both, and `clipsync clipdiag` for
   clipboard or permission issues.

Please redact anything you consider private; the logs may contain device names,
local IP addresses, and file names.

## Code of conduct

Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing, you agree that your contributions will be dual licensed under
MIT and Apache-2.0, matching the project.
