//! The adapter from the account-link plugin interface to the core provider
//! traits.
//!
//! This module is the only place where the account-link world's WIT records
//! become core types and core types become WIT records. Everything above it sees
//! [`AccountLinkProvider`] and its core types; everything below it sees
//! generated bindings.

use std::convert::Infallible;

use async_trait::async_trait;
use hearthsound_core::providers::{
    AccountLinkProvider, LinkMethod, LinkedAccount, ProviderLifecycle, UserContext,
};
use hearthsound_core::{ProviderError, Slug};

use crate::api::v5::bindings;
use crate::api::v5::bindings::account_link::exports::hearthsound::plugin::account_link;
use crate::api::v5::bindings::hearthsound::plugin::types::{
    ProviderError as WitProviderError, UserContext as WitUserContext,
};
use crate::error::map_provider_error;
use crate::pool::StorePool;

/// An account-link provider backed by one plugin instance.
pub(crate) struct WasmAccountLinkProvider {
    pool: StorePool<bindings::AccountLinkProvider>,
    plugin: String,
    slug: Slug,
}

impl WasmAccountLinkProvider {
    /// Wraps a store pool as an account-link provider.
    ///
    /// # Parameters
    ///
    /// - `pool`: the instance's store pool.
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug.
    pub(crate) fn new(
        pool: StorePool<bindings::AccountLinkProvider>,
        plugin: String,
        slug: Slug,
    ) -> WasmAccountLinkProvider {
        WasmAccountLinkProvider { pool, plugin, slug }
    }

    /// Maps a guest error to the core error, logging the guest's message.
    fn map_error(&self, error: WitProviderError) -> ProviderError {
        map_provider_error(&self.plugin, &self.slug, error.into())
    }
}

#[async_trait]
impl ProviderLifecycle for WasmAccountLinkProvider {
    async fn validate_config(&self, server_config: &str) -> Result<Vec<String>, ProviderError> {
        let server_config = server_config.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_lifecycle()
                        .call_validate_config(store, &server_config)
                        .await
                })
            })
            .await?;

        Ok(match outcome {
            Ok(()) => Vec::new(),
            Err(problems) => problems,
        })
    }

    async fn health(&self) -> Result<String, ProviderError> {
        let outcome = self
            .pool
            .call(|store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_lifecycle()
                        .call_health(store)
                        .await
                })
            })
            .await?;

        outcome.map_err(|error| self.map_error(error))
    }
}

#[async_trait]
impl AccountLinkProvider for WasmAccountLinkProvider {
    async fn get_link_method(&self) -> Result<LinkMethod, ProviderError> {
        let outcome = self
            .pool
            .call(|store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_account_link()
                        .call_get_link_method(store)
                        .await
                        .map(Ok::<_, Infallible>)
                })
            })
            .await?;

        Ok(match outcome.map_err(|never| match never {})? {
            account_link::LinkMethod::Config => LinkMethod::Config,
            account_link::LinkMethod::Browser => LinkMethod::Browser,
        })
    }

    async fn verify_account(&self, user: &UserContext) -> Result<LinkedAccount, ProviderError> {
        let user = to_wit_user(user);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_account_link()
                        .call_verify_account(store, &user)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_account)
            .map_err(|error| self.map_error(error))
    }

    async fn begin_link(
        &self,
        user: &UserContext,
        callback_url: &str,
    ) -> Result<String, ProviderError> {
        let user = to_wit_user(user);
        let callback_url = callback_url.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_account_link()
                        .call_begin_link(store, &user, &callback_url)
                        .await
                })
            })
            .await?;

        outcome.map_err(|error| self.map_error(error))
    }

    async fn complete_link(
        &self,
        user: &UserContext,
        query: &[(String, String)],
    ) -> Result<LinkedAccount, ProviderError> {
        let user = to_wit_user(user);
        let query = query.to_vec();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_account_link()
                        .call_complete_link(store, &user, &query)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_account)
            .map_err(|error| self.map_error(error))
    }
}

/// Converts a core user context into the plugin's record.
///
/// # Parameters
///
/// - `user`: the listener the call is made for.
fn to_wit_user(user: &UserContext) -> WitUserContext {
    WitUserContext {
        user_id: user.user_id.clone(),
        config: user.config.clone(),
    }
}

/// Converts the plugin's account into a core linked account.
///
/// # Parameters
///
/// - `account`: the account the plugin named.
fn to_core_account(account: account_link::Account) -> LinkedAccount {
    LinkedAccount {
        name: account.name,
        config: account.config,
    }
}
