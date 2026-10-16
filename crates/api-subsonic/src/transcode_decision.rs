//! The transcode decision: whether a client can play a file as it is, and if
//! not, what to encode for it.
//!
//! The OpenSubsonic transcoding extension has a client describe itself - the
//! containers and codecs it can play directly, the profiles it accepts a
//! transcode in, and the limitations its codec support has - and asks the
//! server what it would do with one media file. This module holds that
//! decision and nothing else: it reads no file and starts no process, so the
//! rule can be tested on its own and the endpoint only has to gather the
//! inputs.
//!
//! The decision order is the one the extension describes: direct play when a
//! direct-play profile covers the file and every codec limitation for its codec
//! holds, and otherwise the first transcoding profile that maps to a format
//! this server can encode.

use serde::{Deserialize, Serialize};

use hearthsound_media::TranscodeFormat;

/// The reason one direct-play profile did not cover the file.
///
/// A client shows these to explain why it is transcoding instead of playing
/// what it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum TranscodeReason {
    /// The profile does not list the file's container.
    ContainerNotSupported,
    /// The profile does not list the file's codec.
    AudioCodecNotSupported,
    /// The profile's protocol is not one this server serves.
    ProtocolNotSupported,
    /// The file has more channels than the profile allows.
    AudioChannelsNotSupported,
    /// The file's bitrate is above the profile's cap.
    AudioBitrateNotSupported,
    /// A codec-profile limitation the client requires does not hold.
    CodecLimitationNotMet,
}

/// One container and codec set a client plays without transcoding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DirectPlayProfile {
    /// The containers it plays; an empty list plays any container.
    pub containers: Vec<String>,
    /// The audio codecs it plays; an empty list plays any codec.
    pub audio_codecs: Vec<String>,
    /// The protocols it plays over; an empty list accepts any protocol.
    pub protocols: Vec<String>,
    /// The most channels it plays; zero or absent plays any count.
    pub max_audio_channels: u8,
}

/// One format a client accepts a transcode in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TranscodingProfile {
    /// The container it accepts.
    pub container: String,
    /// The audio codec it accepts.
    pub audio_codec: String,
    /// The protocol it accepts; an empty value accepts any protocol.
    pub protocol: String,
    /// The most channels it accepts; zero or absent accepts any count.
    pub max_audio_channels: u8,
}

/// One limitation a client's codec support has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CodecLimitation {
    /// The property the limitation is about, for example `audioBitrate`.
    pub name: String,
    /// How the value is compared: `Equals`, `NotEquals`, `LessThanEqual`, or
    /// `GreaterThanEqual`.
    pub comparison: String,
    /// The values the comparison is made against.
    pub values: Vec<String>,
    /// Whether the client cannot play a file that breaks the limitation.
    pub required: bool,
}

/// One codec profile: the limitations of one codec or container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CodecProfile {
    /// Whether the profile is about a `Codec` or a `Container`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The codec or container the profile is about.
    pub name: String,
    /// The limitations it imposes.
    pub limitations: Vec<CodecLimitation>,
}

/// What a client says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientInfo {
    /// The client's name.
    pub name: String,
    /// The platform it runs on.
    pub platform: String,
    /// The most bitrate it plays, in bits per second; zero means no limit.
    pub max_audio_bitrate: u32,
    /// The most bitrate it accepts a transcode at, in bits per second; zero
    /// means no limit.
    pub max_transcoding_audio_bitrate: u32,
    /// What it plays directly.
    pub direct_play_profiles: Vec<DirectPlayProfile>,
    /// What it accepts a transcode in.
    pub transcoding_profiles: Vec<TranscodingProfile>,
    /// The limitations of its codec support.
    pub codec_profiles: Vec<CodecProfile>,
}

/// What one media file is, as far as the decision cares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceStream {
    /// The file's container.
    pub container: String,
    /// The file's audio codec.
    pub codec: String,
    /// How many channels it has.
    pub audio_channels: u8,
    /// Its bitrate in bits per second; zero when unknown.
    pub audio_bitrate: u32,
    /// Its sample rate in hertz; zero when unknown.
    pub audio_samplerate: u32,
    /// Its bit depth; zero when unknown.
    pub audio_bitdepth: u32,
}

/// What to encode a transcode in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeTarget {
    /// The format to encode.
    pub format: TranscodeFormat,
    /// The bitrate in kbit/s.
    pub bitrate_kbps: u32,
    /// How many channels the output carries, or `None` for the source's own.
    pub max_channels: Option<u8>,
}

/// The decision for one client and one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Whether the client plays the file as it is.
    pub can_direct_play: bool,
    /// What to encode when it cannot.
    pub transcode: Option<TranscodeTarget>,
    /// One reason per direct-play profile that did not cover the file.
    pub reasons: Vec<TranscodeReason>,
}

/// Decides how one client is served one file.
///
/// # Parameters
///
/// - `client`: what the client says about itself.
/// - `source`: the file the server would serve.
///
/// # Returns
///
/// The decision: direct play when a direct-play profile covers the file and
/// every required codec limitation for its codec holds, otherwise the first
/// transcoding profile that maps to a format this server encodes.
pub fn decide(client: &ClientInfo, source: &SourceStream) -> Decision {
    let mut reasons = Vec::new();
    for profile in &client.direct_play_profiles {
        match direct_play(profile, client, source) {
            Ok(()) => {
                return Decision {
                    can_direct_play: true,
                    transcode: None,
                    reasons,
                };
            }
            Err(reason) => reasons.push(reason),
        }
    }

    Decision {
        can_direct_play: false,
        transcode: transcode_target(client, source),
        reasons,
    }
}

/// Reports whether one direct-play profile covers a file.
///
/// # Parameters
///
/// - `profile`: the profile.
/// - `client`: the client, whose caps the profile's are checked against.
/// - `source`: the file.
///
/// # Returns
///
/// `Ok(())` when the profile covers the file, and the reason it does not.
fn direct_play(
    profile: &DirectPlayProfile,
    client: &ClientInfo,
    source: &SourceStream,
) -> Result<(), TranscodeReason> {
    if !lists(&profile.containers, &source.container, &["m4a", "mp4"]) {
        return Err(TranscodeReason::ContainerNotSupported);
    }
    if !lists(&profile.audio_codecs, &source.codec, &[]) {
        return Err(TranscodeReason::AudioCodecNotSupported);
    }
    if !serves_protocol(&profile.protocols, &[]) {
        return Err(TranscodeReason::ProtocolNotSupported);
    }
    if profile.max_audio_channels > 0 && source.audio_channels > profile.max_audio_channels {
        return Err(TranscodeReason::AudioChannelsNotSupported);
    }
    if client.max_audio_bitrate > 0 && source.audio_bitrate > client.max_audio_bitrate {
        return Err(TranscodeReason::AudioBitrateNotSupported);
    }
    if !limitations_hold(&client.codec_profiles, source) {
        return Err(TranscodeReason::CodecLimitationNotMet);
    }
    Ok(())
}

/// Returns the first format a client accepts a transcode in.
///
/// # Parameters
///
/// - `client`: what the client says about itself.
/// - `source`: the file being served.
///
/// # Returns
///
/// The target, or `None` when no profile maps to a format this server encodes.
fn transcode_target(client: &ClientInfo, source: &SourceStream) -> Option<TranscodeTarget> {
    for profile in &client.transcoding_profiles {
        if !serves_protocol(std::slice::from_ref(&profile.protocol), &[]) {
            continue;
        }
        let Some(format) = format_for(&profile.container, &profile.audio_codec) else {
            continue;
        };
        let mut bitrate = format.default_bitrate_kbps();
        bitrate = capped(bitrate, client.max_transcoding_audio_bitrate);
        bitrate = capped(bitrate, client.max_audio_bitrate);
        if let Some(codec_cap) = codec_bitrate_cap(&client.codec_profiles, source, format) {
            bitrate = bitrate.min(codec_cap);
        }
        let channels = match profile.max_audio_channels {
            0 => None,
            allowed => Some(source.audio_channels.min(allowed)),
        };
        return Some(TranscodeTarget {
            format,
            bitrate_kbps: bitrate.max(8),
            max_channels: channels.filter(|channels| *channels > 0),
        });
    }
    None
}

/// Returns the format a container and codec pair maps to.
///
/// # Parameters
///
/// - `container`: the profile's container.
/// - `codec`: the profile's audio codec.
///
/// # Returns
///
/// The format, or `None` for a pair this server cannot encode.
fn format_for(container: &str, codec: &str) -> Option<TranscodeFormat> {
    let container = container.trim().to_ascii_lowercase();
    let codec = codec.trim().to_ascii_lowercase();
    match (container.as_str(), codec.as_str()) {
        ("mp3", "mp3") => Some(TranscodeFormat::Mp3),
        ("ogg" | "opus", "opus") => Some(TranscodeFormat::Opus),
        ("aac" | "adts", "aac") => Some(TranscodeFormat::Aac),
        _ => None,
    }
}

/// Returns a bitrate cap in kbit/s from a value in bits per second.
///
/// # Parameters
///
/// - `bitrate_kbps`: the bitrate the format would use.
/// - `cap_bps`: the client's cap in bits per second; zero is no cap.
///
/// # Returns
///
/// The lower of the two, in kbit/s.
fn capped(bitrate_kbps: u32, cap_bps: u32) -> u32 {
    match cap_bps {
        0 => bitrate_kbps,
        cap => bitrate_kbps.min(cap / 1000),
    }
}

/// Returns the bitrate cap a client's codec limitations impose.
///
/// Only a `LessThanEqual` limitation on `audioBitrate` lowers the bitrate; the
/// other comparisons say what a client plays, not what it accepts an encode at.
///
/// # Parameters
///
/// - `profiles`: the client's codec profiles.
/// - `source`: the file being served.
/// - `format`: the format the transcode would use.
///
/// # Returns
///
/// The cap in kbit/s, or `None` when no limitation imposes one.
fn codec_bitrate_cap(
    profiles: &[CodecProfile],
    source: &SourceStream,
    format: TranscodeFormat,
) -> Option<u32> {
    let mut cap: Option<u32> = None;
    for profile in profiles {
        if !profile_matches(profile, source, format) {
            continue;
        }
        for limitation in &profile.limitations {
            if !limitation.name.eq_ignore_ascii_case("audioBitrate")
                || !limitation.comparison.eq_ignore_ascii_case("LessThanEqual")
            {
                continue;
            }
            for value in &limitation.values {
                if let Ok(bps) = value.trim().parse::<u32>() {
                    let kbps = (bps / 1000).max(8);
                    cap = Some(cap.map_or(kbps, |current: u32| current.min(kbps)));
                }
            }
        }
    }
    cap
}

/// Reports whether every codec limitation that applies to a file holds.
///
/// A limitation applies to the file when its profile is about the file's codec
/// or its container. A limitation about a property this server does not know
/// fails only when the client marked it required, because a client that
/// requires what the server cannot check cannot be given the file.
///
/// # Parameters
///
/// - `profiles`: the client's codec profiles.
/// - `source`: the file being served.
///
/// # Returns
///
/// `true` when the file may be played directly.
fn limitations_hold(profiles: &[CodecProfile], source: &SourceStream) -> bool {
    for profile in profiles {
        if !about(profile, source) {
            continue;
        }
        for limitation in &profile.limitations {
            match limitation_holds(limitation, source) {
                Some(true) => {}
                // A limitation the server cannot check is a refusal only when
                // the client says it cannot play a file that breaks it.
                None => {
                    if limitation.required {
                        return false;
                    }
                }
                Some(false) => return false,
            }
        }
    }
    true
}

/// Reports whether a codec profile is about a file's codec or container.
///
/// # Parameters
///
/// - `profile`: the profile.
/// - `source`: the file.
///
/// # Returns
///
/// `true` when the profile names the file's codec or container.
fn about(profile: &CodecProfile, source: &SourceStream) -> bool {
    let name = profile.name.trim();
    if name.is_empty() {
        return true;
    }
    name.eq_ignore_ascii_case(&source.codec) || name.eq_ignore_ascii_case(&source.container)
}

/// Reports whether a codec profile applies to a format a transcode would use.
///
/// # Parameters
///
/// - `profile`: the profile.
/// - `source`: the file being served.
/// - `format`: the format the transcode would use.
///
/// # Returns
///
/// `true` when the profile names the source's codec or container, or the
/// format's codec.
fn profile_matches(profile: &CodecProfile, source: &SourceStream, format: TranscodeFormat) -> bool {
    about(profile, source) || profile.name.trim().eq_ignore_ascii_case(format.as_str())
}

/// Reports whether one limitation holds, when the server can check it.
///
/// # Parameters
///
/// - `limitation`: the limitation.
/// - `source`: the file being served.
///
/// # Returns
///
/// `Some(true)` when the limitation holds, `Some(false)` when it does not, and
/// `None` when the property is one this server does not know.
fn limitation_holds(limitation: &CodecLimitation, source: &SourceStream) -> Option<bool> {
    let actual = property(&limitation.name, source)?;
    let mut known = false;
    let mut holds = false;
    for value in &limitation.values {
        let Ok(expected) = value.trim().parse::<f64>() else {
            continue;
        };
        known = true;
        let comparison = limitation.comparison.trim();
        let matched = if comparison.eq_ignore_ascii_case("Equals") {
            (actual - expected).abs() < f64::EPSILON
        } else if comparison.eq_ignore_ascii_case("NotEquals") {
            (actual - expected).abs() >= f64::EPSILON
        } else if comparison.eq_ignore_ascii_case("LessThanEqual") {
            actual <= expected
        } else if comparison.eq_ignore_ascii_case("GreaterThanEqual") {
            actual >= expected
        } else {
            // A comparison this server does not know cannot be checked.
            return None;
        };
        holds |= matched;
    }
    if !known {
        return None;
    }
    Some(holds)
}

/// Returns the value of one limitation property.
///
/// # Parameters
///
/// - `name`: the property name, as the client wrote it.
/// - `source`: the file being served.
///
/// # Returns
///
/// The value, or `None` for a property this server does not know.
fn property(name: &str, source: &SourceStream) -> Option<f64> {
    let name = name.trim();
    if name.eq_ignore_ascii_case("audioBitrate") {
        Some(f64::from(source.audio_bitrate))
    } else if name.eq_ignore_ascii_case("audioChannels") {
        Some(f64::from(source.audio_channels))
    } else if name.eq_ignore_ascii_case("audioSamplerate") {
        Some(f64::from(source.audio_samplerate))
    } else if name.eq_ignore_ascii_case("audioBitdepth") {
        Some(f64::from(source.audio_bitdepth))
    } else {
        None
    }
}

/// Reports whether a list names a value.
///
/// An empty list names everything, and two spellings of the same container are
/// one container.
///
/// # Parameters
///
/// - `listed`: the list the profile carries.
/// - `value`: the value to find.
/// - `equivalents`: other spellings of the value.
///
/// # Returns
///
/// `true` when the list is empty or names the value.
fn lists(listed: &[String], value: &str, equivalents: &[&str]) -> bool {
    if listed.is_empty() {
        return true;
    }
    listed.iter().any(|entry| {
        let entry = entry.trim();
        entry.eq_ignore_ascii_case(value)
            || equivalents
                .iter()
                .any(|other| entry.eq_ignore_ascii_case(other))
    })
}

/// Reports whether a profile's protocols are ones this server serves.
///
/// # Parameters
///
/// - `protocols`: the protocols the profile lists.
/// - `allowed`: further protocols this server serves, beyond `http`.
///
/// # Returns
///
/// `true` when the list is empty or names `http`, which is the only protocol
/// this server serves.
fn serves_protocol(protocols: &[String], allowed: &[&str]) -> bool {
    protocols.iter().all(|protocol| {
        let protocol = protocol.trim();
        protocol.is_empty()
            || protocol.eq_ignore_ascii_case("http")
            || allowed
                .iter()
                .any(|other| protocol.eq_ignore_ascii_case(other))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a client with one direct-play profile and one transcode profile.
    fn client(direct: DirectPlayProfile, transcode: TranscodingProfile) -> ClientInfo {
        ClientInfo {
            direct_play_profiles: vec![direct],
            transcoding_profiles: vec![transcode],
            ..ClientInfo::default()
        }
    }

    /// Returns the file most cases use: a 320 kbit/s stereo mp3.
    fn source() -> SourceStream {
        SourceStream {
            container: "mp3".to_owned(),
            codec: "mp3".to_owned(),
            audio_channels: 2,
            audio_bitrate: 320_000,
            audio_samplerate: 44_100,
            audio_bitdepth: 16,
        }
    }

    /// A profile that lists nothing plays anything.
    #[test]
    fn an_empty_profile_plays_anything() {
        let client = client(DirectPlayProfile::default(), TranscodingProfile::default());
        let decision = decide(&client, &source());
        assert!(decision.can_direct_play);
        assert_eq!(decision.transcode, None);
        assert!(decision.reasons.is_empty());
    }

    /// Two spellings of one container are one container.
    #[test]
    fn m4a_and_mp4_are_one_container() {
        let client = client(
            DirectPlayProfile {
                containers: vec!["m4a".to_owned()],
                ..DirectPlayProfile::default()
            },
            TranscodingProfile::default(),
        );
        let source = SourceStream {
            container: "mp4".to_owned(),
            ..source()
        };
        assert!(decide(&client, &source).can_direct_play);
    }

    /// Every direct-play profile that does not cover the file leaves a reason.
    #[test]
    fn each_uncovered_profile_leaves_a_reason() {
        let client = ClientInfo {
            direct_play_profiles: vec![
                DirectPlayProfile {
                    containers: vec!["flac".to_owned()],
                    ..DirectPlayProfile::default()
                },
                DirectPlayProfile {
                    protocols: vec!["hls".to_owned()],
                    ..DirectPlayProfile::default()
                },
                DirectPlayProfile {
                    max_audio_channels: 1,
                    ..DirectPlayProfile::default()
                },
                DirectPlayProfile {
                    audio_codecs: vec!["flac".to_owned()],
                    ..DirectPlayProfile::default()
                },
            ],
            transcoding_profiles: vec![TranscodingProfile {
                container: "mp3".to_owned(),
                audio_codec: "mp3".to_owned(),
                ..TranscodingProfile::default()
            }],
            ..ClientInfo::default()
        };
        let decision = decide(&client, &source());
        assert_eq!(
            decision.reasons,
            [
                TranscodeReason::ContainerNotSupported,
                TranscodeReason::ProtocolNotSupported,
                TranscodeReason::AudioChannelsNotSupported,
                TranscodeReason::AudioCodecNotSupported,
            ]
        );
        let target = decision.transcode.expect("a transcode is offered");
        assert_eq!(target.format, TranscodeFormat::Mp3);
        assert_eq!(target.bitrate_kbps, 320, "the format's own default");
        assert_eq!(target.max_channels, None, "the profile accepts any count");
    }

    /// A codec limitation the client requires and the server cannot check
    /// refuses the file; one the client does not require does not.
    #[test]
    fn an_uncheckable_limitation_only_refuses_when_it_is_required() {
        let limitation = CodecLimitation {
            name: "audioProfile".to_owned(),
            comparison: "Equals".to_owned(),
            values: vec!["LC".to_owned()],
            required: false,
        };
        let mut client = client(DirectPlayProfile::default(), TranscodingProfile::default());
        client.codec_profiles = vec![CodecProfile {
            kind: "Codec".to_owned(),
            name: "mp3".to_owned(),
            limitations: vec![limitation.clone()],
        }];
        assert!(
            decide(&client, &source()).can_direct_play,
            "an unrequired limitation the server cannot check is not a refusal"
        );

        client.codec_profiles[0].limitations[0].required = true;
        let decision = decide(&client, &source());
        assert!(!decision.can_direct_play);
        assert_eq!(decision.reasons, [TranscodeReason::CodecLimitationNotMet]);
    }

    /// The comparisons a codec limitation may carry.
    #[test]
    fn limitations_are_compared_as_the_client_asked() {
        let holds = |comparison: &str, value: &str| {
            let mut client = client(DirectPlayProfile::default(), TranscodingProfile::default());
            client.codec_profiles = vec![CodecProfile {
                kind: "Codec".to_owned(),
                name: "mp3".to_owned(),
                limitations: vec![CodecLimitation {
                    name: "audioChannels".to_owned(),
                    comparison: comparison.to_owned(),
                    values: vec![value.to_owned()],
                    required: true,
                }],
            }];
            decide(&client, &source()).can_direct_play
        };

        assert!(holds("Equals", "2"), "the file has two channels");
        assert!(!holds("Equals", "1"));
        assert!(holds("NotEquals", "1"));
        assert!(!holds("NotEquals", "2"));
        assert!(holds("LessThanEqual", "2"));
        assert!(!holds("LessThanEqual", "1"));
        assert!(holds("GreaterThanEqual", "2"));
        assert!(!holds("GreaterThanEqual", "6"));
        assert!(
            !holds("SomethingElse", "2"),
            "a required comparison the server cannot check refuses the file"
        );
    }

    /// A transcode takes the lowest of the format, the client's caps, and the
    /// codec limitation, and the source's channel count when the profile
    /// accepts more than it has.
    #[test]
    fn a_transcode_takes_the_lowest_bitrate_and_the_channel_cap() {
        let mut client = ClientInfo {
            max_audio_bitrate: 128_000,
            max_transcoding_audio_bitrate: 192_000,
            direct_play_profiles: vec![DirectPlayProfile {
                containers: vec!["flac".to_owned()],
                ..DirectPlayProfile::default()
            }],
            transcoding_profiles: vec![TranscodingProfile {
                container: "aac".to_owned(),
                audio_codec: "aac".to_owned(),
                max_audio_channels: 6,
                ..TranscodingProfile::default()
            }],
            ..ClientInfo::default()
        };
        let target = decide(&client, &source())
            .transcode
            .expect("a transcode is offered");
        assert_eq!(target.format, TranscodeFormat::Aac);
        assert_eq!(target.bitrate_kbps, 128, "the client's own cap wins");
        assert_eq!(
            target.max_channels,
            Some(2),
            "the profile allows more channels than the file has"
        );

        // A profile this server cannot encode falls through to the next one.
        client.transcoding_profiles = vec![
            TranscodingProfile {
                container: "flac".to_owned(),
                audio_codec: "flac".to_owned(),
                ..TranscodingProfile::default()
            },
            TranscodingProfile {
                container: "mp3".to_owned(),
                audio_codec: "mp3".to_owned(),
                ..TranscodingProfile::default()
            },
        ];
        let target = decide(&client, &source())
            .transcode
            .expect("a transcode is offered");
        assert_eq!(target.format, TranscodeFormat::Mp3);
    }

    /// A codec limitation lowers the bitrate a transcode asks for.
    #[test]
    fn a_codec_limitation_lowers_the_transcode_bitrate() {
        let mut client = client(
            DirectPlayProfile {
                containers: vec!["flac".to_owned()],
                ..DirectPlayProfile::default()
            },
            TranscodingProfile {
                container: "ogg".to_owned(),
                audio_codec: "opus".to_owned(),
                ..TranscodingProfile::default()
            },
        );
        client.codec_profiles = vec![CodecProfile {
            kind: "Codec".to_owned(),
            name: "opus".to_owned(),
            limitations: vec![CodecLimitation {
                name: "audioBitrate".to_owned(),
                comparison: "LessThanEqual".to_owned(),
                values: vec!["64000".to_owned()],
                required: false,
            }],
        }];
        let target = decide(&client, &source())
            .transcode
            .expect("a transcode is offered");
        assert_eq!(target.format, TranscodeFormat::Opus);
        assert_eq!(target.bitrate_kbps, 64);
    }
}
