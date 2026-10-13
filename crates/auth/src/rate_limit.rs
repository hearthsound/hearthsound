//! Login rate limiting.
//!
//! A Subsonic login that presents a username and a secret is limited to
//! `auth.login_rate_limit_per_minute` attempts per minute, counted against the
//! client's address and against the submitted username separately. Both keys are
//! charged on every attempt, so neither a single address walking a list of
//! usernames nor a single account attacked from many addresses can exceed the
//! limit, and a failed attempt counts exactly like a successful one.
//!
//! The limit is enforced with `governor`'s generic cell rate algorithm over a
//! keyed state store. The clock is a type parameter, which lets a test drive a
//! `FakeRelativeClock` instead of waiting a minute for the bucket to refill.

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use governor::clock::{Clock, DefaultClock};
use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};

/// What one login attempt is counted against.
///
/// The lowercase username is used, so `Echo` and `echo` share one budget - the
/// same rule the unique index on `users.username` enforces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LoginKey {
    /// The address the request came from.
    Ip(IpAddr),
    /// The username the request presented, lowercased.
    Username(String),
}

/// A login attempt was refused because a limit was reached.
///
/// The Subsonic layer reports this as error 40, because the base protocol has no
/// dedicated code for a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the login rate limit was exceeded")]
pub struct RateLimited;

/// The keyed limiter type the clock and the no-op middleware resolve to.
///
/// The middleware type must name the clock's own instant, which is why it cannot
/// be left at its default.
type Limiter<C> = RateLimiter<
    LoginKey,
    DefaultKeyedStateStore<LoginKey>,
    C,
    NoOpMiddleware<<C as Clock>::Instant>,
>;

/// A login rate limiter keyed by address and by username.
///
/// The clock is a type parameter so a test can advance a `FakeRelativeClock`
/// rather than sleep. [`LoginRateLimiter::new`] builds one over the real clock.
///
/// # Examples
///
/// ```
/// use std::net::{IpAddr, Ipv4Addr};
///
/// use hearthsound_auth::LoginRateLimiter;
///
/// let limiter = LoginRateLimiter::new(2);
/// let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// assert!(limiter.check(ip, "echo").is_ok());
/// assert!(limiter.check(ip, "echo").is_ok());
/// assert!(limiter.check(ip, "echo").is_err());
/// ```
pub struct LoginRateLimiter<C: Clock + Clone = DefaultClock> {
    /// The limiter itself, replaced when the rate changes.
    limiter: Mutex<Limiter<C>>,
    /// The rate the limiter currently enforces, per minute.
    ///
    /// Atomic so [`LoginRateLimiter::per_minute`] needs no lock; it is only ever
    /// written while the limiter lock is held.
    per_minute: AtomicU32,
    /// The clock every rebuilt limiter keeps using.
    clock: C,
}

impl LoginRateLimiter<DefaultClock> {
    /// Builds a limiter over the real clock.
    ///
    /// # Parameters
    ///
    /// - `per_minute`: the number of attempts allowed per minute. Zero is
    ///   treated as one, because a limiter that admits nothing could never
    ///   recover.
    ///
    /// # Returns
    ///
    /// The limiter.
    pub fn new(per_minute: u32) -> LoginRateLimiter<DefaultClock> {
        LoginRateLimiter::with_clock(per_minute, DefaultClock::default())
    }
}

impl<C: Clock + Clone> LoginRateLimiter<C> {
    /// Builds a limiter over a given clock.
    ///
    /// # Parameters
    ///
    /// - `per_minute`: the number of attempts allowed per minute; zero is
    ///   treated as one.
    /// - `clock`: the clock the limiter reads.
    ///
    /// # Returns
    ///
    /// The limiter.
    pub fn with_clock(per_minute: u32, clock: C) -> LoginRateLimiter<C> {
        LoginRateLimiter {
            limiter: Mutex::new(build(per_minute, clock.clone())),
            per_minute: AtomicU32::new(per_minute.max(1)),
            clock,
        }
    }

    /// Charges one attempt against an address and a username.
    ///
    /// # Parameters
    ///
    /// - `ip`: the address the request came from.
    /// - `username`: the submitted username, counted case-insensitively.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the attempt may proceed.
    ///
    /// # Errors
    ///
    /// Returns [`RateLimited`] when either key has spent its budget for the
    /// current minute.
    pub fn check(&self, ip: IpAddr, username: &str) -> Result<(), RateLimited> {
        let limiter = self.lock();
        let address_allowed = limiter.check_key(&LoginKey::Ip(ip)).is_ok();
        let username_allowed = limiter
            .check_key(&LoginKey::Username(username.to_ascii_lowercase()))
            .is_ok();
        if address_allowed && username_allowed {
            Ok(())
        } else {
            Err(RateLimited)
        }
    }

    /// Changes the rate, resetting the counters when it actually changes.
    ///
    /// # Parameters
    ///
    /// - `per_minute`: the new number of attempts allowed per minute; zero is
    ///   treated as one.
    pub fn set_per_minute(&self, per_minute: u32) {
        let per_minute = per_minute.max(1);
        if self.per_minute.load(Ordering::Relaxed) == per_minute {
            return;
        }
        let mut limiter = self.lock();
        if self.per_minute.load(Ordering::Relaxed) == per_minute {
            return;
        }
        *limiter = build(per_minute, self.clock.clone());
        self.per_minute.store(per_minute, Ordering::Relaxed);
    }

    /// Returns the rate currently enforced, per minute.
    pub fn per_minute(&self) -> u32 {
        self.per_minute.load(Ordering::Relaxed)
    }

    /// Locks the limiter, recovering from a poison rather than panicking.
    ///
    /// A panic while the lock is held would otherwise make every later login
    /// fail, so the state is taken back instead.
    fn lock(&self) -> std::sync::MutexGuard<'_, Limiter<C>> {
        match self.limiter.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Builds a keyed limiter for a rate.
///
/// # Parameters
///
/// - `per_minute`: the number of attempts allowed per minute; zero is treated
///   as one.
/// - `clock`: the clock the limiter reads.
///
/// # Returns
///
/// The limiter.
fn build<C: Clock + Clone>(per_minute: u32, clock: C) -> Limiter<C> {
    let per_minute = NonZeroU32::new(per_minute.max(1)).unwrap_or(NonZeroU32::MIN);
    RateLimiter::dashmap_with_clock(Quota::per_minute(per_minute), clock)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use governor::clock::FakeRelativeClock;

    use super::*;

    #[test]
    fn limits_an_address_after_the_budget_is_spent() {
        let limiter = LoginRateLimiter::new(2);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        assert_eq!(limiter.check(ip, "echo"), Ok(()));
        assert_eq!(limiter.check(ip, "echo"), Ok(()));
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));
    }

    #[test]
    fn limits_a_username_across_addresses() {
        let limiter = LoginRateLimiter::new(2);
        let first = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let second = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        assert_eq!(limiter.check(first, "echo"), Ok(()));
        assert_eq!(limiter.check(first, "other"), Ok(()));
        assert_eq!(limiter.check(second, "echo"), Ok(()));
        // The first address has spent its two attempts.
        assert_eq!(limiter.check(first, "third"), Err(RateLimited));
        // The username has spent its two attempts.
        assert_eq!(limiter.check(second, "echo"), Err(RateLimited));
    }

    #[test]
    fn counts_usernames_case_insensitively() {
        let limiter = LoginRateLimiter::new(1);
        let first = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let second = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        assert_eq!(limiter.check(first, "Echo"), Ok(()));
        assert_eq!(limiter.check(second, "echo"), Err(RateLimited));
    }

    #[test]
    fn a_fake_clock_advances_the_window() {
        let clock = FakeRelativeClock::default();
        let limiter = LoginRateLimiter::with_clock(1, clock.clone());
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        assert_eq!(limiter.check(ip, "echo"), Ok(()));
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));

        clock.advance(Duration::from_secs(60));

        assert_eq!(limiter.check(ip, "echo"), Ok(()));
    }

    #[test]
    fn changing_the_rate_resets_the_counters() {
        let limiter = LoginRateLimiter::new(1);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        assert_eq!(limiter.check(ip, "echo"), Ok(()));
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));

        limiter.set_per_minute(2);
        assert_eq!(limiter.per_minute(), 2);
        assert_eq!(limiter.check(ip, "echo"), Ok(()));
    }

    #[test]
    fn zero_is_treated_as_one_attempt() {
        let limiter = LoginRateLimiter::new(0);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        assert_eq!(limiter.per_minute(), 1);
        assert_eq!(limiter.check(ip, "echo"), Ok(()));
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));
    }
}
