# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Project license, contributing guide, security policy, and changelog.
- Cargo workspace with the `hearthsound-core` crate: layer and entity kind enums,
  the shared provider error, and provider instance slugs.
- Catalog provider records and the lifecycle and catalog provider traits.
- Settings registry with environment, config file, and default precedence.
- SQLite database layer with a writer pool, a reader pool, and pre-migration
  backups.
- `hearthsound` binary that reports its version.
- `xtask` maintenance binary with a `sync-wit` command that copies the
  WebAssembly interface files from the plugin SDK and verifies them.
