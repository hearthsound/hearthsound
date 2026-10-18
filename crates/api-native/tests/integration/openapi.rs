//! The committed OpenAPI document matches the routes the crate describes.

use std::ffi::OsStr;
use std::path::Path;

/// The environment variable that makes the test rewrite the document.
const UPDATE_VAR: &str = "HEARTHSOUND_UPDATE_OPENAPI";

#[test]
fn the_committed_document_is_current() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi.json");
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&hearthsound_api_native::openapi())?
    );

    if std::env::var_os(UPDATE_VAR).as_deref() == Some(OsStr::new("1")) {
        std::fs::write(&path, &rendered)?;
        return Ok(());
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == rendered,
        "{} does not match the routes this crate describes; run `cargo xtask openapi`",
        path.display()
    );
    Ok(())
}
