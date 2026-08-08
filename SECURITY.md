# Security Policy

## Supported versions

| Version | Supported |
|---|---|
| 1.0.x | ✅ |
| < 1.0 | ❌ |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately through either channel:

- [GitHub private vulnerability reporting](https://github.com/doujianghub/clipsync/security/advisories/new)
  (preferred — it keeps discussion and the eventual advisory in one place)
- Email **wangxin7779@gmail.com** with `[ClipSync security]` in the subject

Please include:

- what the issue is and why you believe it is exploitable,
- the version and platform you observed it on,
- reproduction steps or a proof of concept, if you have one.

You can expect an acknowledgement within 7 days. This is a personal project
maintained in spare time, so please treat any timeline beyond that as
best-effort rather than a guarantee. Once a fix ships, you will be credited in
the advisory unless you ask otherwise.

## What is in scope

ClipSync is designed to protect clipboard contents **in transit**. The following
are in scope:

- Weaknesses in the `Noise_IK` transport or how it is used (key handling, nonce
  reuse, missing authentication, downgrade paths).
- Weaknesses in SPAKE2 pairing — anything that lets an attacker recover the
  code, bypass the 5-attempt cap, or complete a handshake without it.
- Accepting data from an unpaired or unauthenticated peer.
- Memory-safety issues, panics reachable from network input, and resource
  exhaustion triggered by a remote peer (for example decompression bombs).
- Private key material leaking to disk, logs, or the network.
- Path traversal or arbitrary file writes via received file metadata.

## What is out of scope

- **Compromised endpoints.** Anything running on your machine that can read the
  system clipboard can read what ClipSync syncs. This is inherent to what a
  clipboard is, not a flaw to be fixed.
- **Unauthenticated discovery beacons.** These are plaintext by design; they
  only hint at addresses. Forging one achieves nothing beyond causing a failed
  connection attempt, because the handshake still requires the peer's static
  private key.
- **Ad-hoc code signing warnings** on macOS and Windows. These come from not
  having a paid signing certificate and are documented in the README.
- **Physical access** to an unlocked machine.
- Vulnerabilities in dependencies that do not affect ClipSync as it uses them —
  though please still report these; they are useful even when not exploitable
  here.

## Design notes relevant to review

- **Transport:** `Noise_IK_25519_ChaChaPoly_BLAKE2s`. The initiator must already
  know the responder's static public key, which pairing establishes.
- **Pairing:** SPAKE2 over a short-lived TCP listener on port 47685, active only
  during an explicit pairing session. A session ends on success, timeout
  (3 minutes), or 5 failed attempts.
- **Trust is transitive.** Paired devices introduce each other, so trusting a
  device means trusting the devices it trusts. This is deliberate and surfaced
  in the UI, which marks introduced entries as such.
- **Decompression is bounded** by the length the sender declares, and a mismatch
  is an error rather than a truncation.
- **Received filenames are sanitised** before being materialised on disk.
