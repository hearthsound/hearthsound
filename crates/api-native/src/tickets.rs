//! Single-use tickets that let an event stream connect without a header.
//!
//! A browser's `EventSource` cannot attach an `Authorization` header, so a
//! client first asks for a ticket with its ordinary credential and then passes
//! the ticket in the query string of the stream request. A ticket is redeemable
//! once and only for [`TICKET_TTL_MS`] milliseconds. Only the SHA-256 of the
//! token is kept in memory, so a dump of the store reveals no usable ticket.
//!
//! An account may hold at most [`MAX_TICKETS_PER_ACCOUNT`] unredeemed tickets at
//! once. A browser holds one at a time - it asks for a fresh ticket when a
//! stream reconnects - so the cap is invisible in normal use while it bounds
//! what one signed-in client can make the server keep.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hearthsound_auth::Principal;
use rand::RngExt;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::views::now_millis;

/// How long a ticket stays redeemable, in milliseconds.
pub const TICKET_TTL_MS: i64 = 60_000;

/// How many unredeemed tickets one account may hold.
///
/// A normal client needs one: it redeems the ticket as it opens the stream and
/// asks again when the stream reconnects. A page that opens several streams at
/// once is not a supported pattern, and a loop is what the cap is for.
pub const MAX_TICKETS_PER_ACCOUNT: usize = 8;

/// Random bytes behind each ticket; encoded as 52 lowercase base32 characters.
const TICKET_BYTES: usize = 32;

/// A stored ticket: who it stands for and when it stops working.
#[derive(Debug, Clone)]
struct Entry {
    /// The principal the stream connection will act as.
    principal: Principal,
    /// The account the ticket belongs to, which the per-account cap counts.
    user_id: Uuid,
    /// The unix millisecond timestamp after which the ticket is refused.
    expires_at: i64,
}

/// An account already holds the most tickets it may.
///
/// The caller answers 429 and tells the client how long the oldest ticket still
/// has to live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the account already holds the maximum number of stream tickets")]
pub struct TicketLimit {
    /// How long until the oldest outstanding ticket expires, at least one
    /// second.
    pub retry_after: Duration,
}

/// The in-memory store of outstanding stream tickets.
///
/// The stores's principal is a snapshot taken when the ticket was issued. The
/// stream re-checks it at redemption and every few seconds afterwards
/// ([`crate::auth::reauthorize`]), so a ticket issued moments before a logout or
/// a revocation cannot open an administrator stream.
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
    ///
    /// # Errors
    ///
    /// Returns [`TicketLimit`] when the account already holds
    /// [`MAX_TICKETS_PER_ACCOUNT`] unredeemed tickets; its `retry_after` is how
    /// long the oldest of them still has to live.
    pub fn issue(&self, principal: Principal) -> Result<(String, i64), TicketLimit> {
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
    ///
    /// # Errors
    ///
    /// Returns [`TicketLimit`] when the account's outstanding tickets are at
    /// the cap, naming how long the oldest still has to live.
    fn issue_at(&self, principal: Principal, now: i64) -> Result<(String, i64), TicketLimit> {
        let user_id = principal.user_id.uuid();
        let token = new_token();
        let expires_at = now.saturating_add(TICKET_TTL_MS);
        let mut state = self.lock();
        state.retain(|_, entry| entry.expires_at > now);
        let outstanding = state.values().filter(|entry| entry.user_id == user_id);
        let mut count = 0usize;
        let mut oldest = i64::MAX;
        for entry in outstanding {
            count += 1;
            oldest = oldest.min(entry.expires_at);
        }
        if count >= MAX_TICKETS_PER_ACCOUNT {
            // The oldest expiry is the soonest the cap frees up; `at least one
            // second` keeps `Retry-After` honest for a ticket about to expire.
            let wait_ms = oldest.saturating_sub(now).max(1_000);
            return Err(TicketLimit {
                retry_after: Duration::from_millis(u64::try_from(wait_ms).unwrap_or(u64::MAX)),
            });
        }
        state.insert(
            key_of(&token),
            Entry {
                principal,
                user_id,
                expires_at,
            },
        );
        Ok((token, expires_at))
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

    use super::{MAX_TICKETS_PER_ACCOUNT, TICKET_TTL_MS, Tickets};
    use std::time::Duration;

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
        let (token, _) = tickets.issue_at(issued.clone(), 0).unwrap();
        assert_eq!(tickets.redeem_at(&token, 1), Some(issued));
        assert_eq!(tickets.redeem_at(&token, 2), None);
    }

    #[test]
    fn a_ticket_is_refused_at_and_after_its_expiry() {
        let tickets = Tickets::default();
        let (early, expiry) = tickets.issue_at(principal(), 0).unwrap();
        assert_eq!(expiry, TICKET_TTL_MS);
        assert!(tickets.redeem_at(&early, TICKET_TTL_MS - 1).is_some());

        let (late, _) = tickets.issue_at(principal(), 0).unwrap();
        assert!(tickets.redeem_at(&late, TICKET_TTL_MS).is_none());
    }

    #[test]
    fn an_unknown_ticket_is_refused() {
        let tickets = Tickets::default();
        tickets.issue_at(principal(), 0).unwrap();
        assert!(tickets.redeem_at("not-issued", 0).is_none());
    }

    #[test]
    fn issuing_purges_expired_entries() {
        let tickets = Tickets::default();
        tickets.issue_at(principal(), 0).unwrap();
        tickets.issue_at(principal(), TICKET_TTL_MS).unwrap();
        assert_eq!(tickets.lock().len(), 1);
    }

    #[test]
    fn tokens_are_fresh_lowercase_base32() {
        let tickets = Tickets::default();
        let (first, _) = tickets.issue_at(principal(), 0).unwrap();
        let (second, _) = tickets.issue_at(principal(), 0).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 52);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn an_account_cannot_hold_more_than_the_cap() {
        let tickets = Tickets::default();
        let listener = principal();
        let mut issued = Vec::new();
        for _ in 0..MAX_TICKETS_PER_ACCOUNT {
            issued.push(tickets.issue_at(listener.clone(), 0).unwrap().0);
        }

        let limited = tickets.issue_at(listener.clone(), 0).unwrap_err();
        assert_eq!(
            limited.retry_after,
            Duration::from_millis(u64::try_from(TICKET_TTL_MS).unwrap()),
            "the wait is until the oldest ticket expires"
        );

        // Redeeming one frees a slot, so the next issue succeeds again.
        assert!(tickets.redeem_at(&issued[0], 1).is_some());
        assert!(tickets.issue_at(listener, 1).is_ok());
    }

    #[test]
    fn the_cap_counts_one_account_at_a_time() {
        let tickets = Tickets::default();
        for _ in 0..MAX_TICKETS_PER_ACCOUNT {
            tickets.issue_at(principal(), 0).unwrap();
        }
        // Another account has its own budget.
        assert!(tickets.issue_at(principal(), 0).is_ok());
    }

    #[test]
    fn the_cap_frees_up_when_tickets_expire() {
        let tickets = Tickets::default();
        for _ in 0..MAX_TICKETS_PER_ACCOUNT {
            tickets.issue_at(principal(), 0).unwrap();
        }
        // Purging runs on issue: at the expiry instant the old tickets are gone.
        assert!(tickets.issue_at(principal(), TICKET_TTL_MS).is_ok());
    }
}
