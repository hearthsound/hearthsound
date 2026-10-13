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
