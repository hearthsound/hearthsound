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
- The `hearthsound:plugin@0.1.0` WebAssembly interface files, synced from the
  plugin SDK.
- Fixture plugins and the `xtask fixtures` command that packs them into
  `.hsplugin` archives.
- Plugin manifest parsing and validation.
- Installation and validation of `.hsplugin` archives, keeping a precompiled
  component for fast loading.
- Plugin instance state with a sandboxed WASI context, deny-by-default host
  imports, and a rate limited log sink.
- Catalog plugins callable through the `CatalogProvider` trait, using pooled
  stores with per-call deadlines and automatic disabling after repeated traps.
- Initial database schema covering accounts, settings, library folders, the
  music catalog, media files and retention, playlists, acquisitions, plugins,
  jobs, caches, and a shared full-text search index kept in step by triggers.
- `PRAGMA data_version` watching, notifying the running server when another
  connection such as a CLI command writes to the database.
- The client id codec: UUIDv7 generation, materialized `<prefix>_<26 character
  Crockford base32>` ids for entities, and `x_<slug>_<kind>_<payload>` ids for
  catalog entities, both parsed by one entry point that dispatches on the form.
- Availability, acquisition state with its transition table, acquisition
  priority, failure reason, pin reason, granularity, quality tier, import mode,
  and storage class literals, each serialized as its exact wire string.
- Metadata and acquisition provider records mirroring the plugin interfaces,
  the `MetadataProvider` and `AcquisitionProvider` traits, and the per-job
  staging context acquisition calls carry.
- The server event vocabulary covering every event type of the API, serialized
  with its exact type string and camelCase payload, and the event bus that
  sequences events, broadcasts them, and replays the last thousand to a
  reconnecting subscriber.
- Bootstrap configuration: typed server, path, database, log, secret, and web
  keys loaded from an optional TOML file and `HEARTHSOUND__` environment
  variables, with `<data>` substitution, the XDG data directory default, and
  the derived data and storage directories.
- The runtime settings reference: every library, storage, acquisition, prefetch,
  stream, search, Subsonic, privacy, plugin, upload, user, and auth setting with
  its default, its constraint, a typed key, and a flag marking the two settings
  a user may override.
- Runtime settings stored in the `settings` table, resolved as environment,
  config file, database, then default, with a shared service that rejects
  file-pinned and environment-overridden keys and notifies subscribers when the
  resolved snapshot changes.
- The AES-256-GCM secret box and the secret key file, read from
  `HEARTHSOUND_SECRET_KEY` or created on first use with 32 random bytes and
  owner-only permissions.
- The `hearthsound-acquire` crate with candidate normalization, the file name
  and parent directory heuristics that recover Soulseek tags, and the weighted
  match score with its duration reject window and its alternate-version
  qualifier penalty.
- Quality tier assignment from a candidate file's format, bitrate, bit depth
  and sample rate, including the V0 boundary for VBR MP3, and the
  `lossless-preferred` and `compact` quality profiles with their preference
  order, minimum tier and cutoff tier.
- ETA-based candidate selection: the download estimate from size, source speed
  and queue length, the interactive and background ordering rules, the attempt
  list cap, and the `no-candidates` and `no-acceptable-quality` outcomes.
- The persistent job queue on the `jobs` table: enqueue, a single-statement
  claim of the most urgent due job that also reclaims jobs whose worker died,
  lock heartbeats, completion, retry with a fixed backoff schedule, a dead
  state that keeps the last error, cancellation, and listing by state.
- The job runner: per-kind async handlers, concurrent workers that wake on a
  local enqueue and poll otherwise, a heartbeat that extends a long handler's
  claim, retries driven by the queue's policy, an immediate failure for a kind
  with no handler, and a shutdown that waits for in-flight handlers.
- The cron scheduler on the `schedules` table: system and per-user schedules
  with a cron expression, a payload, and a persisted next fire time; an
  idempotent upsert keyed by owner and kind; fire times computed in server-local
  time; and a tick loop that enqueues the schedules that are due, advances them
  in the same transaction, and skips the runs missed during downtime.
- User accounts: Argon2id password hashing on the blocking pool, case-insensitive
  unique usernames, display names, the admin flag, permission flags defaulted
  from `users.default_permissions`, lookup by id or username, disabling, and
  listing, plus per-user setting overrides that reject unknown, server-only, or
  badly typed keys.
- API keys: `hs_` plus the lowercase base32 encoding of 32 random bytes, stored
  as their SHA-256 hash with a short prefix, resolved to their owner with
  malformed, unknown, revoked, and disabled-account keys failing alike, and
  revocable per key.
- App passwords: 20 characters drawn from a 32-symbol typeable alphabet, sealed
  at rest with the AES-256-GCM secret box, listed and revoked per app password,
  and opened for Subsonic credential verification.
- Subsonic credential resolution: API keys, plain and `enc:` hex app passwords,
  and `u`/`t`/`s` token authentication resolved into a principal that names the
  user, the permissions, and the credential used, with the account password
  accepted only when `subsonic.allow_account_password` enables it, the typed
  Subsonic error codes 10, 40, 41, 42, 43, and 44, the app-password help URL,
  and a login rate limit keyed by address and username.
- Generated audio fixtures: the test-support crate synthesizes FLAC, MP3 (CBR and
  VBR), Ogg Vorbis, Opus, M4A AAC, WAV, and WavPack files of a chosen duration,
  sample rate, bit depth, and channel count with ffmpeg, tags them with lofty
  including multi-valued artists and an embedded generated cover, and exposes
  them through a builder that writes into a directory or a temporary directory.
- The `hearthsound-media` crate with file probing: container, codec, losslessly
  encoded or not, bitrate, sample rate, bit depth, channel count, duration, the
  full tag set, and whether the file carries embedded art, plus embedded picture
  extraction and a full-decode verification that uses symphonia and falls back
  to an ffmpeg decode for the codecs symphonia lacks.
- Writing the import tag set into audio files: title, artists, album, album
  artist, track and disc numbers with totals, date, genres, ISRC, the Picard
  MusicBrainz identifiers, the ReplayGain values, and the
  `HEARTHSOUND_ACQUIRED_VIA` marker, each into the key its container uses
  (Vorbis comments, ID3v2 `TXXX`, MP4 freeform atoms, or APE items), with
  optional cover embedding and a sidecar cover writer.
- An ffmpeg process runner: one shared concurrency budget that can be resized
  when `stream.max_concurrent_transcodes` changes, argument vectors passed
  straight to `tokio::process` without a shell, standard error forwarded to the
  debug log, and children killed when their handle is dropped. Verification
  decodes now run through the same budget.
