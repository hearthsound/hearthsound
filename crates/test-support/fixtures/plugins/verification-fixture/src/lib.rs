//! Verification fixture: a verification provider driven by server config.
//!
//! The administrator lists fingerprints the plugin should reject and those it
//! cannot identify; a print on the rejection list is a mismatch with a fixed
//! detail, a print on the unknown list is an unknown verdict, and every other
//! print matches. The plugin can also be told to answer every check with an
//! error, which is how a test exercises a service that is down. That makes every
//! verdict reachable without a real fingerprinting service.

use hearthsound_plugin::types::ProviderError;
use hearthsound_plugin::verification_provider::exports::hearthsound::plugin::{
    lifecycle, verification,
};
use serde::Deserialize;

/// The plugin implementation.
struct VerificationFixture;

/// Server-scoped configuration of the plugin.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// The fingerprints the plugin rejects.
    #[serde(default)]
    reject_fingerprints: Vec<String>,
    /// The fingerprints the plugin cannot identify.
    #[serde(default)]
    unknown_fingerprints: Vec<String>,
    /// Whether every check answers with an error.
    #[serde(default)]
    fail: bool,
}

/// Reads the plugin's server configuration.
///
/// # Errors
///
/// Returns [`ProviderError`] when the configuration cannot be read or is not the
/// document the plugin expects.
fn config() -> Result<Config, ProviderError> {
    hearthsound_plugin::config::server_config()
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for VerificationFixture {
    fn validate_config(_server_config: String) -> Result<(), Vec<String>> {
        Ok(())
    }

    fn health() -> Result<String, ProviderError> {
        Ok("verification-fixture ready".to_string())
    }
}

/// Implements the verification interface.
impl verification::Guest for VerificationFixture {
    fn verify(
        _expected: verification::ExpectedTrack,
        print: verification::Fingerprint,
    ) -> Result<verification::VerificationResult, ProviderError> {
        let config = config()?;
        if config.fail {
            return Err(ProviderError::Unavailable(
                "fixture verification failure".to_string(),
            ));
        }
        if config
            .unknown_fingerprints
            .iter()
            .any(|fingerprint| fingerprint == &print.value)
        {
            return Ok(verification::VerificationResult {
                verdict: verification::Verdict::Unknown,
                score: 0.0,
                detail: Some("fixture cannot identify this recording".to_string()),
                identified: Vec::new(),
            });
        }
        if config
            .reject_fingerprints
            .iter()
            .any(|fingerprint| fingerprint == &print.value)
        {
            return Ok(verification::VerificationResult {
                verdict: verification::Verdict::Mismatch,
                score: 0.9,
                detail: Some("fixture rejects this recording".to_string()),
                identified: Vec::new(),
            });
        }
        Ok(verification::VerificationResult {
            verdict: verification::Verdict::Match,
            score: 0.95,
            detail: None,
            identified: Vec::new(),
        })
    }
}

hearthsound_plugin::export_verification_provider!(VerificationFixture);
