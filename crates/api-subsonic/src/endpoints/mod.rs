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

pub mod bookmarks;
pub mod browse;
mod cover_art;
pub mod feedback;
pub mod genres;
pub mod now_playing;
pub mod play_queue;
pub mod playlists;
mod scrobble;
mod search;
mod stream;
mod system;

pub use bookmarks::{create_bookmark, delete_bookmark, get_bookmarks};
pub use browse::{
    get_album, get_album_list, get_album_list_2, get_artist, get_artists, get_indexes,
    get_music_directory, get_song,
};
pub use cover_art::get_cover_art;
pub use feedback::{get_starred, get_starred2, set_rating, star, unstar};
pub use genres::{get_genres, get_random_songs, get_songs_by_genre};
pub use now_playing::get_now_playing;
pub use play_queue::{
    get_play_queue, get_play_queue_by_index, save_play_queue, save_play_queue_by_index,
};
pub use playlists::{
    create_playlist, delete_playlist, get_playlist, get_playlists, update_playlist,
};
pub use scrobble::scrobble;
pub use search::{search2, search3};
pub use stream::stream;
pub use system::{get_license, get_music_folders, get_open_subsonic_extensions, ping, token_info};
