//! Repository maintenance tasks.
//!
//! Run through the `cargo xtask` alias. Each subcommand exists because it needs
//! the workspace's own dependencies or the repository root, which a shell script
//! in CI would have to reimplement.
#![forbid(unsafe_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Parser, Subcommand};
use sha2::{Digest, Sha256};

/// The repository the WebAssembly interface files are owned by.
const PLUGIN_SDK_URL: &str = "https://github.com/hearthsound/plugin-sdk";

/// The lock file that pins the bundled first-party plugins.
const BUNDLED_PLUGINS_LOCK: &str = "bundled-plugins.lock";

/// Where `fetch-plugins` writes the archives unless `--out` overrides it.
const DEFAULT_BUNDLED_PLUGINS_DIR: &str = "target/bundled-plugins";

/// The largest plugin archive `fetch-plugins` accepts, in bytes.
const MAX_PLUGIN_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// How long one plugin download may take in total.
const PLUGIN_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

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
#[command(group(ArgGroup::new("mode").required(true).args(["version", "check"])))]
struct SyncWitArgs {
    /// The plugin-sdk version to copy the interface files from.
    #[arg(long, value_name = "V")]
    version: Option<String>,
    /// Verify the committed interface files against the version in
    /// `wit/VERSION`.
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Task::SyncWit(args) => sync_wit(args),
        Task::Fixtures => fixtures(),
        Task::FetchPlugins(args) => fetch_plugins(args),
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
fn sync_wit(args: SyncWitArgs) -> Result<()> {
    let root = repo_root()?;
    let wit_dir = root.join("wit");

    let version = if args.check {
        let raw = fs::read_to_string(wit_dir.join("VERSION"))
            .context("reading wit/VERSION for the pinned plugin-sdk version")?;
        raw.trim().to_owned()
    } else {
        match args.version {
            Some(version) => version,
            None => bail!("pass either --version or --check"),
        }
    };

    let checkout = tempfile::tempdir().context("creating a temporary directory")?;
    let clone_dir = checkout.path().join("plugin-sdk");

    let status = Command::new("git")
        .args(["clone", "--quiet", "--depth", "1", "--branch"])
        .arg(format!("v{version}"))
        .arg(PLUGIN_SDK_URL)
        .arg(&clone_dir)
        .status()
        .context("running git clone")?;
    if !status.success() {
        bail!("git clone of plugin-sdk v{version} failed");
    }

    let source = clone_dir.join("wit");
    let files = read_wit_files(&source)?;
    if files.is_empty() {
        bail!("{} contains no .wit files", source.display());
    }

    if args.check {
        check_wit(&wit_dir, &source, &files, &version)
    } else {
        write_wit(&wit_dir, &source, &files, &version)
    }
}

/// Lists the `.wit` file names in a directory.
///
/// The directory must contain only `.wit` files; a subdirectory is an error,
/// because the sync copies a flat package and a nested layout would silently
/// drop files.
fn read_wit_files(dir: &Path) -> Result<Vec<String>> {
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

/// Replaces `wit/` with the files from `source`.
fn write_wit(wit_dir: &Path, source: &Path, files: &[String], version: &str) -> Result<()> {
    fs::create_dir_all(wit_dir)
        .with_context(|| format!("creating directory {}", wit_dir.display()))?;

    for entry in
        fs::read_dir(wit_dir).with_context(|| format!("reading directory {}", wit_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("wit") {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }

    for file in files {
        let target = wit_dir.join(file);
        fs::copy(source.join(file), &target)
            .with_context(|| format!("copying {} to {}", file, target.display()))?;
    }

    let version_file = wit_dir.join("VERSION");
    fs::write(&version_file, format!("{version}\n"))
        .with_context(|| format!("writing {}", version_file.display()))?;

    println!("wit/ synced from plugin-sdk v{version}");
    Ok(())
}

/// Compares `wit/` with `source`, reporting every difference.
fn check_wit(wit_dir: &Path, source: &Path, files: &[String], version: &str) -> Result<()> {
    let found = if wit_dir.is_dir() {
        read_wit_files(wit_dir)?
    } else {
        Vec::new()
    };

    let mut mismatched = false;
    for file in files {
        let target = wit_dir.join(file);
        if !target.is_file() {
            println!("wit/{file}: missing");
            mismatched = true;
        } else if fs::read(source.join(file)).with_context(|| format!("reading {file}"))?
            != fs::read(&target).with_context(|| format!("reading {}", target.display()))?
        {
            println!("wit/{file}: differs");
            mismatched = true;
        }
    }
    for file in &found {
        if !files.contains(file) {
            println!("wit/{file}: unexpected");
            mismatched = true;
        }
    }

    if mismatched {
        std::process::exit(1);
    }

    println!("wit/ matches plugin-sdk v{version}");
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
    let expected = data_encoding::HEXLOWER
        .decode(plugin.sha256.as_bytes())
        .with_context(|| format!("{}: `sha256` is not lowercase hex", plugin.id))?;
    if expected.len() != 32 {
        bail!(
            "{}: `sha256` decodes to {} bytes, expected 32",
            plugin.id,
            expected.len()
        );
    }

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
