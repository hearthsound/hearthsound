# Hearthsound

Hearthsound is an open-source, self-hostable Rust music server that combines a
Navidrome/Jellyfin-style local library with on-demand playback of any song: a
searched song that is missing from the server is acquired through a user-chosen
downloader plugin and then played, while upcoming queue items are prefetched in
the background. Every layer of the system (catalog search, metadata, artwork,
acquisition, verification, scrobbling, discovery, lyrics, playlist import) is
served by an admin-selected plugin, so the hoster decides which external services
the server talks to.

Status: early development. The server does not serve music yet. The workspace
currently holds the domain types, settings registry, database layer, plugin
manifest and component validation, and the Wasm plugin runtime; the HTTP API,
library scanner, and acquisition pipeline arrive with later work.

## Disclaimer

Hearthsound is a music server and plugin host. It does not include, host, or
distribute any music. Plugins connect Hearthsound to services and tools that you
install and configure yourself. You are responsible for complying with the laws of
your jurisdiction and the terms of the services you use. First-party plugins only
integrate with openly distributed software (such as slskd, Lidarr, and yt-dlp) and
never circumvent DRM.

## Building

Prerequisites:

- Rust via rustup. The toolchain channel and targets are pinned in
  `rust-toolchain.toml`; rustup installs them on first use.
- `cargo-nextest` and `cargo-deny` for the full check suite (see
  `CONTRIBUTING.md`).

```sh
cargo build
cargo run -p hearthsound -- --version
```

## License

AGPL-3.0-or-later. See `LICENSE`.
