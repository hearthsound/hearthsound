//! The command line tree.
//!
//! Every command is a clap type in this module and an implementation in
//! [`crate::commands`]. The tree is the documented surface of the product: a
//! command that appears in `--help` does what its help says, and a command that
//! cannot do what it says does not ship.

use std::path::PathBuf;

use clap::{ArgAction, ArgGroup, Args, Parser, Subcommand, ValueEnum};

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
    /// Read and write the runtime settings.
    Settings(SettingsArgs),
    /// Manage accounts, credentials, and library folders.
    Admin(AdminArgs),
    /// Install plugins, manage their lifecycle, and list them.
    Plugins(PluginsArgs),
    /// Configure provider instances and list them.
    Providers(ProvidersArgs),
    /// Assign provider instances to the layers they take part in.
    Layers(LayersArgs),
    /// List acquisitions and cancel them.
    Acquisitions(AcquisitionsArgs),
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
    /// Probe every file instead of trusting the stored size and modification
    /// time.
    #[arg(long)]
    pub full: bool,
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
    /// List the accounts.
    ListUsers,
    /// Change an account.
    UpdateUser(UpdateUserArgs),
    /// Create an API key for an account.
    CreateApiKey(CreateCredentialArgs),
    /// Create an app password for an account.
    CreateAppPassword(CreateCredentialArgs),
    /// Add a library folder, owned by a user or generic.
    AddFolder(AddFolderArgs),
    /// Watch a library folder for filesystem changes, or stop watching it.
    SetFolderWatch(SetFolderWatchArgs),
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
    /// The permissions to grant, as a comma-separated list of `acquire`,
    /// `upload` and `download`.
    ///
    /// An empty list grants none. Without the flag the account gets the
    /// `users.default_permissions` setting.
    #[arg(long, value_name = "LIST")]
    pub permissions: Option<String>,
    /// How many acquisitions the account may start per day.
    ///
    /// Without it the account has no acquisition limit.
    #[arg(long, value_name = "N")]
    pub acquisitions_per_day: Option<u32>,
    /// How many bytes the account's acquisitions may account for per day.
    ///
    /// Without it the account has no byte limit.
    #[arg(long, value_name = "N")]
    pub acquisition_bytes_per_day: Option<u64>,
    /// Read the password from standard input instead of prompting twice.
    ///
    /// One line is read, without its trailing newline, so a password may contain
    /// spaces.
    #[arg(long)]
    pub password_stdin: bool,
}

/// The arguments of `admin update-user`.
#[derive(Debug, Args)]
pub struct UpdateUserArgs {
    /// The login name of the account to change.
    #[arg(long, value_name = "USERNAME")]
    pub user: String,
    /// The new name shown to people.
    #[arg(long, value_name = "NAME")]
    pub display_name: Option<String>,
    /// Whether the account administers the server.
    #[arg(long, value_name = "true|false", action = ArgAction::Set)]
    pub admin: Option<bool>,
    /// The permissions to grant, as a comma-separated list of `acquire`,
    /// `upload` and `download`. An empty list grants none.
    #[arg(long, value_name = "LIST")]
    pub permissions: Option<String>,
    /// The new daily acquisition limit, or `none` for no limit.
    ///
    /// Without the flag the stored limit is kept.
    #[arg(long, value_name = "N|none")]
    pub acquisitions_per_day: Option<String>,
    /// The new daily byte limit, or `none` for no limit.
    ///
    /// Without the flag the stored limit is kept.
    #[arg(long, value_name = "N|none")]
    pub acquisition_bytes_per_day: Option<String>,
    /// Disable the account, which keeps its data but refuses its logins.
    #[arg(long, conflicts_with = "enable")]
    pub disable: bool,
    /// Enable an account that was disabled.
    #[arg(long)]
    pub enable: bool,
    /// Read a new password from standard input instead of prompting twice.
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
    /// Do not watch the folder for filesystem changes.
    #[arg(long)]
    pub no_watch: bool,
}

/// The arguments of `admin set-folder-watch`.
#[derive(Debug, Args)]
pub struct SetFolderWatchArgs {
    /// The library folder's id or path.
    #[arg(long, value_name = "PATH-OR-ID")]
    pub folder: String,
    /// Whether to watch the folder for filesystem changes.
    #[arg(long, value_name = "true|false", action = ArgAction::Set)]
    pub watch: bool,
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
    /// Install a plugin: from a `.hsplugin` archive, from an index, or from an
    /// index URL. It is installed with the permissions its manifest requests and
    /// enabled.
    Install(InstallPluginArgs),
    /// Apply a staged upgrade that requests new permissions.
    Approve(PluginIdArgs),
    /// Switch a plugin on.
    Enable(PluginIdArgs),
    /// Switch a plugin off.
    Disable(PluginIdArgs),
    /// Discard the staged upgrade waiting for review.
    DeclineUpgrade(PluginIdArgs),
    /// Remove a plugin, its rows, and its files.
    Remove(PluginIdArgs),
    /// List the installed plugins.
    List,
    /// List the plugins the reachable indexes offer.
    Available(AvailablePluginsArgs),
    /// Add, list, and remove the plugin index repositories.
    Repositories(RepositoriesArgs),
}

/// Which indexes a `plugins` command covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SourceSelection {
    /// Every index this server knows: the official index and the repositories
    /// an administrator registered.
    Index,
    /// The one index document at the URL the command was given.
    Url,
}

/// The arguments of `plugins install`.
#[derive(Debug, Args)]
pub struct InstallPluginArgs {
    /// The `.hsplugin` archive to install, or the plugin id when `--source`
    /// names an index.
    #[arg(value_name = "FILE-OR-ID")]
    pub target: String,
    /// Install from an index instead of from a file.
    ///
    /// `index` covers the official index and every repository an administrator
    /// registered; `url` covers the index document named by `--url`, which is
    /// registered as a repository when it is new.
    #[arg(long, value_name = "index|url", requires_if("url", "url"))]
    pub source: Option<SourceSelection>,
    /// The plugin version to install. Without it the newest version this
    /// server hosts is installed.
    #[arg(long, value_name = "V", requires = "source")]
    pub version: Option<String>,
    /// The URL of the index document to install from.
    #[arg(long, value_name = "INDEX-URL", requires = "source")]
    pub url: Option<String>,
    /// Record the plugin as shipped with the server rather than sideloaded.
    #[arg(long, conflicts_with = "source")]
    pub bundled: bool,
}

/// The arguments of `plugins available`.
#[derive(Debug, Args)]
pub struct AvailablePluginsArgs {
    /// The indexes to list. Defaults to `index`.
    #[arg(long, value_name = "index|url", requires_if("url", "url"))]
    pub source: Option<SourceSelection>,
    /// The URL of the index document to list.
    #[arg(long, value_name = "INDEX-URL", requires = "source")]
    pub url: Option<String>,
    /// List one registered repository, by URL or by `pr_` id.
    #[arg(long, value_name = "URL-OR-ID", conflicts_with_all = ["source", "url"])]
    pub repository: Option<String>,
}

/// The arguments of `plugins repositories`.
#[derive(Debug, Args)]
pub struct RepositoriesArgs {
    /// The repository command to run.
    #[command(subcommand)]
    pub command: RepositoriesCommand,
}

/// Every command under `plugins repositories`.
#[derive(Debug, Subcommand)]
pub enum RepositoriesCommand {
    /// Register a plugin index.
    Add(AddRepositoryArgs),
    /// List the registered plugin indexes.
    List,
    /// Remove a registered plugin index.
    Remove(RemoveRepositoryArgs),
}

/// The arguments of `plugins repositories add`.
#[derive(Debug, Args)]
pub struct AddRepositoryArgs {
    /// The URL of the repository's index document.
    #[arg(value_name = "URL")]
    pub url: String,
    /// The name to show for the repository.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// The minisign public key the repository signs its index with.
    ///
    /// Without it the index is read unsigned and its plugins are reported as
    /// unverified.
    #[arg(long, value_name = "MINISIGN-PUBLIC-KEY")]
    pub public_key: Option<String>,
}

/// The arguments of `plugins repositories remove`.
#[derive(Debug, Args)]
pub struct RemoveRepositoryArgs {
    /// The repository to remove, by URL or by `pr_` id.
    #[arg(value_name = "URL-OR-ID")]
    pub repository: String,
}

/// The arguments of a command that names one installed plugin.
#[derive(Debug, Args)]
pub struct PluginIdArgs {
    /// The id of the plugin, as its manifest writes it.
    #[arg(value_name = "ID")]
    pub id: String,
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

/// The arguments of `settings`.
#[derive(Debug, Args)]
pub struct SettingsArgs {
    /// The settings command to run.
    #[command(subcommand)]
    pub command: SettingsCommand,
}

/// Every command under `settings`.
#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    /// Print one setting's value and where it comes from.
    Get(SettingKeyArgs),
    /// Store one setting in the database.
    Set(SetSettingArgs),
    /// Remove one setting's stored value, falling back to the layers below it.
    Unset(SettingKeyArgs),
    /// List every setting with its value, source, and default.
    List,
}

/// The arguments of a command that names one setting.
#[derive(Debug, Args)]
pub struct SettingKeyArgs {
    /// The dotted setting key, for example `stream.pending.mode`.
    #[arg(value_name = "KEY")]
    pub key: String,
}

/// The arguments of `settings set`.
#[derive(Debug, Args)]
pub struct SetSettingArgs {
    /// The dotted setting key.
    #[arg(value_name = "KEY")]
    pub key: String,
    /// The new value, written as the setting's kind expects: `true` or `false`,
    /// a number, a string, or a comma-separated list of strings.
    #[arg(value_name = "VALUE")]
    pub value: String,
}

/// The arguments of `acquisitions`.
#[derive(Debug, Args)]
pub struct AcquisitionsArgs {
    /// The acquisition command to run.
    #[command(subcommand)]
    pub command: AcquisitionsCommand,
}

/// Every command under `acquisitions`.
#[derive(Debug, Subcommand)]
pub enum AcquisitionsCommand {
    /// List acquisitions, newest first.
    List(AcquisitionsListArgs),
    /// Cancel one acquisition.
    Cancel(AcquisitionsCancelArgs),
}

/// The arguments of `acquisitions list`.
#[derive(Debug, Args)]
pub struct AcquisitionsListArgs {
    /// Only list acquisitions in this state.
    ///
    /// The value is one of `queued`, `resolving`, `searching`, `downloading`,
    /// `verifying`, `importing`, `completed`, `failed` and `cancelled`.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,
    /// List the acquisitions that have already ended as well.
    ///
    /// Without it, only the acquisitions that still hold work are shown. A
    /// `--state` filter selects that state whether or not this is given.
    #[arg(long)]
    pub all: bool,
}

/// The arguments of `acquisitions cancel`.
#[derive(Debug, Args)]
pub struct AcquisitionsCancelArgs {
    /// The acquisition's client id, `ac_...`.
    #[arg(value_name = "AC_ID")]
    pub acquisition: String,
}
