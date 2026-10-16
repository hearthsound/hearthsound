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
  session per user and track, taking its answer either from a flag a served
  stream sets as it writes bytes or from a probe.
- `bundled-plugins.lock`, pinning the released first-party plugin archives, and
  the `xtask fetch-plugins` command that downloads each pinned archive and
  verifies its SHA-256 before writing it.
- Multi-stage `Dockerfile` for the server image: the binary built with the
  pinned toolchain against the offline query data, then a `debian:trixie-slim`
  runtime image carrying ffmpeg, ca-certificates, curl, the fetched plugin
  archives, and a non-root `hearthsound` user with `/data` and `/storage`.
- Development compose stack under `dev/`: the server image built from this
  repository, a config file with the container bootstrap paths, and the
  gitignored runtime directories the stack mounts, including an imports folder
  an acquisition backend run alongside it can download into.
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
- Materialization of catalog entities through the `external_ids` aliases: a
  catalog track, album or artist that no row describes is fetched from its
  issuing instance and stored through the library crate's own writers with
  origin `catalog` and availability `remote`, recording the provider id
  (`p:<slug>`) and every canonical identifier the entity carries (ISRC, UPC,
  MusicBrainz ids) in the same transaction, so the original `x_` id keeps
  resolving and two providers that know one recording agree on one row. A
  recording the server already stores is reused by its ISRC, an album is stored
  with all of its tracks in track order or not at all, and `provider_aliases`
  and `entity_by_alias` read the alias table back, so a caller can reach the
  instance that issued an entity and the row an identifier names. `ArtistInput`
  gained an `origin` so a catalog artist is not written as a local one.
- Identity resolution through the metadata layer: a track's recording is resolved
  by ISRC or by a recording hint, following the layer policy (the first instance
  whose best match scores at least `IDENTITY_MATCH_THRESHOLD`, otherwise the
  best score across instances, never a merge of two instances' answers), cached
  in `catalog_cache` per metadata instance and request for `ENTITY_CACHE_TTL`.
  `resolve_track_identity` applies the answer in one short transaction: the
  track's `mb_recording_id` through the library's `set_track_mb_recording_id`,
  and the `mb-recording`, `mb-release`, `mb-release-group` and `mb-artist`
  aliases of the canonical identifiers it learned.
- Catalog cover art served through the issuing instance: a catalog id, or a
  stored entity whose `p:<slug>` alias names the instance that issued it,
  resolves to the images its provider describes, the smallest one at least as
  large as the requested size is fetched through the instance's HTTP grant
  (`ProviderRegistry::fetch_image`), and an instance whose manifest declares
  `catalog.imageCachePolicy: cache` has the bytes written under `<data>/artwork/`
  and indexed in `artwork_cache` while one that declares `no-store` is proxied
  without writing anything. A row whose file is gone is a miss rather than a
  failure, and the result carries the image's MIME type and bytes.
- `/rest/stream`: a stored track's preferred file is served with the HTTP
  semantics a seeking client needs - `Accept-Ranges`, a single `Range` answered
  with `206` and `Content-Range`, an unsatisfiable range answered with `416`, a
  strong `ETag` and `Last-Modified`, `If-None-Match`, `If-Modified-Since` and
  `If-Range`, `HEAD`, and the container's content type - and never a Subsonic
  envelope. Serving bytes registers a stream session whose audio-delivered flag
  the first body frame sets and records the file's last access on a detached
  write; an id that names no track, a catalog id, or a track with no file yet is
  answered with `404` through the documented pending extension point.
- `/rest/stream` transcodes: `format=mp3`, `format=opus`, and `format=aac`
  encode the stored file through ffmpeg, a `maxBitRate` below the file's own
  bitrate caps the output (mp3 when no format is named), `timeOffset` starts the
  stream that far in, and `estimateContentLength` answers with an estimated
  `Content-Length` instead of a chunked body. A transcode carries no validators
  and no ranges (`Accept-Ranges: none`), an unknown format is a bad request, and
  a request the stored file already satisfies - no format, `format=raw`, a cap
  the file fits, or the file's own format - is served straight from disk.
- The `getCoverArt` endpoint, answering with image bytes instead of an envelope:
  a stored track is served the embedded picture of its preferred file or a
  sidecar image beside it, a stored album and artist the first of their tracks
  that has one, and an entity with no image of its own but a `p:<slug>` alias,
  or a plain `x_` catalog id, the image its issuing instance describes, proxied
  under that instance's HTTP grant and image cache policy, so a `no-store`
  instance's bytes are never written to the artwork cache. Every image carries
  its content type, its length, a strong `ETag` and a `Cache-Control` (`no-store`
  for an instance that forbids caching), an `If-None-Match` naming the tag is
  answered with 304, `size` is handed to the catalog image choice while local
  images are served unscaled, and a refusal is a real HTTP status - 400, 401,
  404, 500, or 503 with `Retry-After` - never a failure envelope.
- `scrobble` resolving a catalog track id through the provider aliases: an `x_`
  id whose entity the catalog layer materialized records the play of the stored
  row it resolves to, while an entity that was never materialized is still
  accepted and discarded.
- Acquisition request intake and the acquisition ledger: `hearthsound-acquire`
  owns the `acquisitions` and `acquisition_log` tables and gains an
  `AcquisitionOrchestrator` that admits one active acquisition per track,
  raises an existing job's priority when a more urgent request joins it,
  refuses a track that is cooling down and a requester without the permission,
  fans an album request out into one request per album track, creates the
  acquisition's staging directory, marks the track `acquiring`, and publishes
  `acquisition.updated` and `track.availability`. A waiter follows a job
  through a `tokio::sync::watch` channel, and the scheduler claims the most
  urgent queued acquisition within `acquisition.max_concurrent`, logging every
  state change in the same transaction as the row it changes.
- The acquisition search stage: the orchestrator builds an `AcquisitionQuery`
  from the library track (title, credits, track and disc numbers, duration,
  ISRC, the MusicBrainz recording id, and the album with its artist and year)
  plus the preferred formats of the quality profile, walks the acquisition
  layer in assignment order, skips instances that cannot search a track, and
  honors each instance's `maxConcurrentSearches` semaphore and its
  `searchesPerHour` token bucket (burst 10), the latter for `queue`,
  `background` and `upgrade` work only. It polls a search every second until it
  completes, the plugin's call deadline passes, or an interactive request
  already has a usable candidate; scores and orders the candidates with
  `acquisition.min_match_score`, `interactive_eta_target_secs`,
  `max_candidates` and `acquisition.quality_profile`; and fails an acquisition
  as `no-candidates`, `no-acceptable-quality` or `backend-unavailable`, marking
  the track `unavailable` for `acquisition.unavailable_cooldown_hours`.
- The acquisition download stage: each candidate is downloaded under its
  instance's `maxConcurrent` slot budget, polled every 2 seconds for `play` and
  `next` and every 10 seconds otherwise, with the instance, candidate, plugin
  token, progress and attempt count persisted in the ledger and
  `acquisition.updated` carrying the percentage and ETA as bytes advance. A
  candidate that makes no progress for `acquisition.stall_timeout_secs`, or
  that stays queued past `acquisition.queue_timeout_secs.interactive` or
  `.background`, is cancelled and the next candidate is tried; a download token
  the backend no longer knows restarts the acquisition's search; a `play`
  request that finds an instance's download slots busy preempts its least
  urgent `background` or `upgrade` download, which resumes afterwards. A
  completed download waits for each reported file's size to hold across two
  checks, resolves the paths through the instance's import roots, moves the
  acquisition to `verifying`, and hands the stable files to the verification and
  import entry point.
- The acquisition verification stage: every file of a completed candidate is
  decoded, its duration must be within five seconds of the wanted track's
  catalog duration when that is known, and a candidate that claimed a lossless
  tier must have produced a file that decodes losslessly. A rejected candidate
  is recorded in the acquisition log, its staged copies are removed, and the
  download stage tries the next one; when no candidate is left the acquisition
  fails as `verification-failed`. The pipeline's `resolving` stage resolves the
  track's recording identity through the catalog's metadata layer - by ISRC when
  the track has one and otherwise by a title, artist, album and duration hint -
  spawning the resolution for `play` requests so it never delays audio and
  awaiting it under the metadata instances' call deadline for every other
  priority, and never failing an acquisition because the resolution failed.
- The acquisition import stage: a verified file is copied into the acquisition's
  staging directory, tagged there with the track's title, artists, album, album
  artist, track and disc numbers and totals, date, genres, ISRC and MusicBrainz
  ids, plus `HEARTHSOUND_ACQUIRED_VIA`, and installed atomically - a rename on
  one filesystem, otherwise a copy, an fsync and a rename - under
  `<storage>/library` for a pinned track or `<storage>/cache` for an unpinned
  one, named by `library.naming_template` with a deterministic ` (2)` collision
  suffix that never overwrites another track's file. A sidecar cover named by
  `library.cover_filename` is written only when the catalog instance that issued
  the track permits its images to be stored, and `library.tags.embed_cover`
  decides whether the picture is also embedded. A `move` import root loses the
  file and its emptied parent directories, `copy` and `link` roots keep the
  original, and a read-only root is never modified. One short transaction writes
  the `media_files` row, ensures the track's `track_retention` row, sets the
  track `local` or `cached`, and completes the acquisition with its
  `media_file_id`; `track.availability`, `acquisition.updated` and
  `library.changed` are published after it commits, and the acquisition's
  progress carries the final managed path only then.
- The pending-stream path of `/rest/stream`: a request for a track the server
  has not stored resolves an `x_` catalog id through the catalog layer, checks
  the requester's acquire permission, and asks the acquisition layer for the
  track at priority `play`. `stream.pending.mode` then decides the answer: `hold`
  (the default) sends nothing until the file is ready, answers `404` when the
  acquisition fails, and answers `503` with `Retry-After` when
  `stream.pending.max_wait_secs` elapses first; `fail` answers `503` with
  `Retry-After` at once while the acquisition continues. A cooling-down or
  denied track is answered `404` or `403` without starting anything.
- The acquisition orchestrator in the composition root: `Services::build` builds
  it over the catalog layer, the provider registry, the ffmpeg runner, the event
  bus, and the staging root, starts its scheduler, and shares it with the
  `/rest` surface, so `/rest/stream` of a track the server has not stored is
  answered by the same pipeline that runs the server's acquisition workers.
- The provider id of a credited artist, read the same way by every layer that
  needs it: `hearthsound_core::providers::credit_provider_id` returns a credit's
  first identifier outside the canonical namespaces, `artistId` of a catalog
  track or album is built from it, and materialization records it as the
  credited artist's `p:<slug>` alias in the same transaction as the rows, so a
  later `x_<slug>_r_<id>` resolves to the stored artist.
- Bundled Deezer catalog plugin 0.1.1, whose artist credits carry the Deezer
  artist ids that catalog artist links resolve through.
- Acquisition cancellation and the end of a failed one: every failure reason
  takes one terminal path that writes the exact `failure_reason` literal with
  the backend's `error_detail`, marks the track `unavailable` until
  `acquisition.unavailable_cooldown_hours` elapses, cancels the download the
  acquisition still has in flight, removes its staging directory, and tells the
  waiters and the event bus, while a `force` request ignores the cooldown and
  starts a fresh search. `cancel` ends an acquisition as `cancelled` the same
  way, returning a track without a file to catalog-only availability and freeing
  the slots it held, and `reconcile` notices a row another process such as the
  CLI wrote - the server calls it after `PRAGMA data_version` changes - by
  waking the worker that is still serving that acquisition and undoing its
  side effects. `list` renders the ledger for an administrative view with the
  track's title and artist, the instance, the percentage and the failure reason,
  filtered by state or by whether the acquisition still holds work.
- Restart safety for the acquisition pipeline: `start` resumes the acquisitions a
  previous process left unfinished instead of stranding them, in the order the
  ledger holds them and within `acquisition.max_concurrent`. A `queued` row is
  claimed by the scheduler like any other, a `resolving` or `searching` row runs
  the search again, a `downloading` row keeps polling the instance, candidate and
  download token the ledger recorded - and restarts the search when the backend
  no longer knows that token - and a `verifying` or `importing` row redoes the
  completion check and then verification and the import from the files that
  download resolved, which are stored in the candidate column for exactly this
  reason. A server that stops after a download or during an import therefore
  finishes the acquisition without downloading the file again.
- The `acquisitions` command line: `acquisitions list [--state <s>] [--all]`
  prints one acquisition per line - its `ac_` client id, state, priority, track
  artist and title, the instance serving it, the download percentage, the failure
  reason and the creation time, with a `-` for a field the ledger does not hold -
  showing only the acquisitions that still hold work unless `--all` is given, and
  `acquisitions cancel <ac_id>` writes the cancellation through the acquisition
  crate's repository. The command opens the database directly rather than talking
  to a server, so the running server's `reconcile` is what stops the worker and
  returns the track to catalog-only availability.
- Materializing a catalog track links the track to its album in the same
  transaction. The album reference a provider returns either resolves to the
  album row an earlier materialization stored, through the album's `p:<slug>`
  alias, or creates the row from the reference with its title, its credits as
  album artists with their provider-id aliases and its year, with origin
  `catalog`, so a track streamed from a search result is stored with an album
  artist, an album title and a release year. A later materialization of the
  album itself reports that row instead of creating a second one. Imported
  tracks therefore carry the album, albumartist and date tags and are named
  `<album artist>/<album> (<year>)/<track> - <title>.<ext>` instead of landing
  beside the storage root with only a title.
- Prefetching the tracks a client is about to play: `PrefetchScheduler` infers
  the implied queue from the listing the client last read - the most recent
  `getAlbum` of the same user and client instance, kept in memory for 30 minutes
  and bounded to one entry per user and client - and when a streamed track is in
  that listing it requests the next `prefetch.lookahead` items, the first at
  priority `next` and the rest at `queue`, with reason `prefetch` and the user as
  the requester, skipping the items that already have a file or are cooling down.
  A later stream that moves the upcoming set cancels the prefetch acquisitions
  that left it, unless their track is pinned or their download is past half done,
  and every start and drop is published as a `queue.prefetch` event.
  `prefetch.enabled` off disables the whole scheduler, and an `x_` catalog item
  is materialized only when it is actually prefetched.
- Listing and stream recording for the prefetch scheduler: `getAlbum` records the
  ordered track ids it answers with as the implied queue of the requesting user
  and `c` client instance - for a stored album as stored track ids and for an
  `x_` catalog album as catalog ids - and `/rest/stream` notes the track it
  served off the request path, so the scheduler can prefetch what the client is
  about to reach. The server builds the scheduler in `Services::build` and hands
  it to the `/rest` state, and the reload path reconciles the acquisition ledger
  after an external write, so a cancellation written by a command line stops the
  worker that is still serving that acquisition. A new `album-catalog` fixture
  plugin answers a five-track album, which is what the full-stack test of the
  prefetch needs.
- Importing a catalog track derives its album totals and its album artist from
  the album the issuing instance lists instead of from the stored rows alone: a
  lone streamed track of a fourteen-track album is tagged `TRACKTOTAL=14` rather
  than `1`, and a track of a multi-disc album is tagged with the track count of
  its own disc, gets the disc prefix its release needs and carries the release's
  disc count. An album whose materialized row names no artist - the album
  reference a provider embeds in a track may omit it, as Deezer's does - is named
  and tagged with the artist the catalog album credits or, when it credits none,
  with the track's own artist, so the file lands under an artist directory with
  an `albumartist` tag instead of directly in the storage root. A catalog album
  that cannot be read leaves the totals empty rather than failing an import whose
  file verified.
- Read-then-write transactions through the command line start as writes
  (`BEGIN IMMEDIATE`), so a commit a running server makes between the read and
  the write cannot fail them with `SQLITE_BUSY`: cancelling, moving, failing and
  completing an acquisition, adding a library folder, and a scan's write batches
  now take the writer lock up front, where the five second busy timeout applies,
  rather than upgrading a read snapshot the server's commit has already
  invalidated. The reload test no longer retries the cancellation it writes
  through a second connection.
- Atomic import destinations, and a terminal failure for an import that fails:
  the import stage reserves the absolute destination path it is about to write
  in a process-wide set that is held until the import's transaction commits or
  it gives up, and it still counts the filesystem's own answer, so two imports
  of different tracks that render the same relative path can no longer both pick
  it and no import writes to the file the other is about to place. Installing a
  file hardlinks the tagged copy into place instead of renaming it over the
  destination, so a name that turns out to be taken is left untouched; a
  destination that a stored `media_files` row already holds is retried under the
  next ` (n)` suffix, and the source file is still removed from a `move` import
  root. An import that fails for any other reason now ends its acquisition
  through the normal failure path - `failed` with `all-candidates-failed` and
  the import error as the detail, and the track in cooldown - so no worker is
  left holding a row in `importing`.
- Full-stack tests of on-demand playback in
  `crates/server/tests/integration/on_demand.rs`,
  which start the real server over a temporary data directory and drive it over
  HTTP with the album catalog and acquisition fixtures: `search3` answers the
  requester's own library first and the catalog after it; a stream of an
  unmaterialized catalog id sends nothing while the acquisition runs and then
  serves the imported file as any stored file is served, which lands in the cache
  under the template's name with the track's own tags and leaves the `move`
  import root empty; a replay of that id serves the stored file and answers a
  `Range` request with `206`; a track no backend can find ends its stream with
  `404` and cools down, and a replay inside the cooldown answers `404` without
  starting a second acquisition; `stream.pending.mode`
  switches between `hold`, the default, and `fail`, which answers `503` with
  `Retry-After`; and a server stopped in the middle of a download resumes it in a
  new process on the same data directory, imports the file and empties the import
  root. The shared fixture grows the helpers a restart needs: the data directory
  it runs over, a stop that hands that directory back, and a reopen that builds a
  new process over it.
- Verification with `ffmpeg` for the files `symphonia` cannot read now fails
  when the decode reports an error, so a truncated WavPack or Opus file is
  rejected instead of accepted.
- Verification now hands a file to `ffmpeg` when the `symphonia` probe latches
  onto a container marker that runs out of stream, so a WavPack file whose audio
  data happens to contain such a marker is no longer rejected.
- The library scan skips hidden files and directories, such as the `._` metadata
  files macOS writes, and reports files it cannot read as audio instead of
  stopping at the first one.
- The Subsonic login rate limit counts only failed logins, so a client that
  authenticates every request with a username and app password is no longer
  lock out after ten requests a minute. A username or address that has spent
  its budget is refused before its secret is checked.
- `server.cors_origins` now takes effect: the listed origins, or every origin
  for `*`, may read any response, which browser-engine clients such as the
  Feishin desktop app need. The development stack allows every origin.
- Settings for how often the acquisition orchestrator polls its backends:
  `acquisition.poll_interval_ms.interactive` (2000) and `.background` (10000)
  for downloads, `acquisition.search_poll_interval_ms` (1000) for searches, and
  `acquisition.completion_check_interval_ms` (1000) for the size checks before
  an import. They were fixed intervals before.
- The tag reader collects every MusicBrainz artist id a file carries and reads
  the compilation flag, so a track credited to several artists keeps each
  artist's id and a compilation can be recognised.
- A scan stores the ReplayGain values a file carries and gives every credited
  artist its own MusicBrainz id, both of which the Subsonic projection reports.
- A single-valued artist tag is split into its credited artists on the
  separators in the new `library.scan.artist_separators` setting, so `AC/DC`
  stays one artist while `A feat. B` becomes two.
- An album whose compilation flag is set, or whose album artist is named
  `Various Artists`, is stored as a compilation, credited to `Various Artists`
  when the file names no album artist, and reported with `isCompilation`.
- A scan reads a folder's stored rows in one query and probes the changed files
  several at a time, with the new `library.scan.concurrency` setting (8 by
  default) bounding how many run at once.
- A scan job carries the directories to scan and `hearthsound scan` gained
  `--full`, which probes every file instead of trusting the stored size and
  modification time; scans of one folder never overlap.
- The scanner reports what it is doing: a queued job counts as scanning from the
  moment it is requested, and the files a running scan processed are counted
  too.
- A library folder whose `watch` flag is set is watched for filesystem changes:
  after the quiet period in `library.watch.debounce_ms` (2 seconds by default)
  the changed directories are scanned, never the whole library.
- The watcher starts with the server, so a watched folder is scanned once at
  startup and a folder added while the server runs is picked up by the reload
  that follows the write.
- `admin add-folder --no-watch` adds a folder without watching it, and
  `admin set-folder-watch` turns watching on or off for a folder named by path
  or id.
- An ignored benchmark scans a generated 20,000-file library and rescans it,
  which is how the scan timings are measured.
- The database gained the per-user activity tables: bookmarks, the current
  playback entry per client, the saved play queue per client, and ratings.
- The media probe searches much further for a file's first frame, so a file
  whose tagger left a second ID3v2 tag in place is read instead of being
  reported as an unknown container.
- Liking a track or an album records the like and pins the tracks it covers in
  the user's personal library, in one transaction; unliking releases a pin the
  user's saved rows no longer hold.
- A client can rate tracks, albums, and artists from 1 to 5, and a rating of 0
  clears the rating.
- Playlists can be created, renamed, made public, added to, edited by entry
  index, replaced, and deleted; every track whose membership changes has its
  playlist pin updated in the same transaction.
- A client can save the play queue it is playing per client, and the most
  recently saved queue is the one the queue endpoints answer.
- A track can be bookmarked at a position with an optional note, and bookmarks
  are listed newest first.
- A `scrobble` with `submission=false` is stored as now playing instead of being
  discarded, with a stale row left out after the track plus a grace period.
- The OpenSubsonic `reportPlayback` reports are applied: they keep now playing
  current, accumulate the listening time each client reports, record the play a
  stopped session earned, and suppress the scrobble that repeats it.
- Listing endpoints can read a user's stars, ratings, play counts, and last-play
  instants for a whole page of tracks, albums, or artists in a bounded number of
  statements.
- Album listings are ordered by an artist-aware name, the user's rating, play
  count or last play, their likes, a release-year range, or a genre, and one
  query serves every order instead of one query text per order.
- The stored genres of a scope can be listed with their song and album counts,
  the songs of one genre can be paged, and a random sample of songs can be
  narrowed by genre and release year.

### Security

- Updated wasmtime to 49.0.3 and rustls to 0.23.46, fixing the plugin runtime
  panics, the WASI sandbox escape and the TLS advisories published in
  RUSTSEC-2026-0346 through RUSTSEC-2026-0364.
