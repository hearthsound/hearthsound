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

mod system;

pub use system::{get_license, get_music_folders, get_open_subsonic_extensions, ping, token_info};
