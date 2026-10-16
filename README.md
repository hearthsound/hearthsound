# Hearthsound

Hearthsound is a self-hostable music server written in Rust. It serves your own
music library and is designed to play any song on demand: when you search for a
song that is not on the server yet, Hearthsound fetches it through a plugin you
choose, then plays it and keeps it. Search, metadata, artwork, downloading,
scrobbling and other integrations are all plugins, so you decide which outside
services your server talks to. Hearthsound speaks the Subsonic and OpenSubsonic
APIs, so existing Subsonic clients can connect to it, and a first-party web
client is planned.

## Status

Hearthsound is in early development. Nothing has been released, and features,
commands, configuration and APIs are incomplete and will change without notice.
It is not ready for everyday use.

## Disclaimer

Hearthsound does not include, host or distribute any music. Plugins connect it
to services and tools that you install and configure yourself, and you are
responsible for complying with the laws of your jurisdiction and the terms of
the services you use. First-party plugins never circumvent DRM.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

Please report vulnerabilities privately as described in
[SECURITY.md](SECURITY.md).

## License

Hearthsound is licensed under the GNU Affero General Public License v3.0 or
later. See [LICENSE](LICENSE).
