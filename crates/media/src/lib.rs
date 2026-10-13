//! Media handling: probing, tagging, and ffmpeg process management.
//!
//! The crate owns everything that touches audio bytes or audio metadata on the
//! way in and out of the server: [`probe`] reads a file's container, codec,
//! technical properties, and tags; [`verify_decodes`] proves that a file
//! decodes from end to end; [`extract_picture`] pulls embedded art out;
//! [`write_tags`] writes the server's tag set back into a file, and
//! [`write_sidecar_cover`] writes cover art beside it.
//!
//! The crate takes plain values - paths, caps, bitrates, durations - and never
//! reads a setting itself; the composition root resolves settings and passes
//! the values in.
//!
//! Blocking work (`lofty`, `symphonia`) runs on blocking threads, so every
//! public function here is asynchronous.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), hearthsound_media::MediaError> {
//! use std::path::Path;
//!
//! use hearthsound_media::FfmpegRunner;
//!
//! let path = Path::new("/music/track.flac");
//! let runner = FfmpegRunner::new(None, 8);
//! let probed = hearthsound_media::probe(path).await?;
//! let verified = hearthsound_media::verify_decodes(path, &runner).await?;
//! println!("{} {} {:?}", probed.container.as_str(), probed.codec.as_str(), verified.duration_ms);
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

mod error;
pub mod ffmpeg;
pub mod probe;
pub mod tags;

pub use error::MediaError;
pub use ffmpeg::{FfmpegProcess, FfmpegRunner};
pub use probe::{
    Codec, Container, DecodeBackend, DecodeReport, ProbeResult, extract_picture, probe,
    verify_decodes,
};
pub use tags::{ACQUIRED_VIA_KEY, Artwork, TrackTags, write_sidecar_cover, write_tags};
