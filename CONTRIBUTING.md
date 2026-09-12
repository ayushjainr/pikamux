# Contributing to Pika

Start with the [README](README.md), [operating guide](docs/guide.md), and
[architecture](DESIGN.md). A small, reproducible bug report is as useful as a
pull request.

## Development setup

Use macOS or Linux with Rust 1.88 or newer and tmux.

```bash
cargo build --locked
cargo run -- --help
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests use disposable state and fake providers. Real tmux contracts use a
dedicated socket and inert processes. Do not use provider credentials, real
conversations, or fleet machines for routine tests. Packaging checks also use
the platform's scripting tools.

## Changes

Explain the concrete problem and resulting behavior. Keep changes focused;
verify the affected contracts and their meaningful failure cases. Reuse
passing results when the underlying code has not changed.

Preserve exact conversation identity, unread state, and truthful consultation
receipts. Unknown delivery must not trigger a blind retry. Include no
credentials, personal transcripts, or internal project paths.

Pika integrates existing agent harnesses. Discuss changes that add a daemon,
runtime dependency, or new provider workflow before implementing them.

## Releases and reporting

See [releasing](docs/releasing.md) for packaging and publication, and
[SECURITY.md](SECURITY.md) for private vulnerability reporting.
Contributions are under the MIT license; retain applicable third-party notices.
