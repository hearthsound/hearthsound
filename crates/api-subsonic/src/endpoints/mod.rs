//! The endpoints the `/rest` surface serves.
//!
//! A handler is an `async fn` whose last argument is [`Call`] (or
//! [`BinaryCall`](crate::BinaryCall) for an endpoint that answers with media
//! bytes) and whose return type is
//! [`Encoded`](crate::Encoded). It builds a [`Response`](crate::Response) from
//! the call's parameters and the shared state, and renders it in the encoding
//! the client asked for.
//!
//! Register a handler in [`crate::routes`], which serves it at both
//! `/rest/<name>` and `/rest/<name>.view`.

mod browse;
mod scrobble;
mod search;
mod system;

pub use browse::{get_album, get_album_list_2, get_artist, get_artists, get_song};
pub use scrobble::scrobble;
pub use search::search3;
pub use system::{get_license, get_music_folders, get_open_subsonic_extensions, ping, token_info};
