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
- Streaming transcodes: mp3 through `libmp3lame`, Opus in Ogg through
  `libopus`, and AAC as ADTS, with a `maxBitRate` cap, a start offset, packet
  flushing so the first bytes arrive immediately, an encoded-length estimate for
  `estimateContentLength`, a bounded queue so a stalled client cannot grow a
  buffer, and a failure reported on the stream rather than as a truncated body.
- The pending PCM bus: one encoder that reads s16le 44100 Hz stereo PCM and
  produces the requested format, fed by a two second silence burst, then silence
  paced at the bus rate, then a decoder that pipes the finished file into the
  same encoder with no gap. The stream ends when the song ends, one second after
  a reported failure, or when the wait elapses; it reports whether any file
  audio was delivered, and dropping it kills both processes and releases their
  permits.
- The `host-http` import: outbound requests through an instance's approved host
  allowlist with literal, wildcard and `$config:` entries, a public-address
  policy enforced on the addresses the connection uses, request timeouts
  clamped to the call deadline, re-checked redirects, a streaming response body
  cap, a default User-Agent, and a per-instance rate limiter.
- The `host-kv` import backed by the `plugin_kv` table: per-instance reads and
  writes with a 256 KiB per-value cap, an approved total quota that fails a
  write instead of evicting, read-time TTL expiry, and a sweep the server
  schedules to reclaim expired rows.
- The `host-files` import for acquisition plugins: reads and directory listings
  under the import roots an instance grants plus the call's staging root, with
  absolute paths, `..` components and symlinks escaping a root refused by
  canonicalizing the path and checking containment.
- Fixture plugins for the metadata and acquisition provider worlds, and one
  whose catalog search performs an outbound `host-http` request, all built from
  plugin SDK 0.1.1 by `cargo xtask fixtures`; the metadata fixture resolves
  recordings from a configured table and the acquisition fixture simulates a
  downloader through `host-kv` and `host-files`, with scenarios for empty
  searches, download failures, stalled progress and unknown tokens.
- Metadata and acquisition plugins callable through the `MetadataProvider` and
  `AcquisitionProvider` traits, with host bindings for every provider world
  sharing one linker and one instance state, one store pool generic over the
  world it drives, and each acquisition call carrying its job's staging root.
- The `hearthsound-activity` crate: play recording in the `plays` table and a
  scrobble entry point that discards a now-playing report and records a
  submission unless the latest stream session for that user and track delivered
  no song audio. A bounded in-memory stream-session registry keeps the latest
  session per user and track, taking its answer either from a flag a raw file
  stream sets as it writes bytes or from a probe such as the pending PCM bus's
  audio-delivered handle.
- `bundled-plugins.lock`, pinning the released first-party plugin archives, and
  the `xtask fetch-plugins` command that downloads each pinned archive and
  verifies its SHA-256 before writing it.
- Multi-stage `Dockerfile` for the server image: the binary built with the
  pinned toolchain against the offline query data, then a `debian:trixie-slim`
  runtime image carrying ffmpeg, ca-certificates, curl, the fetched plugin
  archives, and a non-root `hearthsound` user with `/data` and `/storage`.
- Development compose stack under `dev/`: the server image built from this
  repository plus slskd 0.26.1, a config file with the container bootstrap
  paths, and the gitignored runtime directories the stack mounts.
- The `hearthsound-api-subsonic` crate with the Subsonic response envelope: one
  ordered value model rendered to both JSON and XML, the fixed envelope fields,
  the error member with its optional `helpUrl`, and format selection with XML
  as the protocol default.
- Authentication for the `/rest` surface: query string and form post
  parameters, credentials resolved into a principal with the errors the base
  protocol defines, a trusted-proxy list deciding which client address a
  forwarding header is believed from, and two extractors so a binary endpoint
  answers a failed check with an HTTP status code instead of an error envelope.
  Endpoints are registered at both `/rest/<name>` and `/rest/<name>.view`, and
  an endpoint this server does not implement answers error 70.
- The first Subsonic endpoints: `ping`, `getLicense`,
  `getOpenSubsonicExtensions` with the `apiKeyAuthentication` and `formPost`
  extensions, `tokenInfo`, and `getMusicFolders` with the personal library and
  the server browse folder, each snapshot tested in both encodings.
- The plugin, provider instance and layer assignment repositories, and the
  provider registry built from them: each layer holds the enabled instances in
  the order the administrator assigned, exposes the plugin id and version, the
  manifest, the import roots and the matching provider trait object, and reuses
  one compiled component per plugin version across rebuilds.
- Configuring a provider instance: the slug is validated and must be free, the
  plugin must be enabled, the server-scoped configuration is validated against
  the plugin's JSON Schema with its defaults applied and its `writeOnly` fields
  sealed with the secret box, the import roots must be existing directories, and
  the plugin's `lifecycle.validate-config` decides whether the configuration is
  stored at all; installing a plugin records its manifest, hash and permissions
  as approved and returns the requested permissions with their risk labels.
- Instances are disabled in the database and reported through a `plugin.status`
  event when a plugin traps five times in ten minutes, and every plugin host
  publishes on one `EventBus` a composition root can replace.
- Catalog artwork fetched on behalf of an instance: the host performs the
  request through the same client, and therefore the same allowlist, address
  policy, redirect re-checks, timeout and body cap, that `host-http` enforces
  for the plugin, and returns the status, content type and bytes together with
  the instance's `catalog.imageCachePolicy` so the caller decides whether to
  store them.
- The `hearthsound-library` crate with the repository for the tables it owns:
  artists deduped by MusicBrainz id and otherwise by case-insensitive name,
  albums deduped by MusicBrainz release id and otherwise by folded title with
  their primary artist, ordered album and track credits, case-folded genres,
  tracks with their credits and genres, media files with the one preferred file
  per track chosen by storage class (permanent managed, then folder, then
  evictable cache), and library folders added by their canonical path with an
  optional owner. Every write takes a connection or transaction so a caller can
  keep several writes atomic, and the pool-level facade offers the same
  operations one short transaction each.
- The library scanner: a walk that recognises audio extensions and never
  follows a symbolic link, probes each file with the media crate outside any
  write transaction, and indexes its tags into artists, albums, tracks, and
  folder media files in short write transactions. A rescan skips files whose
  size and modification time match, refreshes a file whose probe columns
  changed, removes the rows of files that vanished (deleting a locally created
  track that lost its last file, demoting a catalog track to remote), records
  the scan time, pins the tracks of an owned folder with the `folder` reason in
  the same transaction, and publishes `scan.progress` and `library.changed`
  events. A `library.scan` job handler runs a folder or the whole scan set, and
  a cover art lookup reads a track's preferred file for an embedded picture or
  a sidecar image beside it.
- The import path template: parsing and rendering `library.naming_template` with
  the `album_artist`, `album`, `year`, `disc_prefix`, `track`, `title`, and
  `ext` tokens, `:NN` zero padding, `[...]` optional groups dropped when any
  token inside them is empty, and a `disc_prefix` that only appears on a
  multi-disc release. Every rendered component is sanitized for the filesystem
  (reserved and control characters become `_`, trailing dots and spaces are
  trimmed, the component is capped at 200 bytes without splitting a UTF-8
  sequence, Windows device names are prefixed with `_`), empty components are
  dropped, and a component that would be `..` is rejected.
- Track retention kept in step with pin changes: adding and removing a pin
  reason on a track inside the caller's transaction, maintaining `pinned`, the
  `pin_reasons` JSON array and `updated_at`, and moving the availability of a
  managed file between `local` and `cached` while a folder or managed permanent
  file stays `local` and an acquiring track is left alone. Changing a library
  folder's owner re-pins or unpins every track of the folder in one transaction,
  and a helper answers whether a track is pinned so an import lands under
  `<storage>/library` when it is and under `<storage>/cache` when it is not.
- The browse and search queries over the personal and server scopes: the
  personal library as the union of saved and uploaded tracks, likes, permanent
  playlists, and folder-owned content, with albums and artists following by
  containment; the server scope as everything stored, permanent content only,
  or nothing per `privacy.server_browse`. The queries return an artist index
  with album counts, artist and album details, album lists ordered newest,
  alphabetically, or at random with a size and offset, a local full-text search
  over the shared index with a sanitized prefix query per entity kind, a track
  view ready for the Subsonic projection (album, artists, genres, and the
  preferred file's suffix, content type, size, bitrate, duration, and relative
  path), the preferred file's absolute path, and a last-accessed touch.
- The browse endpoints of the Subsonic surface: `getArtists`, an artist index
  grouped by the first letter of the article-stripped sort name and reporting
  the `The El La Los Las Le Les` ignored articles; `getArtist`, one artist with
  its albums; `getAlbum`, one album with its songs; `getSong`, one song; and
  `getAlbumList2`, the albums in scope ordered `newest`, `alphabeticalByName`,
  or `random` with a size capped at 500 and an offset. An explicit
  `musicFolderId` selects the personal library (`1`) or the server-wide view
  (`2`, empty while `privacy.server_browse` is disabled), and a request without
  one follows the user's `subsonic.default_scope` override. The projections
  emit every base protocol field plus the OpenSubsonic additions with an empty
  or default value when the server has none, name the album as `coverArt` when
  the track's file carries embedded art and the track itself otherwise, and a
  malformed, unknown, out-of-scope, or catalog id is answered with error 70.
- The server composition root: a `hearthsound-server` crate that creates the
  data and storage directory trees, opens and migrates the database, loads the
  runtime settings and the secret key, and builds the event bus, the credential
  resolver, the plugin host and provider registry, the ffmpeg runner capped by
  `stream.max_concurrent_transcodes`, the library scanner, the job queue with
  the scan and plugin key-value sweep handlers, the scheduler with an hourly
  sweep schedule, and the activity service.
- The surfaces the composition root mounts itself: `/healthz`, `/readyz`
  reported against the schema version the bundled migrations bring a database
  to, and the JSON status object `GET /` answers while there is no web client,
  with request tracing and a graceful shutdown that stops the listener, the job
  runner, and the scheduler on `SIGINT` or `SIGTERM`.
- The schema version accessor on the database layer, reporting the migration
  version a database file is actually at.
- The external-change worker of the running server: it polls `PRAGMA
  data_version`, and a commit made by another connection reloads the runtime
  settings, rebuilds the provider registry from the plugin tables, and
  reapplies the settings the services cache, so a command-line change such as a
  new layer assignment takes effect within about a second without a restart.
- The `serve`, `migrate`, and `scan` commands of the `hearthsound` binary: a
  global `--config <file>` with the rest of the bootstrap keys from the
  environment, logging at `log.level` in text or JSON per `log.format`, a
  migration report naming the version the database started at, moved to, and,
  when one was needed, the backup it wrote, and a scan that indexes one folder
  (by id or by path) or every folder and prints the counts per folder.
- The `admin` commands: `create-user` with a password prompted twice on the
  terminal or read from standard input and the login name defaulting to `admin`
  for `--admin`, `create-api-key` and `create-app-password` printing the new
  secret exactly once with a warning that it cannot be shown again, and
  `add-folder` with exactly one of `--owner <user>` and `--generic`, applying the
  association through the library layer so the folder's tracks are pinned to its
  owner in the same transaction as the ownership column.
- The `plugins`, `providers`, and `layers` commands: `plugins install <file>
  [--bundled]` installs an archive, records the manifest's requested permissions
  as the approved set, enables the plugin, and prints its id, version, api,
  provided layers, and each permission with its risk label, `plugins list` lists
  what is installed, `providers add --plugin <id> --slug <s> --plugin-config
  <json> [--display-name <n>] [--import-root <id>=<path>:<mode>]` validates,
  seals, and stores an instance (printing the plugin's own problems when it
  rejects the configuration) and `providers list` lists the instances, and
  `layers set <layer> <slug>...` replaces a layer's ordered instances while
  `layers list` shows every layer.
- The `settings` commands: `settings get <key>` and `settings list` print each
  setting's effective value, the layer it came from, its built-in default, and
  whether the config file pins it, `settings set <key> <value>` parses the value
  as the registry declares the key's kind and refuses a key the config file pins
  or an environment variable overrides with the reason it cannot take a stored
  value, and `settings unset <key>` removes the stored value and reports what the
  key falls back to.
- The `hearthsound-catalog` crate with the catalog service: lookup by catalog id
  routed to the instance that issued it, artist albums and top tracks, ISRC
  lookup, and search that asks the primary catalog instance under
  `search.catalog.timeout_ms`, falls back to a later instance only when it errors
  or times out, never merges two instances' results, caps results at
  `search.catalog.max_results`, and returns nothing when
  `search.catalog.enabled` is false. Every result carries its
  `x_<slug>_<kind>_<payload>` id, and provider failures map to typed not-found,
  unavailable, rate-limited (with the retry delay), unauthorized, invalid
  configuration, and internal errors.
- A two-level cache of catalog provider responses: an in-memory tier and the
  `catalog_cache` table, both keyed by provider instance and a canonical request
  key so two instances never share an answer. Search answers are cached for five
  minutes (`SEARCH_CACHE_TTL`) and entity lookups for six hours
  (`ENTITY_CACHE_TTL`), a stale row is ignored rather than served, failures are
  never cached, a row whose stored JSON no longer decodes is a miss, and
  `sweep_expired_cache` deletes rows whose lifetime ran out.
- Deduplication of catalog results against local ones: a catalog track is
  dropped and reported with the stored track's id when it matches a local hit by
  ISRC, when an alias in `external_ids` already resolves it (the provider's
  `p:<slug>` id or a stored `isrc`), or when its normalized title and artist
  match a local track within three seconds of duration
  (`DURATION_TOLERANCE_MS`). Albums and artists are reported when they are
  already materialized but never dropped, and the alias lookups run once per
  instance and entity kind through the `external_ids` table.
- `search3` on the Subsonic surface: the local full-text matches first, then
  the catalog layer's matches appended when `search.catalog.enabled` and the
  local count of a kind is below its requested `artistCount`, `albumCount`, or
  `songCount` (each with its offset, and each search bounded by
  `search.catalog.timeout_ms` and capped by `search.catalog.max_results`).
  Catalog results are deduplicated against the local hits, so a catalog entity
  the server already stores is represented by its stored row. A catalog artist,
  album, or song is projected with its `x_<slug>_<kind>_<payload>` id, the
  pending stream format, the `subsonic.catalog_title_suffix` title, and a
  synthetic `catalog/<slug>/<id>` path; an empty query lists the local library
  instead, and a catalog failure or timeout never fails the search. The `/rest`
  state carries the catalog layer and the activity service, and
  `hearthsound-library` gains a scope-filtered track listing for the empty
  query.
- Browsing a catalog entity by its `x_` id through `getSong`, `getAlbum`, and
  `getArtist`: the entity is answered from the catalog layer without storing
  anything, projected with the same catalog rules as a `search3` result, with an
  album carrying its `x_`-id songs and an artist its `x_`-id albums. An id the
  catalog does not have, or one of another kind, is Subsonic error 70, and an
  instance that cannot answer is a generic error naming it.
- `scrobble` on the Subsonic surface: each `id` is recorded as a play through
  the activity service, honoring its "no song audio delivered" guard, while a
  `submission=false` now-playing report is accepted and discarded. Only a
  stored track is recorded; an `x_` catalog id is accepted and discarded.
