//! Single-use tickets that let an event stream connect without a header.
//!
//! A browser's `EventSource` cannot attach an `Authorization` header, so a
//! client first asks for a ticket with its ordinary credential and then passes
//! the ticket in the query string of the stream request. A ticket is redeemable
//! once and only for [`TICKET_TTL_MS`] milliseconds. Only the SHA-256 of the
//! token is kept in memory, so a dump of the store reveals no usable ticket.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use hearthsound_auth::Principal;
use rand::RngExt;
use sha2::{Digest, Sha256};

use crate::views::now_millis;

/// How long a ticket stays redeemable, in milliseconds.
pub const TICKET_TTL_MS: i64 = 60_000;

/// Random bytes behind each ticket; encoded as 52 lowercase base32 characters.
const TICKET_BYTES: usize = 32;

/// A stored ticket: who it stands for and when it stops working.
#[derive(Debug, Clone)]
struct Entry {
    /// The principal the stream connection will act as.
    principal: Principal,
    /// The unix millisecond timestamp after which the ticket is refused.
    expires_at: i64,
}

/// The in-memory store of outstanding stream tickets.
///
/// Cloning the store shares its contents, so every clone of the native state
/// sees the same tickets.
#[derive(Debug, Clone, Default)]
pub struct Tickets {
    /// Entries keyed by the SHA-256 digest of the ticket token.
    inner: Arc<Mutex<HashMap<[u8; 32], Entry>>>,
}

impl Tickets {
    /// Issues a ticket for a principal, valid for [`TICKET_TTL_MS`] from now.
    ///
    /// Expired tickets are purged while the new one is inserted, so the store
    /// never grows past the tickets issued within one time-to-live window.
    ///
    /// # Parameters
    ///
    /// - `principal`: the caller the ticket authenticates as.
    ///
    /// # Returns
    ///
    /// The ticket token (52 lowercase base32 characters) and its expiry as a
    /// unix millisecond timestamp.
    pub fn issue(&self, principal: Principal) -> (String, i64) {
        self.issue_at(principal, now_millis())
    }

    /// Redeems a ticket, consuming it whether or not it is still valid.
    ///
    /// # Parameters
    ///
    /// - `token`: the ticket token the client presented.
    ///
    /// # Returns
    ///
    /// The principal the ticket was issued to, or `None` when the token is
    /// unknown, already redeemed, or expired.
    pub fn redeem(&self, token: &str) -> Option<Principal> {
        self.redeem_at(token, now_millis())
    }

    /// Issues a ticket as of `now`; split out so tests can inject the clock.
    ///
    /// # Parameters
    ///
    /// - `principal`: the caller the ticket authenticates as.
    /// - `now`: the current unix millisecond timestamp.
    ///
    /// # Returns
    ///
    /// The ticket token and its expiry, as [`Tickets::issue`] reports them.
    fn issue_at(&self, principal: Principal, now: i64) -> (String, i64) {
        let token = new_token();
        let expires_at = now.saturating_add(TICKET_TTL_MS);
        let mut state = self.lock();
        state.retain(|_, entry| entry.expires_at > now);
        state.insert(
            key_of(&token),
            Entry {
                principal,
                expires_at,
            },
        );
        (token, expires_at)
    }

    /// Redeems a ticket as of `now`; split out so tests can inject the clock.
    ///
    /// # Parameters
    ///
    /// - `token`: the ticket token the client presented.
    /// - `now`: the current unix millisecond timestamp.
    ///
    /// # Returns
    ///
    /// The principal when the ticket exists and has not expired, else `None`.
    fn redeem_at(&self, token: &str, now: i64) -> Option<Principal> {
        let entry = self.lock().remove(&key_of(token))?;
        (entry.expires_at > now).then_some(entry.principal)
    }

    /// Locks the store, recovering the data from a poisoned lock.
    ///
    /// The map is only ever mutated by whole-entry inserts and removals, so a
    /// panic in another thread cannot leave it half-updated.
    fn lock(&self) -> MutexGuard<'_, HashMap<[u8; 32], Entry>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Returns a fresh random ticket token.
///
/// # Returns
///
/// [`TICKET_BYTES`] random bytes as lowercase RFC 4648 base32 without padding.
fn new_token() -> String {
    let mut bytes = [0u8; TICKET_BYTES];
    rand::rng().fill(&mut bytes[..]);
    data_encoding::BASE32_NOPAD
        .encode(&bytes)
        .to_ascii_lowercase()
}

/// Derives the map key for a token: the SHA-256 digest of its bytes.
///
/// # Parameters
///
/// - `token`: the ticket token, issued or presented.
///
/// # Returns
///
/// The 32-byte digest used as the store key.
fn key_of(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use hearthsound_auth::{Credential, Principal};
    use hearthsound_core::{EntityId, IdPrefix};
    use uuid::Uuid;

    use super::{TICKET_TTL_MS, Tickets};

    /// A non-admin principal with a fresh user id.
    fn principal() -> Principal {
        Principal {
            user_id: EntityId::new(IdPrefix::User),
            username: "listener".to_owned(),
            is_admin: false,
            can_acquire: false,
            can_upload: false,
            can_download: true,
            credential: Credential::Session { id: Uuid::nil() },
        }
    }

    #[test]
    fn a_ticket_redeems_once_for_its_principal() {
        let tickets = Tickets::default();
        let issued = principal();
        let (token, _) = tickets.issue_at(issued.clone(), 0);
        assert_eq!(tickets.redeem_at(&token, 1), Some(issued));
        assert_eq!(tickets.redeem_at(&token, 2), None);
    }

    #[test]
    fn a_ticket_is_refused_at_and_after_its_expiry() {
        let tickets = Tickets::default();
        let (early, expiry) = tickets.issue_at(principal(), 0);
        assert_eq!(expiry, TICKET_TTL_MS);
        assert!(tickets.redeem_at(&early, TICKET_TTL_MS - 1).is_some());

        let (late, _) = tickets.issue_at(principal(), 0);
        assert!(tickets.redeem_at(&late, TICKET_TTL_MS).is_none());
    }

    #[test]
    fn an_unknown_ticket_is_refused() {
        let tickets = Tickets::default();
        tickets.issue_at(principal(), 0);
        assert!(tickets.redeem_at("not-issued", 0).is_none());
    }

    #[test]
    fn issuing_purges_expired_entries() {
        let tickets = Tickets::default();
        tickets.issue_at(principal(), 0);
        tickets.issue_at(principal(), TICKET_TTL_MS);
        assert_eq!(tickets.lock().len(), 1);
    }

    #[test]
    fn tokens_are_fresh_lowercase_base32() {
        let tickets = Tickets::default();
        let (first, _) = tickets.issue_at(principal(), 0);
        let (second, _) = tickets.issue_at(principal(), 0);
        assert_ne!(first, second);
        assert_eq!(first.len(), 52);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }
}
