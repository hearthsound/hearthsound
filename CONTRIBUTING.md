# Contributing

## Setup

Install Rust with [rustup](https://rustup.rs). The channel and the
`wasm32-wasip2` target are pinned in `rust-toolchain.toml`, so rustup installs
them on first use.

Install the extra tools the required checks need:

```sh
cargo install --locked cargo-nextest cargo-deny
```

## Required checks

Run these before considering a change complete:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask fixtures
cargo nextest run --workspace
cargo test --workspace --doc
cargo deny check
cargo xtask sync-wit --check
```

`cargo xtask fixtures` must run before the tests so the plugin-host tests find
their `.hsplugin` archives.

## Commit style

Commit titles start with a capital letter, stay under 75 characters, and carry no
`fix:`/`feat:`-style prefix.

## WebAssembly interface

The WebAssembly interface (WIT) is owned by the `hearthsound/plugin-sdk`
repository. To change it, change it there first, then run `cargo xtask sync-wit`
in this repository to copy it in. Never edit `wit/` by hand.

## Test assets

Do not commit copyrighted audio. Test audio is generated.

## Licensing

Contributions are licensed AGPL-3.0-or-later.
