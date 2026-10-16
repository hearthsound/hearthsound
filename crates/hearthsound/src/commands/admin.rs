//! `hearthsound admin`: accounts, credentials, and library folders.
//!
//! These are the first-run commands of a fresh installation: an operator creates
//! the first administrator, hands a client an API key or an app password, and
//! associates the first library folder with that administrator. They write the
//! database directly, so a running server picks the change up through its
//! `PRAGMA data_version` watcher.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};
use hearthsound_auth::{ApiKeys, AppPasswords, NewUser, Users};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_library::LibraryRepository;

use crate::cli::{
    AddFolderArgs, AdminArgs, AdminCommand, CreateCredentialArgs, CreateUserArgs,
    SetFolderWatchArgs,
};
use crate::commands::scan::resolve_folder;
use crate::context::Context;

/// Runs one `admin` command.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
/// - `env`: the process environment, keyed by variable name.
/// - `args`: the subcommand to run.
///
/// # Errors
///
/// Returns an error when the context cannot be opened or the command itself
/// fails, with a message naming what was refused.
pub async fn run(
    config: BootstrapConfig,
    env: &BTreeMap<String, String>,
    args: AdminArgs,
) -> anyhow::Result<()> {
    let context = Context::open(config, env).await?;
    match args.command {
        AdminCommand::CreateUser(args) => create_user(&context, args).await,
        AdminCommand::CreateApiKey(args) => create_api_key(&context, args).await,
        AdminCommand::CreateAppPassword(args) => create_app_password(&context, args).await,
        AdminCommand::AddFolder(args) => add_folder(&context, args).await,
        AdminCommand::SetFolderWatch(args) => set_folder_watch(&context, args).await,
    }
}

/// Creates an account.
///
/// # Parameters
///
/// - `context`: the open database and settings.
/// - `args`: the account's fields.
///
/// # Errors
///
/// Returns an error when no username was given and `--admin` was not, when the
/// password cannot be read or does not match its confirmation, or when the
/// account cannot be stored (an empty field, a taken username, a database
/// failure).
async fn create_user(context: &Context, args: CreateUserArgs) -> anyhow::Result<()> {
    let username = match (args.username, args.admin) {
        (Some(username), _) => username,
        (None, true) => "admin".to_owned(),
        (None, false) => bail!("a username is required unless --admin is given"),
    };
    let password = read_password(args.password_stdin)?;
    let new = NewUser::new(
        &username,
        args.display_name.unwrap_or_default(),
        password,
        args.admin,
    );
    let user = Users::new(context.database())
        .create(&context.settings().current(), &new)
        .await
        .with_context(|| format!("could not create the account `{username}`"))?;

    println!(
        "created {} account `{}` ({})",
        if user.is_admin {
            "administrator"
        } else {
            "standard"
        },
        user.username,
        user.id,
    );
    Ok(())
}

/// Creates an API key for an account.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `args`: the account and the key's name.
///
/// # Errors
///
/// Returns an error when no account has the login name or the key cannot be
/// stored.
async fn create_api_key(context: &Context, args: CreateCredentialArgs) -> anyhow::Result<()> {
    let user = find_user(context, &args.user).await?;
    let generated = ApiKeys::new(context.database())
        .generate(user.id, &args.name)
        .await
        .context("could not store the API key")?;

    println!("created API key `{}` for `{}`", args.name, user.username);
    println!("{}", generated.plaintext);
    println!();
    println!("Store it now: only its hash is kept, so it cannot be shown again.");
    Ok(())
}

/// Creates an app password for an account.
///
/// # Parameters
///
/// - `context`: the open database and secret box.
/// - `args`: the account and the app password's name.
///
/// # Errors
///
/// Returns an error when no account has the login name or the app password
/// cannot be sealed or stored.
async fn create_app_password(context: &Context, args: CreateCredentialArgs) -> anyhow::Result<()> {
    let user = find_user(context, &args.user).await?;
    let generated = AppPasswords::new(context.database(), context.secret_box().clone())
        .generate(user.id, &args.name)
        .await
        .context("could not store the app password")?;

    println!(
        "created app password `{}` for `{}`",
        args.name, user.username
    );
    println!("{}", generated.plaintext);
    println!();
    println!("Store it now: only its sealed form is kept, so it cannot be shown again.");
    Ok(())
}

/// Adds a library folder, owned by a user or generic.
///
/// The association is applied through the library layer, which pins the folder's
/// tracks to its owner (or leaves them unpinned for a generic folder) in the
/// same transaction as the ownership column, so the two never disagree.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `args`: the path, the ownership, and the optional display name.
///
/// # Errors
///
/// Returns an error when the path is not a directory, the named owner does not
/// exist, or the folder cannot be stored.
async fn add_folder(context: &Context, args: AddFolderArgs) -> anyhow::Result<()> {
    let owner = match &args.owner {
        Some(username) => Some(find_user(context, username).await?.id),
        None => None,
    };
    let repository = LibraryRepository::new(context.database());
    let folder = repository
        .add_library_folder(&args.path, owner)
        .await
        .with_context(|| format!("could not add {}", args.path.display()))?;
    repository
        .set_library_folder_owner(folder.id, owner)
        .await
        .context("could not associate the folder with its owner")?;
    if let Some(name) = &args.name {
        repository
            .rename_library_folder(folder.id, name)
            .await
            .context("could not rename the folder")?;
    }
    let watch = !args.no_watch;
    if !watch {
        repository
            .set_library_folder_watch(folder.id, false)
            .await
            .context("could not stop watching the folder")?;
    }

    let name = args.name.as_deref().unwrap_or(&folder.name);
    let watched = if watch { "watched" } else { "not watched" };
    match &args.owner {
        Some(owner) => println!(
            "added library folder `{name}` ({}) owned by `{owner}`, {watched}",
            folder.path.display()
        ),
        None => println!(
            "added library folder `{name}` ({}) as a generic folder, {watched}",
            folder.path.display()
        ),
    }
    Ok(())
}

/// Watches a library folder for filesystem changes, or stops watching it.
///
/// # Parameters
///
/// - `context`: the open database and settings.
/// - `args`: the folder to update and the flag to set.
///
/// # Errors
///
/// Returns an error when the selector names no known folder or the update
/// fails.
async fn set_folder_watch(context: &Context, args: SetFolderWatchArgs) -> anyhow::Result<()> {
    let repository = LibraryRepository::new(context.database());
    let folders = repository
        .list_library_folders()
        .await
        .context("could not list the library folders")?;
    let folder = resolve_folder(&folders, &args.folder)?;
    repository
        .set_library_folder_watch(folder.id, args.watch)
        .await
        .with_context(|| {
            format!(
                "could not update the watch flag of {}",
                folder.path.display()
            )
        })?;
    let state = if args.watch { "watches" } else { "ignores" };
    println!(
        "library folder `{}` ({}) now {state} filesystem changes",
        folder.name,
        folder.path.display()
    );
    Ok(())
}

/// Looks up an account by login name.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `username`: the login name to look for.
///
/// # Errors
///
/// Returns an error when no account has that login name or the lookup fails.
async fn find_user(context: &Context, username: &str) -> anyhow::Result<hearthsound_auth::User> {
    Users::new(context.database())
        .find_by_username(username)
        .await
        .context("could not look up the account")?
        .with_context(|| format!("no account is named `{username}`"))
}

/// Reads the new account's password.
///
/// # Parameters
///
/// - `from_stdin`: read one line from standard input instead of prompting on the
///   terminal twice.
///
/// # Returns
///
/// The password, without the line's trailing newline when it came from standard
/// input.
///
/// # Errors
///
/// Returns an error when the password is empty, the two prompted values differ,
/// or the terminal or standard input cannot be read.
fn read_password(from_stdin: bool) -> anyhow::Result<String> {
    if from_stdin {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
            .context("could not read the password from standard input")?;
        let password = line.trim_end_matches(['\n', '\r']).to_owned();
        if password.is_empty() {
            bail!("the password read from standard input is empty");
        }
        return Ok(password);
    }

    let password = rpassword::prompt_password("Password: ")
        .context("could not read the password from the terminal")?;
    if password.is_empty() {
        bail!("the password is empty");
    }
    let repeated = rpassword::prompt_password("Repeat password: ")
        .context("could not read the password from the terminal")?;
    if password != repeated {
        bail!("the two passwords do not match");
    }
    Ok(password)
}
