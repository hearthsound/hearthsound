//! The import path template.
//!
//! An imported file's destination is built from `library.naming_template`. The
//! template is a slash-separated path whose components mix literal text with
//! tokens:
//!
//! - `{album_artist}`, `{album}`, `{year}`, `{disc_prefix}`, `{track}`,
//!   `{title}`, and `{ext}` are the tokens; a token with no value renders as the
//!   empty string;
//! - `{track:02}` zero-pads the track number to the given width, so track 1
//!   renders as `01`;
//! - `{disc_prefix}` renders as `<disc>-` on a multi-disc release and as nothing
//!   on a single-disc one;
//! - `[ ... ]` marks an optional group. The whole group, including the literal
//!   text inside it, is dropped when any token inside it renders empty, which is
//!   what turns `{album}[ ({year})]` into `Album` rather than `Album ()` for a
//!   release with no known year.
//!
//! Every rendered component is sanitized for a filesystem: `/ \ : * ? " < > |`
//! and control characters become `_`, trailing dots and spaces are trimmed, the
//! component is capped at 200 bytes without splitting a UTF-8 sequence, and a
//! Windows reserved name is prefixed with `_`. A component that renders empty is
//! dropped, and a component that would be `..` is rejected, so a rendered path
//! can never escape the root it is joined to.
//!
//! # Examples
//!
//! ```
//! use hearthsound_library::naming::{NamingContext, NamingTemplate};
//!
//! let template = NamingTemplate::parse(
//!     "{album_artist}/{album}[ ({year})]/{disc_prefix}{track:02} - {title}.{ext}",
//! )
//! .unwrap();
//! let context = NamingContext {
//!     album_artist: Some("Energy 52"),
//!     album: Some("Cafe del Mar"),
//!     year: None,
//!     disc_number: None,
//!     disc_total: None,
//!     track_number: Some(1),
//!     title: Some("Cafe del Mar"),
//!     extension: Some("flac"),
//! };
//! assert_eq!(
//!     template.render(&context).unwrap(),
//!     std::path::Path::new("Energy 52/Cafe del Mar/01 - Cafe del Mar.flac")
//! );
//! ```

use std::path::{Path, PathBuf};

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;

/// The longest a rendered path component may be, in bytes.
const MAX_COMPONENT_BYTES: usize = 200;

/// The characters that are replaced with `_` in a rendered component.
const RESERVED_CHARACTERS: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// The Windows device names that are prefixed with `_`.
const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The values a template renders from.
///
/// Every field is optional because a track may lack any of them; a missing value
/// renders as the empty string, which is what an optional group tests for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NamingContext<'a> {
    /// The album artist.
    pub album_artist: Option<&'a str>,
    /// The album title.
    pub album: Option<&'a str>,
    /// The release year.
    pub year: Option<i64>,
    /// The disc number.
    pub disc_number: Option<u32>,
    /// The number of discs on the release.
    pub disc_total: Option<u32>,
    /// The track number within its disc.
    pub track_number: Option<u32>,
    /// The track title.
    pub title: Option<&'a str>,
    /// The file extension, without the leading dot.
    pub extension: Option<&'a str>,
}

/// An error from parsing or rendering a naming template.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NamingError {
    /// A token name is not one of the known tokens.
    #[error("unknown template token `{{{name}}}`")]
    UnknownToken {
        /// The unrecognized token name.
        name: String,
    },
    /// A `{` was never closed.
    #[error("template token starting at `{0}` is not closed")]
    UnterminatedToken(String),
    /// A `[` was never closed.
    #[error("template group is not closed")]
    UnterminatedGroup,
    /// A `]` appeared without a matching `[`.
    #[error("template group close `]` has no matching `[`")]
    UnexpectedGroupClose,
    /// A `:NN` suffix was not a number in `1..=9`.
    #[error("`{value}` is not a padding width between 1 and 9")]
    InvalidPadding {
        /// The rejected suffix.
        value: String,
    },
    /// A `:NN` suffix was applied to a token that does not take one.
    #[error("token `{{{token}}}` does not take a padding width")]
    PaddingNotAllowed {
        /// The token the padding was applied to.
        token: String,
    },
    /// The rendered path is empty.
    #[error("the template rendered an empty path")]
    EmptyPath,
    /// A rendered component would leave the root it is joined to.
    #[error("the template rendered a path component `..` that would escape the root")]
    EscapesRoot,
}

/// One path component of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Component {
    /// The component's elements, in order.
    elements: Vec<Element>,
}

/// One element of a path component.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Element {
    /// Literal text.
    Literal(String),
    /// A token.
    Token(Token),
    /// An optional group, dropped when any token inside it renders empty.
    Optional(Vec<Element>),
}

/// A template token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    /// `{album_artist}`.
    AlbumArtist,
    /// `{album}`.
    Album,
    /// `{year}`.
    Year,
    /// `{disc_prefix}`.
    DiscPrefix,
    /// `{track}` or `{track:NN}`.
    Track(Option<usize>),
    /// `{title}`.
    Title,
    /// `{ext}`.
    Extension,
}

impl Token {
    /// Renders the token's value.
    ///
    /// # Parameters
    ///
    /// - `context`: the values to render from.
    ///
    /// # Returns
    ///
    /// The value, or an empty string when the context has none.
    fn render(self, context: &NamingContext<'_>) -> String {
        match self {
            Token::AlbumArtist => context.album_artist.unwrap_or_default().to_owned(),
            Token::Album => context.album.unwrap_or_default().to_owned(),
            Token::Year => context
                .year
                .map(|year| year.to_string())
                .unwrap_or_default(),
            Token::DiscPrefix => {
                let multi_disc =
                    context.disc_total.unwrap_or(1) > 1 || context.disc_number.unwrap_or(1) > 1;
                match (multi_disc, context.disc_number) {
                    (true, Some(disc)) => format!("{disc}-"),
                    _ => String::new(),
                }
            }
            Token::Track(width) => match context.track_number {
                Some(number) => match width {
                    Some(width) => format!("{number:0width$}"),
                    None => number.to_string(),
                },
                None => String::new(),
            },
            Token::Title => context.title.unwrap_or_default().to_owned(),
            Token::Extension => context.extension.unwrap_or_default().to_owned(),
        }
    }
}

/// A parsed import path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamingTemplate {
    /// The template's path components, in order.
    components: Vec<Component>,
}

impl NamingTemplate {
    /// Parses a template.
    ///
    /// # Parameters
    ///
    /// - `template`: the template text, for example
    ///   `{album_artist}/{album}[ ({year})]/{disc_prefix}{track:02} - {title}.{ext}`.
    ///
    /// # Returns
    ///
    /// The parsed template.
    ///
    /// # Errors
    ///
    /// Returns [`NamingError::UnknownToken`] for a token outside the known set,
    /// [`NamingError::UnterminatedToken`] and [`NamingError::UnterminatedGroup`]
    /// for a `{` or `[` that is never closed,
    /// [`NamingError::UnexpectedGroupClose`] for a `]` without a `[`, and the
    /// padding errors when a `:NN` suffix is malformed or misplaced.
    pub fn parse(template: &str) -> Result<NamingTemplate, NamingError> {
        let chars: Vec<char> = template.chars().collect();
        let mut position = 0;
        let mut components = Vec::new();
        loop {
            let mut elements = Vec::new();
            parse_elements(&chars, &mut position, &mut elements, false)?;
            components.push(Component { elements });
            if position >= chars.len() {
                break;
            }
            // The only character that stops a top-level component is `/`.
            position += 1;
        }
        Ok(NamingTemplate { components })
    }

    /// Parses the template configured under `library.naming_template`.
    ///
    /// # Parameters
    ///
    /// - `settings`: the resolved settings snapshot.
    ///
    /// # Returns
    ///
    /// The parsed template.
    ///
    /// # Errors
    ///
    /// Returns [`hearthsound_config::ConfigError`] when the setting is missing or
    /// unreadable, and the parse errors of [`NamingTemplate::parse`].
    pub fn from_settings(settings: &SettingsStore) -> Result<NamingTemplate, NamingTemplateError> {
        let template = settings.get(keys::LIBRARY_NAMING_TEMPLATE)?;
        Ok(NamingTemplate::parse(&template)?)
    }

    /// Renders the relative path.
    ///
    /// Components that render empty are dropped, and the result is relative:
    /// join it under `<storage>/library` or `<storage>/cache` to get the
    /// destination. A component that renders as `..` is rejected before it is
    /// sanitized, so no template can walk out of the root it is joined to.
    ///
    /// # Parameters
    ///
    /// - `context`: the values to render from.
    ///
    /// # Returns
    ///
    /// The rendered path, relative to the destination root.
    ///
    /// # Errors
    ///
    /// Returns [`NamingError::EmptyPath`] when every component rendered empty,
    /// and [`NamingError::EscapesRoot`] when a component rendered as `..`.
    pub fn render(&self, context: &NamingContext<'_>) -> Result<PathBuf, NamingError> {
        let mut path = PathBuf::new();
        for component in &self.components {
            let rendered = render_elements(&component.elements, context);
            // The traversal check runs on the rendered text, before sanitizing:
            // trailing dots are trimmed, so `..` would otherwise collapse to the
            // empty string and be dropped rather than rejected.
            if rendered.trim() == ".." {
                return Err(NamingError::EscapesRoot);
            }
            let sanitized = sanitize_component(&rendered);
            if sanitized.is_empty() || sanitized == "." {
                continue;
            }
            if sanitized == ".." {
                return Err(NamingError::EscapesRoot);
            }
            path.push(sanitized);
        }
        if path.as_os_str().is_empty() {
            return Err(NamingError::EmptyPath);
        }
        Ok(path)
    }

    /// Renders the absolute destination path under a root.
    ///
    /// # Parameters
    ///
    /// - `root`: the destination root, `<storage>/library` for a pinned track
    ///   and `<storage>/cache` for an unpinned one.
    /// - `context`: the values to render from.
    ///
    /// # Returns
    ///
    /// The root joined with the rendered relative path.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`NamingTemplate::render`].
    pub fn render_under(
        &self,
        root: &Path,
        context: &NamingContext<'_>,
    ) -> Result<PathBuf, NamingError> {
        Ok(root.join(self.render(context)?))
    }
}

/// An error from reading or parsing the configured naming template.
#[derive(Debug, thiserror::Error)]
pub enum NamingTemplateError {
    /// The configured setting could not be read.
    #[error(transparent)]
    Setting(#[from] hearthsound_config::ConfigError),
    /// The configured template could not be parsed.
    #[error(transparent)]
    Template(#[from] NamingError),
}

/// Parses the elements of one path component.
///
/// # Parameters
///
/// - `chars`: the whole template.
/// - `position`: the cursor, advanced past what is parsed.
/// - `out`: the elements parsed so far, appended to.
/// - `in_group`: whether the parser is inside an optional group, which is what
///   decides whether `]` and `/` stop the component.
///
/// # Errors
///
/// Returns the parse errors of [`NamingTemplate::parse`].
fn parse_elements(
    chars: &[char],
    position: &mut usize,
    out: &mut Vec<Element>,
    in_group: bool,
) -> Result<(), NamingError> {
    let mut literal = String::new();
    while let Some(character) = chars.get(*position).copied() {
        match character {
            '/' if !in_group => break,
            ']' if in_group => break,
            ']' => return Err(NamingError::UnexpectedGroupClose),
            '[' => {
                flush_literal(&mut literal, out);
                *position += 1;
                let mut inner = Vec::new();
                parse_elements(chars, position, &mut inner, true)?;
                if chars.get(*position) != Some(&']') {
                    return Err(NamingError::UnterminatedGroup);
                }
                *position += 1;
                out.push(Element::Optional(inner));
            }
            '{' => {
                flush_literal(&mut literal, out);
                let start = *position;
                *position += 1;
                let mut name = String::new();
                while let Some(character) = chars.get(*position).copied() {
                    if character == '}' {
                        break;
                    }
                    name.push(character);
                    *position += 1;
                }
                if chars.get(*position) != Some(&'}') {
                    return Err(NamingError::UnterminatedToken(
                        chars[start..].iter().collect(),
                    ));
                }
                *position += 1;
                out.push(Element::Token(parse_token(&name)?));
            }
            _ => {
                literal.push(character);
                *position += 1;
            }
        }
    }
    flush_literal(&mut literal, out);
    Ok(())
}

/// Pushes accumulated literal text as an element, when it is not empty.
///
/// # Parameters
///
/// - `literal`: the accumulated text, cleared when it is pushed.
/// - `out`: the elements to append to.
fn flush_literal(literal: &mut String, out: &mut Vec<Element>) {
    if !literal.is_empty() {
        out.push(Element::Literal(std::mem::take(literal)));
    }
}

/// Parses the text between `{` and `}`.
///
/// # Parameters
///
/// - `name`: the token text, optionally with a `:NN` suffix.
///
/// # Returns
///
/// The token.
///
/// # Errors
///
/// Returns [`NamingError::UnknownToken`] for an unknown name and the padding
/// errors for a malformed or misplaced `:NN`.
fn parse_token(name: &str) -> Result<Token, NamingError> {
    let (name, padding) = match name.split_once(':') {
        Some((name, padding)) => (name, Some(padding)),
        None => (name, None),
    };
    let token = match name {
        "album_artist" => Token::AlbumArtist,
        "album" => Token::Album,
        "year" => Token::Year,
        "disc_prefix" => Token::DiscPrefix,
        "title" => Token::Title,
        "ext" => Token::Extension,
        "track" => {
            let width = match padding {
                Some(padding) => Some(parse_padding(padding)?),
                None => None,
            };
            return Ok(Token::Track(width));
        }
        other => {
            return Err(NamingError::UnknownToken {
                name: other.to_owned(),
            });
        }
    };
    if padding.is_some() {
        return Err(NamingError::PaddingNotAllowed {
            token: name.to_owned(),
        });
    }
    Ok(token)
}

/// Parses a `:NN` padding width.
///
/// # Parameters
///
/// - `padding`: the digits after the colon.
///
/// # Returns
///
/// The width.
///
/// # Errors
///
/// Returns [`NamingError::InvalidPadding`] when the text is not a decimal number
/// in `1..=9`.
fn parse_padding(padding: &str) -> Result<usize, NamingError> {
    match padding.parse::<usize>() {
        Ok(width) if (1..=9).contains(&width) => Ok(width),
        _ => Err(NamingError::InvalidPadding {
            value: padding.to_owned(),
        }),
    }
}

/// Renders a list of elements.
///
/// # Parameters
///
/// - `elements`: the elements to render.
/// - `context`: the values to render from.
///
/// # Returns
///
/// The rendered text, with optional groups dropped when any token inside them
/// renders empty.
fn render_elements(elements: &[Element], context: &NamingContext<'_>) -> String {
    let mut out = String::new();
    for element in elements {
        match element {
            Element::Literal(text) => out.push_str(text),
            Element::Token(token) => out.push_str(&token.render(context)),
            Element::Optional(inner) => {
                if !any_token_empty(inner, context) {
                    out.push_str(&render_elements(inner, context));
                }
            }
        }
    }
    out
}

/// Reports whether any token inside a group renders empty.
///
/// # Parameters
///
/// - `elements`: the elements of the group.
/// - `context`: the values to render from.
///
/// # Returns
///
/// `true` when at least one token, at any nesting depth, renders as the empty
/// string.
fn any_token_empty(elements: &[Element], context: &NamingContext<'_>) -> bool {
    elements.iter().any(|element| match element {
        Element::Literal(_) => false,
        Element::Token(token) => token.render(context).is_empty(),
        Element::Optional(inner) => any_token_empty(inner, context),
    })
}

/// Sanitizes a rendered path component for the filesystem.
///
/// The reserved characters and control characters become `_`, trailing dots and
/// spaces are trimmed, a Windows reserved device name is prefixed with `_`, and
/// the component is capped at [`MAX_COMPONENT_BYTES`] bytes without splitting a
/// UTF-8 sequence.
///
/// # Parameters
///
/// - `component`: the rendered component.
///
/// # Returns
///
/// The sanitized component.
pub fn sanitize_component(component: &str) -> String {
    let replaced: String = component
        .chars()
        .map(|character| {
            if RESERVED_CHARACTERS.contains(&character) || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect();
    let trimmed = replaced.trim_end_matches(['.', ' ']);
    let mut sanitized = if is_windows_reserved(trimmed) {
        format!("_{trimmed}")
    } else {
        trimmed.to_owned()
    };
    if sanitized.len() > MAX_COMPONENT_BYTES {
        sanitized = truncate_bytes(&sanitized, MAX_COMPONENT_BYTES)
            .trim_end_matches(['.', ' '])
            .to_owned();
    }
    sanitized
}

/// Reports whether a component names a Windows device.
///
/// # Parameters
///
/// - `component`: the sanitized component.
///
/// # Returns
///
/// `true` when the part before the first dot is a reserved device name,
/// compared case-insensitively.
fn is_windows_reserved(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    WINDOWS_RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(stem))
}

/// Truncates text to at most `max` bytes without splitting a UTF-8 sequence.
///
/// # Parameters
///
/// - `text`: the text to truncate.
/// - `max`: the byte budget.
///
/// # Returns
///
/// The longest prefix of `text` that is at most `max` bytes long and ends on a
/// character boundary.
fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
