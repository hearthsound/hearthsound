//! Repository maintenance tasks.
//!
//! Run through the `cargo xtask` alias. Each subcommand exists because it needs
//! the workspace's own dependencies or the repository root, which a shell script
//! in CI would have to reimplement.
#![forbid(unsafe_code)]

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Parser, Subcommand};
use semver::Version;
use sha2::{Digest, Sha256};

/// The repository the WebAssembly interface files are owned by.
const PLUGIN_SDK_URL: &str = "https://github.com/hearthsound/plugin-sdk";

/// The file inside `wit/` naming the plugin API version it holds.
const WIT_VERSION: &str = "VERSION";

/// The file inside `wit/` naming the previous plugin API minors the host loads.
const WIT_SINCE: &str = "SINCE";

/// The lock file that pins the bundled first-party plugins.
const BUNDLED_PLUGINS_LOCK: &str = "bundled-plugins.lock";

/// Where `fetch-plugins` writes the archives unless `--out` overrides it.
const DEFAULT_BUNDLED_PLUGINS_DIR: &str = "target/bundled-plugins";

/// The largest plugin archive `fetch-plugins` accepts, in bytes.
const MAX_PLUGIN_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// How long one plugin download may take in total.
const PLUGIN_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// The lock file that pins the embedded web client.
const WEB_CLIENT_LOCK: &str = "web-client.lock";

/// Where `fetch-web-client` extracts the client the server crate embeds.
const WEB_CLIENT_DIR: &str = "target/web-client";

/// The marker inside [`WEB_CLIENT_DIR`] recording the extracted release.
const WEB_CLIENT_MARKER: &str = ".version";

/// The entry document a web client archive must hold at its root.
const WEB_CLIENT_INDEX: &str = "index.html";

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Repository maintenance tasks")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Copy or verify the WebAssembly interface files owned by plugin-sdk.
    SyncWit(SyncWitArgs),
    /// Build every fixture plugin into a `.hsplugin` archive.
    Fixtures,
    /// Download the bundled plugins pinned in `bundled-plugins.lock`.
    FetchPlugins(FetchPluginsArgs),
    /// Regenerate the committed OpenAPI document of the native API.
    Openapi,
    /// Print the runtime settings reference as markdown.
    SettingsDoc(SettingsDocArgs),
    /// Download and extract the web client pinned in `web-client.lock`.
    FetchWebClient,
}

/// Arguments of the `settings-doc` task.
#[derive(Debug, Args)]
struct SettingsDocArgs {
    /// File to write; the reference is printed to standard output without it.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
}

/// Arguments of the `fetch-plugins` task.
#[derive(Debug, Args)]
struct FetchPluginsArgs {
    /// Directory the plugin archives are written to.
    ///
    /// Defaults to `target/bundled-plugins` in the repository root.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[command(disable_version_flag = true)]
#[command(group(ArgGroup::new("mode").required(true).multiple(true).args(["version", "check"])))]
struct SyncWitArgs {
    /// The plugin-sdk version to copy the interface files from.
    #[arg(long, value_name = "V")]
    version: Option<String>,
    /// A previous plugin API version to pin beside the primary one.
    ///
    /// May be repeated; each version is written to `wit/since_v0.<minor>/`.
    /// Ignored with `--check`, which reads `wit/SINCE`.
    #[arg(long, value_name = "V")]
    since: Vec<String>,
    /// Verify the committed interface files against the versions pinned in
    /// `wit/VERSION` and `wit/SINCE`.
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Task::SyncWit(args) => sync_wit(args),
        Task::Fixtures => fixtures(),
        Task::FetchPlugins(args) => fetch_plugins(args),
        Task::Openapi => openapi(),
        Task::SettingsDoc(args) => settings_doc(args),
        Task::FetchWebClient => fetch_web_client(),
    }
}

/// Returns the repository root.
fn repo_root() -> Result<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    match manifest.parent() {
        Some(root) => Ok(root.to_path_buf()),
        None => bail!("xtask has no parent directory"),
    }
}

/// Copies or verifies the `wit/` directory against plugin-sdk.
///
/// `wit/` holds the package named by `wit/VERSION`. Each version passed to
/// `--since` is pinned beside it in `wit/since_v0.<minor>/` and named by
/// `wit/SINCE`, because the host still loads those previous minors. Check mode
/// reads both files and verifies every directory they name.
fn sync_wit(args: SyncWitArgs) -> Result<()> {
    let root = repo_root()?;
    let wit_dir = root.join("wit");
    let checkout = tempfile::tempdir().context("creating a temporary directory")?;

    if args.check {
        return check_wit(&root, &wit_dir, checkout.path());
    }

    let version = match args.version {
        Some(version) => version,
        None => bail!("pass either --version or --check"),
    };
    parse_version(&version, "--version")?;
    let since = sorted_versions(&args.since)?;

    let (source, files) = fetch_package(checkout.path(), &version)?;
    write_wit(&wit_dir, &source, &files, &version)?;

    for legacy in &since {
        let (source, files) = fetch_package(checkout.path(), legacy)?;
        let dir = legacy_dir(&wit_dir, legacy)?;
        write_package(&dir, &source, &files)?;
        println!(
            "{}/ synced from plugin-sdk v{legacy}",
            report_label(&root, &dir)
        );
    }
    if !since.is_empty() {
        write_since(&wit_dir, &since)?;
    }
    prune_legacy_dirs(&root, &wit_dir, &since)?;
    Ok(())
}

/// Removes every `wit/since_v*` directory that `wit/SINCE` does not name.
///
/// A legacy package left behind would be exported by every WIT consumer in the
/// workspace and keep a dropped API alive, so a sync that drops a minor removes
/// its directory instead of only forgetting it. `--check` reports such a
/// directory as unexpected, so this keeps a sync the only way to change `wit/`.
fn prune_legacy_dirs(root: &Path, wit_dir: &Path, since: &[String]) -> Result<()> {
    let mut listed = Vec::with_capacity(since.len());
    for version in since {
        listed.push(legacy_dir(wit_dir, version)?);
    }

    let entries = fs::read_dir(wit_dir)
        .with_context(|| format!("reading directory {}", wit_dir.display()))?;
    let mut unlisted = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading directory {}", wit_dir.display()))?;
        let path = entry.path();
        if path.is_dir() && !listed.contains(&path) {
            unlisted.push(path);
        }
    }

    for path in unlisted {
        fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        println!(
            "{}/ removed: no longer named by {WIT_SINCE}",
            report_label(root, &path)
        );
    }
    Ok(())
}

/// Verifies `wit/` and every directory `wit/SINCE` names, reporting every
/// difference and exiting non-zero when one is found.
fn check_wit(root: &Path, wit_dir: &Path, checkout: &Path) -> Result<()> {
    let version = read_version(wit_dir)?;
    parse_version(&version, &format!("wit/{}", WIT_VERSION))?;
    let since = read_since(wit_dir)?;

    let (source, files) = fetch_package(checkout, &version)?;
    let primary = report_label(root, wit_dir);
    let mut mismatched = check_package(wit_dir, &source, &files, &primary)?;
    let mut verified = vec![(primary, version)];

    for legacy in &since {
        let dir = legacy_dir(wit_dir, legacy)?;
        let (source, files) = fetch_package(checkout, legacy)?;
        let label = report_label(root, &dir);
        mismatched |= check_package(&dir, &source, &files, &label)?;
        verified.push((label, legacy.clone()));
    }
    mismatched |= check_unlisted_directories(root, wit_dir, &since)?;

    if mismatched {
        std::process::exit(1);
    }
    for (label, version) in verified {
        println!("{label}/ matches plugin-sdk v{version}");
    }
    Ok(())
}

/// Lists the `.wit` file names of a plugin-sdk package.
///
/// The package must be flat: a subdirectory is an error, because the sync
/// copies the files it names and a nested layout would silently drop files.
fn read_package_files(dir: &Path) -> Result<Vec<String>> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading directory {}", dir.display()))?;
        if entry.file_type()?.is_dir() {
            bail!(
                "{} contains the subdirectory `{}`",
                dir.display(),
                entry.file_name().to_string_lossy()
            );
        }
        if entry.path().extension().and_then(|ext| ext.to_str()) == Some("wit") {
            files.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    files.sort();
    Ok(files)
}

/// Lists the `.wit` file names committed at the top level of a directory.
///
/// Subdirectories are ignored, because `wit/` holds the legacy packages in
/// `since_v0.<minor>/` beside its own files. A missing directory has no files.
fn read_committed_files(dir: &Path) -> Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("reading directory {}", dir.display()))?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        if entry.path().extension().and_then(|ext| ext.to_str()) == Some("wit") {
            files.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    files.sort();
    Ok(files)
}

/// Returns the plugin API version `wit/` holds.
fn read_version(wit_dir: &Path) -> Result<String> {
    let path = wit_dir.join(WIT_VERSION);
    let raw = fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} for the pinned plugin-sdk version",
            path.display()
        )
    })?;
    Ok(raw.trim().to_owned())
}

/// Returns the previous plugin API versions named by `wit/SINCE`.
///
/// Empty lines and lines starting with `#` are skipped and every other line
/// must be a full `X.Y.Z` version. The result is deduplicated and sorted
/// ascending, so the file's own order never matters.
fn read_since(wit_dir: &Path) -> Result<Vec<String>> {
    let path = wit_dir.join(WIT_SINCE);
    let raw = fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} for the pinned legacy plugin API versions",
            path.display()
        )
    })?;

    let mut versions = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let origin = format!("{}:{}", path.display(), index + 1);
        versions.push(parse_version(line, &origin)?);
    }
    Ok(canonical_versions(versions))
}

/// Parses a plugin API version, naming its origin in the error.
fn parse_version(version: &str, origin: &str) -> Result<Version> {
    Version::parse(version)
        .with_context(|| format!("{origin}: `{version}` is not a full X.Y.Z plugin API version"))
}

/// Parses, deduplicates and sorts a list of plugin API versions ascending.
fn sorted_versions(versions: &[String]) -> Result<Vec<String>> {
    let mut parsed = Vec::with_capacity(versions.len());
    for version in versions {
        parsed.push(parse_version(version, "--since")?);
    }
    Ok(canonical_versions(parsed))
}

/// Sorts parsed plugin API versions ascending, drops duplicates and renders
/// them back as `X.Y.Z` strings.
fn canonical_versions(mut versions: Vec<Version>) -> Vec<String> {
    versions.sort();
    versions.dedup();
    versions
        .into_iter()
        .map(|version| version.to_string())
        .collect()
}

/// Returns the directory a previous plugin API version is pinned in.
///
/// The name carries the major and minor only (`wit/since_v0.3/`), so a version
/// with a non-zero patch has no stable directory and is refused.
fn legacy_dir(wit_dir: &Path, version: &str) -> Result<PathBuf> {
    let parsed = parse_version(version, "legacy plugin API version")?;
    if parsed.patch != 0 {
        bail!(
            "`{version}` has a non-zero patch; a legacy API is pinned in since_v{major}.{minor}/",
            major = parsed.major,
            minor = parsed.minor
        );
    }
    Ok(wit_dir.join(format!("since_v{}.{}", parsed.major, parsed.minor)))
}

/// Clones plugin-sdk at `version` into `parent` and returns the `.wit` package
/// it holds.
fn fetch_package(parent: &Path, version: &str) -> Result<(PathBuf, Vec<String>)> {
    let clone_dir = parent.join(format!("plugin-sdk-{version}"));

    let status = Command::new("git")
        .args([
            "-c",
            "advice.detachedHead=false",
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--branch",
        ])
        .arg(format!("v{version}"))
        .arg(PLUGIN_SDK_URL)
        .arg(&clone_dir)
        .status()
        .context("running git clone")?;
    if !status.success() {
        bail!("git clone of plugin-sdk v{version} failed");
    }

    let source = clone_dir.join("wit");
    let files = read_package_files(&source)?;
    if files.is_empty() {
        bail!("{} contains no .wit files", source.display());
    }
    Ok((source, files))
}

/// Returns how a directory is named in the report, relative to the repository
/// root.
fn report_label(root: &Path, dir: &Path) -> String {
    match dir.strip_prefix(root) {
        Ok(relative) => relative.display().to_string(),
        Err(_) => dir.display().to_string(),
    }
}

/// Replaces `wit/`'s package with the files from `source` and pins the version.
fn write_wit(wit_dir: &Path, source: &Path, files: &[String], version: &str) -> Result<()> {
    write_package(wit_dir, source, files)?;

    let version_file = wit_dir.join(WIT_VERSION);
    fs::write(&version_file, format!("{version}\n"))
        .with_context(|| format!("writing {}", version_file.display()))?;

    println!("wit/ synced from plugin-sdk v{version}");
    Ok(())
}

/// Replaces the top-level `.wit` files in `dir` with the ones from `source`.
///
/// Only files are removed, so the `since_v0.<minor>/` subdirectories beside the
/// primary package survive and no legacy package is ever touched.
fn write_package(dir: &Path, source: &Path, files: &[String]) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating directory {}", dir.display()))?;

    for entry in
        fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("wit") {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }

    for file in files {
        let target = dir.join(file);
        fs::copy(source.join(file), &target)
            .with_context(|| format!("copying {} to {}", file, target.display()))?;
    }
    Ok(())
}

/// Writes `wit/SINCE`, naming the previous plugin API minors the host loads.
fn write_since(wit_dir: &Path, versions: &[String]) -> Result<()> {
    let mut body = String::from(
        "# Previous plugin API minors the host still loads, one `X.Y.Z` version per\n\
         # line. The directory is wit/since_v0.<minor>/.\n",
    );
    for version in versions {
        body.push_str(version);
        body.push('\n');
    }

    let path = wit_dir.join(WIT_SINCE);
    fs::write(&path, body).with_context(|| format!("writing {}", path.display()))
}

/// Compares the package committed in `dir` with the one at `source`.
///
/// `label` is how the report names the directory, so a difference reads
/// `wit/since_v0.3/host.wit: differs`. Returns whether anything differed.
fn check_package(dir: &Path, source: &Path, files: &[String], label: &str) -> Result<bool> {
    let found = read_committed_files(dir)?;

    let mut mismatched = false;
    for file in files {
        let target = dir.join(file);
        if !target.is_file() {
            println!("{label}/{file}: missing");
            mismatched = true;
        } else if fs::read(source.join(file)).with_context(|| format!("reading {file}"))?
            != fs::read(&target).with_context(|| format!("reading {}", target.display()))?
        {
            println!("{label}/{file}: differs");
            mismatched = true;
        }
    }
    for file in &found {
        if !files.contains(file) {
            println!("{label}/{file}: unexpected");
            mismatched = true;
        }
    }
    Ok(mismatched)
}

/// Reports every subdirectory of `wit/` that `wit/SINCE` does not name.
///
/// Only `since_v0.<minor>/` packages listed by `SINCE` belong there; anything
/// else was pinned outside the sync. Returns whether one was found.
fn check_unlisted_directories(root: &Path, wit_dir: &Path, since: &[String]) -> Result<bool> {
    if !wit_dir.is_dir() {
        return Ok(false);
    }

    let mut listed = Vec::with_capacity(since.len());
    for version in since {
        listed.push(legacy_dir(wit_dir, version)?);
    }

    let mut mismatched = false;
    for entry in
        fs::read_dir(wit_dir).with_context(|| format!("reading directory {}", wit_dir.display()))?
    {
        let entry = entry.with_context(|| format!("reading directory {}", wit_dir.display()))?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if !listed.contains(&path) {
            println!("{}/: unexpected directory", report_label(root, &path));
            mismatched = true;
        }
    }
    Ok(mismatched)
}

/// Regenerates the committed OpenAPI document of the native API.
///
/// The integration test owns both the rendering and the comparison, so the
/// command runs it with the variable that makes it write the file. The test
/// itself is what CI runs without the variable.
fn openapi() -> Result<()> {
    let root = repo_root()?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .args([
            "test",
            "-p",
            "hearthsound-api-native",
            "--test",
            "integration",
            "openapi",
        ])
        .env("HEARTHSOUND_UPDATE_OPENAPI", "1")
        .current_dir(&root)
        .status()
        .context("running the native API document test")?;
    if !status.success() {
        bail!("writing crates/api-native/openapi.json failed");
    }
    println!("crates/api-native/openapi.json is up to date");
    Ok(())
}

/// Writes the runtime settings reference as markdown.
///
/// The registry is the source of truth, so the reference cannot drift from the
/// settings the server actually reads. One table is written per dotted area,
/// with the key, its default, the constraint and the description.
///
/// # Parameters
///
/// - `args`: where to write the reference; standard output when absent.
fn settings_doc(args: SettingsDocArgs) -> Result<()> {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;

    use hearthsound_config::{SettingKind, SettingValue, SettingsRegistry};

    /// Formats one default value the way an operator would type it.
    fn default_of(value: &SettingValue) -> String {
        match value {
            SettingValue::Bool(flag) => flag.to_string(),
            SettingValue::Integer(number) => number.to_string(),
            SettingValue::Float(number) => {
                let text = format!("{number}");
                text
            }
            SettingValue::String(text) => {
                if text.is_empty() {
                    "empty".to_owned()
                } else {
                    format!("`{text}`")
                }
            }
            SettingValue::StringList(items) => {
                if items.is_empty() {
                    "empty".to_owned()
                } else {
                    let quoted: Vec<String> =
                        items.iter().map(|item| format!("`{item}`")).collect();
                    quoted.join(", ")
                }
            }
        }
    }

    /// Describes the constraint a kind imposes, when it imposes one.
    fn constraint_of(kind: &SettingKind) -> Option<String> {
        match kind {
            SettingKind::Bool => None,
            SettingKind::Integer { min, max } => Some(format!("{min}..={max}")),
            SettingKind::Float { min, max } => Some(format!("{min}..={max}")),
            SettingKind::String { allowed } | SettingKind::StringList { allowed }
                if allowed.is_empty() =>
            {
                None
            }
            SettingKind::String { allowed } | SettingKind::StringList { allowed } => {
                Some(allowed.join(", "))
            }
        }
    }

    let registry = SettingsRegistry::builtin();
    let mut areas: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for def in registry.iter() {
        let area = def.key.split('.').next().unwrap_or("other");
        let mut row = format!(
            "| `{}` | {} | {} |",
            def.key,
            default_of(&def.default),
            def.description
        );
        if let Some(constraint) = constraint_of(&def.kind) {
            let _ = write!(row, " {} |", constraint);
        }
        if def.user_overridable {
            let _ = write!(row, " yes |");
        }
        areas.entry(area).or_default().push(row);
    }

    let mut out = String::from(
        "<!-- Generated by `cargo xtask settings-doc`; do not edit by hand. -->\n\n# Runtime settings\n\nEach setting is read by env > file > database > default.\n",
    );
    let typed = registry
        .iter()
        .any(|def| constraint_of(&def.kind).is_some() || def.user_overridable);
    for (area, rows) in areas {
        let _ = write!(out, "\n## {area}\n\n| Key | Default | Meaning |");
        if typed {
            let _ = write!(out, " Constraint | User |");
        }
        let rule = if typed {
            "\n|---|---|---|---|---|"
        } else {
            "\n|---|---|---|"
        };
        let _ = writeln!(out, "{rule}");
        for row in rows {
            let _ = writeln!(out, "{row}");
        }
    }

    match args.out {
        Some(path) => {
            std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
            println!("{}", path.display());
        }
        None => print!("{out}"),
    }
    Ok(())
}

/// Builds every fixture plugin into a `.hsplugin` archive.
///
/// Fixture directories live in `crates/test-support/fixtures/plugins/<name>`,
/// where `<name>` is the package name. Each one is built for `wasm32-wasip2`
/// with a shared target directory, then packed with its manifest into
/// `target/fixtures/plugins/<id>-<version>.hsplugin`.
fn fixtures() -> Result<()> {
    let root = repo_root()?;
    let plugins_dir = root.join("crates/test-support/fixtures/plugins");
    let build_dir = root.join("target/fixtures/build");
    let out_dir = root.join("target/fixtures/plugins");

    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());

    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(&plugins_dir)
        .with_context(|| format!("reading directory {}", plugins_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();

    for dir in dirs {
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("fixture directory {} is not valid UTF-8", dir.display()))?
            .to_owned();
        let manifest_path = dir.join("manifest.json");
        let cargo_toml = dir.join("Cargo.toml");

        let status = Command::new(&cargo)
            .args([
                "build",
                "--release",
                "--locked",
                "--target",
                "wasm32-wasip2",
                "--manifest-path",
            ])
            .arg(&cargo_toml)
            .arg("--target-dir")
            .arg(&build_dir)
            .status()
            .with_context(|| format!("building fixture {name}"))?;
        if !status.success() {
            bail!("building fixture {name} failed");
        }

        let manifest_bytes = fs::read(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("parsing {}", manifest_path.display()))?;
        let id = manifest
            .get("id")
            .and_then(|value| value.as_str())
            .with_context(|| format!("{}: manifest has no string `id`", manifest_path.display()))?;
        let version = manifest
            .get("version")
            .and_then(|value| value.as_str())
            .with_context(|| {
                format!(
                    "{}: manifest has no string `version`",
                    manifest_path.display()
                )
            })?;

        let wasm_name = format!("{}.wasm", name.replace('-', "_"));
        let wasm_path = build_dir
            .join("wasm32-wasip2")
            .join("release")
            .join(&wasm_name);
        let wasm =
            fs::read(&wasm_path).with_context(|| format!("reading {}", wasm_path.display()))?;

        let archive = out_dir.join(format!("{id}-{version}.hsplugin"));
        let file = fs::File::create(&archive)
            .with_context(|| format!("creating {}", archive.display()))?;
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer
            .start_file("manifest.json", options)
            .context("writing manifest.json into the archive")?;
        writer.write_all(&manifest_bytes)?;
        writer
            .start_file("plugin.wasm", options)
            .context("writing plugin.wasm into the archive")?;
        writer.write_all(&wasm)?;
        writer.finish().context("finishing the archive")?;

        println!("{}", archive.display());
    }

    Ok(())
}

/// The contents of `bundled-plugins.lock`.
#[derive(Debug, serde::Deserialize)]
struct BundledPlugins {
    /// One entry per bundled archive, in lock file order.
    #[serde(default)]
    plugin: Vec<BundledPlugin>,
}

/// One plugin pinned in `bundled-plugins.lock`.
#[derive(Debug, serde::Deserialize)]
struct BundledPlugin {
    /// The plugin id from its manifest, for example `app.hearthsound.slskd`.
    id: String,
    /// The released plugin version; the release tag is `v<version>`.
    version: String,
    /// Direct download URL of the `.hsplugin` archive.
    url: String,
    /// Lowercase hex SHA-256 of the archive.
    sha256: String,
}

/// Downloads every plugin pinned in `bundled-plugins.lock`.
///
/// Archives land in `args.out`, or in `target/bundled-plugins` under the
/// repository root when it is absent. A download is only written to its final
/// path after its SHA-256 matched the lock file, so an interrupted or tampered
/// download never leaves a usable archive behind. An archive that is already
/// present with the pinned digest is left untouched.
///
/// Fails when the lock file cannot be read or parsed, when a download fails,
/// when a digest does not match, or when an archive exceeds
/// [`MAX_PLUGIN_ARCHIVE_BYTES`].
fn fetch_plugins(args: FetchPluginsArgs) -> Result<()> {
    let root = repo_root()?;
    let lock_path = root.join(BUNDLED_PLUGINS_LOCK);
    let raw = fs::read_to_string(&lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    let lock: BundledPlugins =
        toml::from_str(&raw).with_context(|| format!("parsing {}", lock_path.display()))?;
    if lock.plugin.is_empty() {
        bail!("{} pins no plugins", lock_path.display());
    }

    let out_dir = match args.out {
        Some(out) => out,
        None => root.join(DEFAULT_BUNDLED_PLUGINS_DIR),
    };
    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(concat!("hearthsound-xtask/", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(PLUGIN_DOWNLOAD_TIMEOUT))
            .build(),
    );

    for plugin in &lock.plugin {
        fetch_plugin(&agent, plugin, &out_dir)?;
    }

    Ok(())
}

/// Downloads one pinned archive into `out_dir`, verifying its digest first.
///
/// Returns an error when the pinned `sha256` is not 32 bytes of lowercase hex,
/// when the download fails, or when the downloaded bytes hash to something
/// other than the pinned digest. A present archive whose digest differs is
/// refetched and replaced.
fn fetch_plugin(agent: &ureq::Agent, plugin: &BundledPlugin, out_dir: &Path) -> Result<()> {
    let expected = pinned_digest(&plugin.id, &plugin.sha256)?;

    let target = out_dir.join(format!("{}-{}.hsplugin", plugin.id, plugin.version));
    if target.is_file() {
        let existing =
            fs::read(&target).with_context(|| format!("reading {}", target.display()))?;
        if Sha256::digest(&existing).as_slice() == expected.as_slice() {
            println!("{} (up to date)", target.display());
            return Ok(());
        }
        println!("{} (digest differs, refetching)", target.display());
    }

    let bytes = download(agent, &plugin.url)?;
    let found = Sha256::digest(&bytes);
    if found.as_slice() != expected.as_slice() {
        bail!(
            "{}: sha256 mismatch: the lock file pins {}, the download is {}",
            plugin.url,
            plugin.sha256,
            data_encoding::HEXLOWER.encode(found.as_slice())
        );
    }

    let mut file = tempfile::NamedTempFile::new_in(out_dir)
        .with_context(|| format!("creating a temporary file in {}", out_dir.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing {}", target.display()))?;
    file.persist(&target)
        .with_context(|| format!("moving the download to {}", target.display()))?;
    println!("{}", target.display());

    Ok(())
}

/// Fetches `url` and returns its body, rejecting oversized responses.
fn download(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_PLUGIN_ARCHIVE_BYTES)
        .read_to_vec()
        .with_context(|| format!("reading the body of {url}"))
}

/// The contents of `web-client.lock`.
#[derive(Debug, serde::Deserialize)]
struct WebClientLock {
    /// The one pinned web client release.
    web_client: WebClientPin,
}

/// The web client release pinned in `web-client.lock`.
#[derive(Debug, serde::Deserialize)]
struct WebClientPin {
    /// The released web client version; the release tag is `v<version>`.
    version: String,
    /// Direct download URL of the `.zip` release asset.
    url: String,
    /// Lowercase hex SHA-256 of the archive.
    sha256: String,
}

/// Downloads and extracts the web client pinned in `web-client.lock`.
///
/// The archive is verified against the pinned digest before anything is
/// written, extracted into a staging directory under `target/`, and swapped
/// into `target/web-client/` only once every entry is in place, so the server
/// crate never embeds a half-extracted client. The staged tree records
/// `<version> <sha256>` in `.version`; a tree that already records the pin is
/// left untouched.
///
/// Fails when the lock file cannot be read or parsed, when the download fails
/// or exceeds [`MAX_PLUGIN_ARCHIVE_BYTES`], when the digest does not match, or
/// when the archive is not a client: an entry whose name is absolute or climbs
/// out with `..`, or no `index.html` at its root.
fn fetch_web_client() -> Result<()> {
    let root = repo_root()?;
    let lock_path = root.join(WEB_CLIENT_LOCK);
    let raw = fs::read_to_string(&lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    let lock: WebClientLock =
        toml::from_str(&raw).with_context(|| format!("parsing {}", lock_path.display()))?;
    let pin = lock.web_client;
    let expected = pinned_digest(&pin.version, &pin.sha256)?;

    let out_dir = root.join(WEB_CLIENT_DIR);
    let marker = format!("{} {}", pin.version, pin.sha256);
    if extracted_release(&out_dir).as_deref() == Some(marker.as_str()) {
        println!("{} (up to date at {})", out_dir.display(), pin.version);
        return Ok(());
    }

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(concat!("hearthsound-xtask/", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(PLUGIN_DOWNLOAD_TIMEOUT))
            .build(),
    );
    let bytes = download(&agent, &pin.url)?;
    let found = Sha256::digest(&bytes);
    if found.as_slice() != expected.as_slice() {
        bail!(
            "{}: sha256 mismatch: the lock file pins {}, the download is {}",
            pin.url,
            pin.sha256,
            data_encoding::HEXLOWER.encode(found.as_slice())
        );
    }

    let staging_parent = root.join("target");
    fs::create_dir_all(&staging_parent)
        .with_context(|| format!("creating {}", staging_parent.display()))?;
    extract_web_client(&bytes, &staging_parent, &out_dir, &marker)?;
    println!("{} ({})", out_dir.display(), pin.version);
    Ok(())
}

/// Decodes a pinned SHA-256 digest.
///
/// # Parameters
///
/// - `label`: what the digest belongs to, for the error message.
/// - `sha256`: the digest as the lock file holds it.
///
/// # Errors
///
/// Fails when the digest is not lowercase hex or does not decode to 32 bytes.
fn pinned_digest(label: &str, sha256: &str) -> Result<Vec<u8>> {
    let expected = data_encoding::HEXLOWER
        .decode(sha256.as_bytes())
        .with_context(|| format!("{label}: `sha256` is not lowercase hex"))?;
    if expected.len() != 32 {
        bail!(
            "{label}: `sha256` decodes to {} bytes, expected 32",
            expected.len()
        );
    }
    Ok(expected)
}

/// Reads the release an extracted web client records.
///
/// # Parameters
///
/// - `out_dir`: the extracted client directory.
///
/// # Returns
///
/// The trimmed contents of its `.version` marker, or `None` when there is no
/// extracted client or no marker.
fn extracted_release(out_dir: &Path) -> Option<String> {
    fs::read_to_string(out_dir.join(WEB_CLIENT_MARKER))
        .ok()
        .map(|marker| marker.trim().to_owned())
}

/// Extracts a web client archive and swaps it into place.
///
/// # Parameters
///
/// - `bytes`: the verified archive.
/// - `staging_parent`: where the staging directory is created; it must be on
///   the same filesystem as `out_dir` so the swap is a rename.
/// - `out_dir`: the directory the client ends up in, replaced as a whole.
/// - `marker`: the `<version> <sha256>` line written to `.version`.
///
/// # Errors
///
/// Fails when the archive cannot be read, when an entry name is absolute or
/// climbs out of the directory, when there is no `index.html` at the archive
/// root, or when the files cannot be written or moved.
fn extract_web_client(
    bytes: &[u8],
    staging_parent: &Path,
    out_dir: &Path,
    marker: &str,
) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("reading the archive")?;
    let staging = tempfile::tempdir_in(staging_parent).with_context(|| {
        format!(
            "creating a staging directory in {}",
            staging_parent.display()
        )
    })?;

    let mut has_index = false;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("reading archive entry {index}"))?;
        let name = entry
            .name()
            .with_context(|| format!("reading the name of archive entry {index}"))?
            .into_owned();
        let Some(relative) = safe_entry_path(&name) else {
            bail!("archive entry `{name}` is absolute or leaves the client directory");
        };
        let target = staging.path().join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("creating {}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file =
            fs::File::create(&target).with_context(|| format!("creating {}", target.display()))?;
        std::io::copy(&mut entry, &mut file).with_context(|| format!("extracting `{name}`"))?;
        has_index |= relative == Path::new(WEB_CLIENT_INDEX);
    }
    if !has_index {
        bail!("the archive has no `{WEB_CLIENT_INDEX}` at its root");
    }
    fs::write(
        staging.path().join(WEB_CLIENT_MARKER),
        format!("{marker}\n"),
    )
    .context("writing the version marker")?;

    if out_dir.exists() {
        fs::remove_dir_all(out_dir).with_context(|| format!("removing {}", out_dir.display()))?;
    }
    let staged = staging.keep();
    fs::rename(&staged, out_dir)
        .with_context(|| format!("moving the client to {}", out_dir.display()))?;
    Ok(())
}

/// Turns an archive entry name into a relative path below the client
/// directory.
///
/// # Parameters
///
/// - `name`: the entry name as the archive stores it.
///
/// # Returns
///
/// The relative path, or `None` when the name is empty, absolute, holds a
/// backslash, or has any component other than a plain name (`..`, `.`, a root
/// or a drive prefix).
fn safe_entry_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\\') {
        return None;
    }
    let path = Path::new(name);
    let plain = path
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)));
    if !plain || path.components().next().is_none() {
        return None;
    }
    Some(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Builds a zip archive holding the given files.
    fn archive(files: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in files {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn read_since_skips_comments_and_blank_lines_and_sorts() {
        let wit = tempfile::tempdir().unwrap();
        fs::write(
            wit.path().join(WIT_SINCE),
            "# previous minors, one per line\n\n0.5.0\n# a comment\n0.3.0\n  0.5.0  \n",
        )
        .unwrap();

        assert_eq!(
            read_since(wit.path()).unwrap(),
            vec!["0.3.0".to_owned(), "0.5.0".to_owned()]
        );
    }

    #[test]
    fn read_since_refuses_a_line_that_is_not_a_full_version() {
        let wit = tempfile::tempdir().unwrap();
        fs::write(wit.path().join(WIT_SINCE), "0.3\n").unwrap();

        let error = read_since(wit.path()).unwrap_err();
        assert!(error.to_string().contains("0.3"), "{error}");
    }

    #[test]
    fn a_legacy_api_with_a_patch_release_has_no_directory() {
        let wit = Path::new("wit");

        assert_eq!(
            legacy_dir(wit, "0.3.0").unwrap(),
            PathBuf::from("wit").join("since_v0.3")
        );
        assert!(legacy_dir(wit, "0.3.1").is_err());
    }

    #[test]
    fn writing_the_primary_package_keeps_the_legacy_directories() {
        let wit = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("host.wit"), "new").unwrap();
        fs::write(wit.path().join("stale.wit"), "stale").unwrap();
        fs::create_dir_all(wit.path().join("since_v0.3")).unwrap();
        fs::write(wit.path().join("since_v0.3/host.wit"), "old").unwrap();

        write_wit(wit.path(), source.path(), &["host.wit".to_owned()], "0.4.0").unwrap();

        assert_eq!(
            fs::read_to_string(wit.path().join("host.wit")).unwrap(),
            "new"
        );
        assert!(!wit.path().join("stale.wit").exists());
        assert_eq!(
            fs::read_to_string(wit.path().join("since_v0.3/host.wit")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(wit.path().join(WIT_VERSION)).unwrap(),
            "0.4.0\n"
        );
    }

    #[test]
    fn the_since_file_names_the_legacy_versions() {
        let wit = tempfile::tempdir().unwrap();

        write_since(wit.path(), &["0.3.0".to_owned()]).unwrap();

        assert_eq!(
            fs::read_to_string(wit.path().join(WIT_SINCE)).unwrap(),
            "# Previous plugin API minors the host still loads, one `X.Y.Z` version per\n\
             # line. The directory is wit/since_v0.<minor>/.\n\
             0.3.0\n"
        );
    }

    #[test]
    fn a_legacy_directory_the_since_file_does_not_name_is_removed() {
        let wit = tempfile::tempdir().unwrap();
        fs::create_dir_all(wit.path().join("since_v0.3")).unwrap();
        fs::write(wit.path().join("since_v0.3/host.wit"), "old").unwrap();
        fs::create_dir_all(wit.path().join("since_v0.4")).unwrap();
        fs::write(wit.path().join("since_v0.4/host.wit"), "older").unwrap();
        fs::write(wit.path().join("host.wit"), "current").unwrap();

        prune_legacy_dirs(Path::new("."), wit.path(), &["0.4.0".to_owned()]).unwrap();

        assert!(!wit.path().join("since_v0.3").exists());
        assert_eq!(
            fs::read_to_string(wit.path().join("since_v0.4/host.wit")).unwrap(),
            "older"
        );
        assert_eq!(
            fs::read_to_string(wit.path().join("host.wit")).unwrap(),
            "current"
        );
    }

    #[test]
    fn entry_names_that_leave_the_directory_are_refused() {
        assert_eq!(
            safe_entry_path("index.html"),
            Some(PathBuf::from("index.html"))
        );
        assert_eq!(
            safe_entry_path("_app/immutable/app.js"),
            Some(PathBuf::from("_app/immutable/app.js"))
        );
        assert_eq!(safe_entry_path("_app/"), Some(PathBuf::from("_app")));
        for refused in [
            "",
            "/etc/passwd",
            "../outside",
            "_app/../../x",
            "./index.html",
            "a\\b",
        ] {
            assert_eq!(
                safe_entry_path(refused),
                None,
                "{refused:?} should be refused"
            );
        }
    }

    #[test]
    fn an_archive_with_an_escaping_entry_writes_nothing() {
        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("web-client");
        let bytes = archive(&[("index.html", "<html>"), ("../escape.txt", "x")]);
        let error = extract_web_client(&bytes, parent.path(), &out, "0.1.0 abc").unwrap_err();
        assert!(error.to_string().contains("../escape.txt"), "{error}");
        assert!(!out.exists());
        assert!(!parent.path().join("escape.txt").exists());
    }

    #[test]
    fn an_archive_without_an_index_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("web-client");
        let bytes = archive(&[("app/index.html", "<html>")]);
        assert!(extract_web_client(&bytes, parent.path(), &out, "0.1.0 abc").is_err());
        assert!(!out.exists());
    }

    #[test]
    fn an_extracted_client_records_its_release_and_replaces_the_old_one() {
        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("web-client");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("stale.js"), "old").unwrap();

        let bytes = archive(&[("index.html", "<html>"), ("_app/immutable/app.js", "app")]);
        extract_web_client(&bytes, parent.path(), &out, "0.1.0 abc").unwrap();

        assert_eq!(
            fs::read_to_string(out.join("index.html")).unwrap(),
            "<html>"
        );
        assert_eq!(
            fs::read_to_string(out.join("_app/immutable/app.js")).unwrap(),
            "app"
        );
        assert!(!out.join("stale.js").exists());
        assert_eq!(extracted_release(&out).as_deref(), Some("0.1.0 abc"));
    }
}
