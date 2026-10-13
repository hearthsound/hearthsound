//! The command line tree.
//!
//! Every command is a clap type in this module and an implementation in
//! [`crate::commands`]. The tree is the documented surface of the product: a
//! command that appears in `--help` does what its help says, and a command that
//! cannot do what it says does not ship.

use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};

/// The Hearthsound command line.
#[derive(Debug, Parser)]
#[command(
    name = "hearthsound",
    version,
    about = "Self-hosted music server",
    arg_required_else_help = true
)]
pub struct Cli {
    /// The TOML configuration file to read.
    ///
    /// A missing file is not an error: every bootstrap key can also come from a
    /// `HEARTHSOUND__SECTION__KEY` environment variable, which wins over the
    /// file.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Every command the binary understands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the server.
    Serve,
    /// Apply pending database migrations.
    ///
    /// A database that already holds data is copied into `<data>/backups` with
    /// `VACUUM INTO` before the migrations run, and the command reports the
    /// version it started at and the version it ended at.
    Migrate,
    /// Index the tags of the files in the library folders.
    Scan(ScanArgs),
    /// Manage accounts, credentials, and library folders.
    Admin(AdminArgs),
    /// Install plugins and list what is installed.
    Plugins(PluginsArgs),
    /// Configure provider instances and list them.
    Providers(ProvidersArgs),
    /// Assign provider instances to the layers they take part in.
    Layers(LayersArgs),
    // The acquisition commands (`acquisitions list` and `acquisitions cancel`)
    // are registered here once the acquisition orchestrator exists.
}

/// The arguments of `scan`.
#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Scan one folder instead of every folder.
    ///
    /// The value is either a library folder id or the path of a folder that was
    /// added with `admin add-folder`.
    #[arg(long, value_name = "PATH-OR-ID")]
    pub folder: Option<String>,
}

/// The arguments of `admin`.
#[derive(Debug, Args)]
pub struct AdminArgs {
    /// The administration command to run.
    #[command(subcommand)]
    pub command: AdminCommand,
}

/// Every command under `admin`.
#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Create an account.
    CreateUser(CreateUserArgs),
    /// Create an API key for an account.
    CreateApiKey(CreateCredentialArgs),
    /// Create an app password for an account.
    CreateAppPassword(CreateCredentialArgs),
    /// Add a library folder, owned by a user or generic.
    AddFolder(AddFolderArgs),
}

/// The arguments of `admin create-user`.
#[derive(Debug, Args)]
pub struct CreateUserArgs {
    /// Make the account an administrator.
    #[arg(long)]
    pub admin: bool,
    /// The login name. Defaults to `admin` when `--admin` is given without it.
    #[arg(long, value_name = "USERNAME")]
    pub username: Option<String>,
    /// The name shown to people. Defaults to the login name.
    #[arg(long, value_name = "NAME")]
    pub display_name: Option<String>,
    /// Read the password from standard input instead of prompting twice.
    ///
    /// One line is read, without its trailing newline, so a password may contain
    /// spaces.
    #[arg(long)]
    pub password_stdin: bool,
}

/// The arguments of a command that creates a credential for an account.
#[derive(Debug, Args)]
pub struct CreateCredentialArgs {
    /// The login name of the account the credential belongs to.
    #[arg(long, value_name = "USERNAME")]
    pub user: String,
    /// The name to give the credential, for example the client it is for.
    #[arg(long, value_name = "NAME")]
    pub name: String,
}

/// The arguments of `admin add-folder`.
#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("ownership")
        .required(true)
        .args(["owner", "generic"])
))]
pub struct AddFolderArgs {
    /// The directory to add to the scan set.
    #[arg(long, value_name = "PATH")]
    pub path: PathBuf,
    /// Associate the folder with a user, which puts its content in that user's
    /// personal library and pins it.
    #[arg(long, value_name = "USERNAME")]
    pub owner: Option<String>,
    /// Leave the folder generic: it belongs to no user and any user may adopt
    /// its items.
    #[arg(long)]
    pub generic: bool,
    /// The folder's display name. Defaults to the directory's file name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
}

/// The arguments of `plugins`.
#[derive(Debug, Args)]
pub struct PluginsArgs {
    /// The plugin command to run.
    #[command(subcommand)]
    pub command: PluginsCommand,
}

/// Every command under `plugins`.
#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// Install a `.hsplugin` archive and enable it.
    Install(InstallPluginArgs),
    /// List the installed plugins.
    List,
}

/// The arguments of `plugins install`.
#[derive(Debug, Args)]
pub struct InstallPluginArgs {
    /// The `.hsplugin` archive to install.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,
    /// Record the plugin as shipped with the server rather than sideloaded.
    #[arg(long)]
    pub bundled: bool,
}

/// The arguments of `providers`.
#[derive(Debug, Args)]
pub struct ProvidersArgs {
    /// The provider command to run.
    #[command(subcommand)]
    pub command: ProvidersCommand,
}

/// Every command under `providers`.
#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    /// Configure a provider instance from an installed plugin.
    Add(AddProviderArgs),
    /// List the configured provider instances.
    List,
}

/// The arguments of `providers add`.
#[derive(Debug, Args)]
pub struct AddProviderArgs {
    /// The id of the installed plugin the instance is configured from.
    #[arg(long, value_name = "ID")]
    pub plugin: String,
    /// The instance slug, `^[a-z0-9]{2,24}$`, unique and immutable.
    #[arg(long, value_name = "SLUG")]
    pub slug: String,
    /// The plugin's server-scoped configuration, as a JSON object.
    ///
    /// The bootstrap configuration file stays `--config`; this flag carries the
    /// configuration the plugin itself is given.
    #[arg(long = "plugin-config", value_name = "JSON")]
    pub server_config: String,
    /// The name shown in the admin UI. Defaults to the slug.
    #[arg(long, value_name = "NAME")]
    pub display_name: Option<String>,
    /// A directory the instance may reach, written `<id>=<path>:<mode>` with a
    /// mode of `move`, `copy`, or `link`. May be given more than once.
    #[arg(long = "import-root", value_name = "ID=PATH:MODE")]
    pub import_roots: Vec<String>,
}

/// The arguments of `layers`.
#[derive(Debug, Args)]
pub struct LayersArgs {
    /// The layer command to run.
    #[command(subcommand)]
    pub command: LayersCommand,
}

/// Every command under `layers`.
#[derive(Debug, Subcommand)]
pub enum LayersCommand {
    /// Replace the instances of one layer, first is primary.
    Set(SetLayerArgs),
    /// List the instances of every layer.
    List,
}

/// The arguments of `layers set`.
#[derive(Debug, Args)]
pub struct SetLayerArgs {
    /// The layer to assign: `catalog`, `metadata`, `artwork`, `acquisition`,
    /// `verification`, `scrobble`, `discovery`, `lyrics`, or `playlist-import`.
    #[arg(value_name = "LAYER")]
    pub layer: String,
    /// The instance slugs of the layer, in priority order.
    #[arg(value_name = "SLUG", num_args = 1..)]
    pub slugs: Vec<String>,
}
