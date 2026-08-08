## What this changes

<!-- Describe the problem being solved, not just the diff. -->

## Why

<!-- The reasoning that is not recoverable from reading the code: the
     alternative you rejected, the bug that motivated a guard, the measurement
     behind a constant. -->

## Checklist

- [ ] `cargo test --workspace` passes
- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets` is clean
- [ ] Cross-checked the other platform (`cargo check --target ...`)
- [ ] New behaviour has tests; bug fixes have a regression test
- [ ] If the wire format changed: `PROTOCOL_VERSION` bumped and CHANGELOG notes it
