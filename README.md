# Hearthsound

Hearthsound is an open-source, self-hostable Rust music server that combines a
Navidrome/Jellyfin-style local library with on-demand playback of any song: a
searched song that is missing from the server is acquired through a user-chosen
downloader plugin and then played, while upcoming queue items are prefetched in
the background. Every layer of the system (catalog search, metadata, artwork,
acquisition, verification, scrobbling, discovery, lyrics, playlist import) is
served by an admin-selected plugin, so the hoster decides which external services
the server talks to.

Status: early development. The on-demand vertical slice works end to end: a
Subsonic client can search, stream a catalog song the server does not have yet,
and get the acquired file, while the library scanner, the browser UI, and the
remaining API surfaces arrive with later work. The CLI, the runtime settings, and
the plugin host are usable today.

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
- `ffmpeg` and `ffprobe` on `PATH`. Acquired files are verified and transcoded by
  running `ffmpeg`, so a server without it cannot verify a download or serve a
  pending stream.
- `cargo-nextest` and `cargo-deny` for the full check suite (see
  `CONTRIBUTING.md`).
- `rustup target add wasm32-wasip2`, only to build the test fixture plugins with
  `cargo xtask fixtures`. A normal build does not need it.

```sh
cargo build
cargo run -p hearthsound -- --version
```

The bundled first-party plugins are pinned in `bundled-plugins.lock` and fetched
into `target/bundled-plugins/`:

```sh
cargo xtask fetch-plugins
```

That writes one archive per plugin, for example
`target/bundled-plugins/app.hearthsound.slskd-0.1.0.hsplugin`.

## Running a server

Every command reads the same bootstrap configuration: `--config <FILE>` names a
TOML file, and without it every key comes from `HEARTHSOUND__SECTION__KEY`
environment variables or its default. The data directory defaults to
`$XDG_DATA_HOME/hearthsound` (or `~/.local/share/hearthsound`), the database to
`<data>/hearthsound.db`, and the HTTP listener to port `4327`. The commands below
assume no config file; add `--config hearthsound.toml` to each of them to pin
paths, or export `HEARTHSOUND__PATHS__DATA=/srv/hearthsound`.

1. Create the data directory and apply the schema:

   ```sh
   hearthsound migrate
   ```

2. Create the administrator. The password is prompted twice; `--password-stdin`
   reads one line instead:

   ```sh
   hearthsound admin create-user --admin --username admin
   ```

3. Create the credentials a client uses:

   ```sh
   hearthsound admin create-api-key --user admin --name web
   hearthsound admin create-app-password --user admin --name symfonium
   ```

   Both print the secret once. The api key goes into a modern client's
   `apiKey` parameter; the app password goes into a client's username and
   password fields.

4. Add the folder the server scans. `--owner admin` puts its content in the
   administrator's library and pins it; `--generic` leaves it for any user to
   adopt:

   ```sh
   hearthsound admin add-folder --path /srv/music --owner admin
   hearthsound scan
   ```

5. Install the bundled plugins. Each install prints the permissions the plugin
   asked for:

   ```sh
   hearthsound plugins install target/bundled-plugins/app.hearthsound.deezer-0.1.1.hsplugin
   hearthsound plugins install target/bundled-plugins/app.hearthsound.musicbrainz-0.1.0.hsplugin
   hearthsound plugins install target/bundled-plugins/app.hearthsound.slskd-0.1.0.hsplugin
   ```

6. Configure one instance per plugin. `--plugin-config` is the plugin's own
   server-scoped configuration and `--import-root <id>=<path>:<mode>` grants a
   directory it may reach. Deezer and MusicBrainz need no credentials; both
   accept an empty object and default to their public base URL. slskd needs the
   url of the slskd instance, an api key created with the `readwrite` role, and
   the import root its downloads land in:

   ```sh
   hearthsound providers add --plugin app.hearthsound.deezer --slug deezer \
     --plugin-config '{}'

   hearthsound providers add --plugin app.hearthsound.musicbrainz --slug musicbrainz \
     --plugin-config '{}'

   hearthsound providers add --plugin app.hearthsound.slskd --slug slskd \
     --plugin-config '{"url":"http://127.0.0.1:5030","apiKey":"<key>","importRoot":"slskd-downloads"}' \
     --import-root slskd-downloads=/srv/slskd/downloads:move
   ```

   `importRoot` is the id of the granted import root, not a path. The `move`
   mode makes the server delete an imported file from that root once the file is
   stored; `copy` and `link` leave it in place.

7. Assign the instances to the layers they serve. The first slug of a layer is
   its primary:

   ```sh
   hearthsound layers set catalog deezer
   hearthsound layers set metadata musicbrainz
   hearthsound layers set acquisition slskd
   ```

8. Serve:

   ```sh
   hearthsound serve
   ```

`hearthsound settings list` prints every runtime setting with its value and where
that value comes from; `hearthsound settings set <key> <value>` stores one, and
`hearthsound settings unset <key>` falls back to the layers below it.

### Pending playback modes

A stream of a song the server does not have yet answers before the download
finishes. `stream.pending.mode` decides how:

```sh
hearthsound settings set stream.pending.mode pad    # default: answer 200 at once and play silence until the file is ready
hearthsound settings set stream.pending.mode hold   # send nothing until the file is ready, then serve it
hearthsound settings set stream.pending.mode fail   # answer 503 with Retry-After and keep acquiring
```

`pad` is what a player with no buffer of its own needs; `hold` trades the wait
for a clean start of the file; `fail` lets a client show its own retry and try
again later.

## The development stack

`dev/compose.yml` runs the server built from this checkout together with the
slskd acquisition backend:

```sh
docker compose -f dev/compose.yml up --build
```

`dev/.env` (never committed) holds the Soulseek credentials and the slskd api
key:

```sh
SLSKD_SLSK_USERNAME=<soulseek user>
SLSKD_SLSK_PASSWORD=<soulseek password>
SLSKD_API_KEY=role=readwrite;<16 to 255 characters>
```

Sign in at `http://localhost:5030` once to set
`transfers.download.destination.permissions.mode` to a mode with execute bits
(for example `777`) and to share at least one folder: the Soulseek network bans
non-sharers. The plugin instance is configured with the key part after the last
`;`, so the stack's provider command is:

```sh
docker compose -f dev/compose.yml exec hearthsound \
  hearthsound plugins install /usr/share/hearthsound/plugins/app.hearthsound.slskd-0.1.0.hsplugin
docker compose -f dev/compose.yml exec hearthsound \
  hearthsound providers add --plugin app.hearthsound.slskd --slug slskd \
    --plugin-config '{"url":"http://slskd:5030","apiKey":"<key>","importRoot":"slskd-downloads"}' \
    --import-root slskd-downloads=/imports/slskd:move
```

The server listens on `http://localhost:4327`, its data lives in `dev/data`, and
`HEARTHSOUND_MUSIC_DIR` points the read-only music mount at another folder.

## Clients

Any Subsonic client works. Modern clients authenticate with the api key from
`admin create-api-key` as the `apiKey` parameter (or the `X-API-Key` header); no
password is needed. Clients that only support the legacy token scheme
authenticate with the account name and an app password from
`admin create-app-password`, which is a per-device secret that can be revoked
without changing the account password.

## License

AGPL-3.0-or-later. See `LICENSE`.
