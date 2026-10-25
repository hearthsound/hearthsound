//! `hearthsound admin`: accounts, credentials, and library folders.
//!
//! These are the first-run commands of a fresh installation: an operator creates
//! the first administrator, hands a client an API key or an app password, and
//! associates the first library folder with that administrator. They write the
//! database directly, so a running server picks the change up through its
//! `PRAGMA data_version` watcher.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};
use hearthsound_auth::{
    ApiKeys, AppPasswords, NewUser, Permission, Quotas, UserPermissions, UserQuota, UserUpdate,
    Users,
};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_library::LibraryRepository;
use hearthsound_library::folders::check_folder_path;

use crate::cli::{
    AddFolderArgs, AdminArgs, AdminCommand, CreateCredentialArgs, CreateUserArgs,
    SetFolderWatchArgs, UpdateUserArgs,
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
        AdminCommand::ListUsers => list_users(&context).await,
        AdminCommand::UpdateUser(args) => update_user(&context, args).await,
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
/// Returns an error when no username was given and `--admin` was not, when a
/// permission name is unknown, when the password cannot be read or does not
/// match its confirmation, or when the account or its quota cannot be stored
/// (an empty field, a taken username, a database failure).
async fn create_user(context: &Context, args: CreateUserArgs) -> anyhow::Result<()> {
    let username = match (args.username, args.admin) {
        (Some(username), _) => username,
        (None, true) => "admin".to_owned(),
        (None, false) => bail!("a username is required unless --admin is given"),
    };
    let password = read_password(args.password_stdin)?;
    let mut new = NewUser::new(
        &username,
        args.display_name.unwrap_or_default(),
        password,
        args.admin,
    );
    if let Some(list) = &args.permissions {
        new = new.with_permissions(parse_permissions(list)?);
    }
    let user = Users::new(context.database())
        .create(&context.settings().current(), &new)
        .await
        .with_context(|| format!("could not create the account `{username}`"))?;

    let quota = UserQuota {
        acquisitions_per_day: args.acquisitions_per_day,
        acquisition_bytes_per_day: args.acquisition_bytes_per_day,
    };
    if !quota.is_unlimited() {
        Quotas::new(context.database())
            .set(user.id, &quota)
            .await
            .with_context(|| format!("could not store the quota of `{username}`"))?;
    }

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
    println!("{}", describe_account(&user, &quota));
    Ok(())
}

/// Lists every account with its permissions, quota, and state.
///
/// # Parameters
///
/// - `context`: the open database.
///
/// # Errors
///
/// Returns an error when the accounts or a quota cannot be read.
async fn list_users(context: &Context) -> anyhow::Result<()> {
    let users = Users::new(context.database());
    let quotas = Quotas::new(context.database());
    let accounts = users.list().await.context("could not list the accounts")?;

    for user in &accounts {
        let quota = quotas
            .get(user.id)
            .await
            .with_context(|| format!("could not read the quota of `{}`", user.username))?;
        println!("{}", describe_account(user, &quota));
    }
    println!("{} account(s)", accounts.len());
    Ok(())
}

/// Changes an account: its name, admin flag, permissions, quota, password, and
/// enabled state.
///
/// The changes that are asked for are applied and the rest of the account is
/// left as it is. Quotas are read, changed, and written back as a whole, so
/// clearing one dimension leaves the other alone.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `args`: the account and the changes.
///
/// # Errors
///
/// Returns an error when the account does not exist, when a permission name or
/// a quota is not one the flags accept, when the password cannot be read or is
/// shorter than 8 characters, when a write fails, or when disabling or demoting
/// the account would leave no active administrator (the layer refuses it and
/// nothing is written).
async fn update_user(context: &Context, args: UpdateUserArgs) -> anyhow::Result<()> {
    let users = Users::new(context.database());
    let quotas = Quotas::new(context.database());
    let user = find_user(context, &args.user).await?;

    let mut update = UserUpdate::default();
    if let Some(display_name) = args.display_name {
        update = update.with_display_name(display_name);
    }
    if let Some(is_admin) = args.admin {
        update = update.with_is_admin(is_admin);
    }
    if let Some(list) = &args.permissions {
        update = update.with_permissions(parse_permissions(list)?);
    }
    let user = users
        .update(user.id, &update)
        .await
        .with_context(|| format!("could not update the account `{}`", args.user))?;

    if args.disable {
        users
            .disable(user.id)
            .await
            .context("could not disable the account")?;
    }
    if args.enable {
        users
            .enable(user.id)
            .await
            .context("could not enable the account")?;
    }
    if args.password_stdin {
        let password = read_password(true)?;
        let revoked = users
            .set_password(user.id, &password, None)
            .await
            .context("could not store the new password")?;
        // The command line has no session, so every credential of the account
        // goes: another session, an API key or an app password would otherwise
        // keep working with the password the operator just replaced.
        println!(
            "revoked {} session(s), {} API key(s) and {} app password(s)",
            revoked.sessions, revoked.api_keys, revoked.app_passwords
        );
    }

    let mut quota = quotas
        .get(user.id)
        .await
        .context("could not read the stored quota")?;
    if let Some(limit) = &args.acquisitions_per_day {
        quota.acquisitions_per_day = parse_limit(limit)?;
    }
    if let Some(limit) = &args.acquisition_bytes_per_day {
        quota.acquisition_bytes_per_day = parse_limit(limit)?;
    }
    quotas
        .set(user.id, &quota)
        .await
        .context("could not store the quota")?;

    let stored = users
        .find_by_id(user.id)
        .await
        .context("could not read the account back")?
        .with_context(|| format!("no account is named `{}`", args.user))?;
    println!("updated account `{}`", stored.username);
    println!("{}", describe_account(&stored, &quota));
    Ok(())
}

/// Parses a comma-separated permission list.
///
/// # Parameters
///
/// - `list`: the names to grant, in any order. An empty string grants none.
///
/// # Returns
///
/// The flags the names ask for.
///
/// # Errors
///
/// Returns an error naming the first name that is not `acquire`, `upload` or
/// `download`.
fn parse_permissions(list: &str) -> anyhow::Result<UserPermissions> {
    let mut names = Vec::new();
    for name in list
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let permission = name
            .parse::<Permission>()
            .map_err(|_| anyhow::anyhow!("`{name}` is not one of acquire, upload, download"))?;
        names.push(permission.as_str().to_owned());
    }
    Ok(UserPermissions::from_names(&names))
}

/// Parses one quota flag: a number, or `none` for no limit.
///
/// # Parameters
///
/// - `value`: the flag's value.
///
/// # Returns
///
/// The limit, or `None` when the value asks for no limit.
///
/// # Errors
///
/// Returns an error when the value is neither `none` nor a number the type
/// holds.
fn parse_limit<T>(value: &str) -> anyhow::Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    if value.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    value
        .parse::<T>()
        .map(Some)
        .map_err(|error| anyhow::anyhow!("`{value}` is not a number or `none`: {error}"))
}

/// Describes an account as one line of `key=value` fields.
///
/// # Parameters
///
/// - `user`: the account.
/// - `quota`: the account's acquisition quota.
///
/// # Returns
///
/// The line `admin list-users` prints and the commands that change an account
/// report, with the username, the admin flag, the permissions, both quota
/// dimensions, and the enabled state.
fn describe_account(user: &hearthsound_auth::User, quota: &UserQuota) -> String {
    let granted: Vec<&str> = Permission::ALL
        .into_iter()
        .filter(|permission| user.permissions().allows(*permission))
        .map(Permission::as_str)
        .collect();
    let permissions = if granted.is_empty() {
        "none".to_owned()
    } else {
        granted.join(",")
    };
    let limit = |value: Option<u64>| match value {
        Some(value) => value.to_string(),
        None => "unlimited".to_owned(),
    };
    format!(
        "username={} admin={} permissions={} acquisitions-per-day={} \
         acquisition-bytes-per-day={} state={}",
        user.username,
        if user.is_admin { "yes" } else { "no" },
        permissions,
        limit(quota.acquisitions_per_day.map(u64::from)),
        limit(quota.acquisition_bytes_per_day),
        if user.is_disabled() {
            "disabled"
        } else {
            "enabled"
        },
    )
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
    // The name rule is the crate's, so a 100 KB name is refused here exactly as
    // it is over the API, and the reported name is the trimmed one that was
    // stored.
    let name = hearthsound_auth::validate_credential_name(&args.name)
        .context("could not use the API key name")?;
    let generated = ApiKeys::new(context.database())
        .generate(user.id, name)
        .await
        .context("could not store the API key")?;

    println!("created API key `{name}` for `{}`", user.username);
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
    let name = hearthsound_auth::validate_credential_name(&args.name)
        .context("could not use the app password name")?;
    let generated = AppPasswords::new(context.database(), context.secret_box().clone())
        .generate(user.id, name)
        .await
        .context("could not store the app password")?;

    println!("created app password `{name}` for `{}`", user.username);
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
    let path = check_folder_path(&args.path, &context.server_paths())
        .map_err(|rejection| anyhow::anyhow!(rejection))?;
    let repository = LibraryRepository::new(context.database());
    let folder = repository
        .add_library_folder(&path, owner)
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
/// Returns an error when the password is empty, shorter than the eight
/// characters every password write keeps, the two prompted values differ, or the
/// terminal or standard input cannot be read.
fn read_password(from_stdin: bool) -> anyhow::Result<String> {
    let password = if from_stdin {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
            .context("could not read the password from standard input")?;
        let password = line.trim_end_matches(['\n', '\r']).to_owned();
        if password.is_empty() {
            bail!("the password read from standard input is empty");
        }
        password
    } else {
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
        password
    };
    // The rule every password write keeps, checked before anything is written,
    // so the operator learns it from this command rather than from a refusal
    // after the rest of the command has already run.
    if let Err(hearthsound_auth::AuthError::InvalidPassword { .. }) =
        hearthsound_auth::validate_password(&password)
    {
        bail!("the password is at least 8 characters long");
    }
    Ok(password)
}
