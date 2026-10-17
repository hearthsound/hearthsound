//! Login rate limiting.
//!
//! A Subsonic login that presents a username and a secret is limited to
//! `auth.login_rate_limit_per_minute` failed attempts per minute, counted
//! against the client's address and against the submitted username separately.
//! Both keys are charged on every failure, so neither a single address walking
//! a list of usernames nor a single account attacked from many addresses can
//! exceed the limit.
//!
//! A successful login is not charged. Subsonic clients send their credentials
//! with every request, so counting successes would lock out a client that
//! merely browses quickly. Once a key has spent its budget, every attempt for
//! it is refused before the secret is checked, including a correct one, until
//! the budget refills; otherwise the limiter would answer a guess with success
//! or failure and stop nothing.
//!
//! The budget is `governor`'s generic cell rate algorithm over a keyed state
//! store. `governor` cannot inspect a key without charging it, so a key that
//! spends its last cell is recorded in a block list with the instant its next
//! cell arrives, and [`LoginRateLimiter::check`] reads that list. The clock is
//! a type parameter, which lets a test drive a `FakeRelativeClock` instead of
//! waiting a minute for the bucket to refill.

use std::collections::HashMap;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use governor::clock::{Clock, DefaultClock};
use governor::middleware::StateInformationMiddleware;
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

/// The keyed limiter type the clock resolves to.
///
/// The state information middleware reports the burst capacity left after a
/// charge, which is how a key that has just spent its budget is recognised.
type Limiter<C> =
    RateLimiter<LoginKey, DefaultKeyedStateStore<LoginKey>, C, StateInformationMiddleware>;

/// The limiter and the keys it currently refuses, guarded together.
struct State<C: Clock> {
    /// The failure budget of every key.
    limiter: Limiter<C>,
    /// Keys that have spent their budget, with the instant the next attempt
    /// is allowed.
    blocked: HashMap<LoginKey, C::Instant>,
}

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
/// limiter.record_failure(ip, "echo");
/// assert!(limiter.check(ip, "echo").is_ok());
/// limiter.record_failure(ip, "echo");
/// assert!(limiter.check(ip, "echo").is_err());
/// ```
pub struct LoginRateLimiter<C: Clock + Clone = DefaultClock> {
    /// The limiter and its block list, replaced when the rate changes.
    state: Mutex<State<C>>,
    /// The rate the limiter currently enforces, per minute.
    ///
    /// Atomic so [`LoginRateLimiter::per_minute`] needs no lock; it is only ever
    /// written while the state lock is held.
    per_minute: AtomicU32,
    /// The clock every rebuilt limiter keeps using.
    clock: C,
}

impl LoginRateLimiter<DefaultClock> {
    /// Builds a limiter over the real clock.
    ///
    /// # Parameters
    ///
    /// - `per_minute`: the number of failed attempts allowed per minute. Zero
    ///   is treated as one, because a limiter that admits nothing could never
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
    /// - `per_minute`: the number of failed attempts allowed per minute; zero
    ///   is treated as one.
    /// - `clock`: the clock the limiter reads.
    ///
    /// # Returns
    ///
    /// The limiter.
    pub fn with_clock(per_minute: u32, clock: C) -> LoginRateLimiter<C> {
        LoginRateLimiter {
            state: Mutex::new(State::new(per_minute, clock.clone())),
            per_minute: AtomicU32::new(per_minute.max(1)),
            clock,
        }
    }

    /// Reports whether an attempt from an address for a username may be
    /// checked, without charging either key.
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
    /// Returns [`RateLimited`] when either key has spent its failure budget and
    /// it has not refilled yet.
    pub fn check(&self, ip: IpAddr, username: &str) -> Result<(), RateLimited> {
        let state = self.lock();
        let now = self.clock.now();
        let address = LoginKey::Ip(ip);
        let username = LoginKey::Username(username.to_ascii_lowercase());
        if state.is_blocked(&address, now) || state.is_blocked(&username, now) {
            Err(RateLimited)
        } else {
            Ok(())
        }
    }

    /// Charges one failed attempt against an address and a username.
    ///
    /// A key whose budget this failure spends is refused by
    /// [`LoginRateLimiter::check`] until its next attempt is due.
    ///
    /// # Parameters
    ///
    /// - `ip`: the address the request came from.
    /// - `username`: the submitted username, counted case-insensitively.
    pub fn record_failure(&self, ip: IpAddr, username: &str) {
        let mut state = self.lock();
        let now = self.clock.now();
        state.blocked.retain(|_, until| *until > now);
        state.charge(LoginKey::Ip(ip));
        state.charge(LoginKey::Username(username.to_ascii_lowercase()));
    }

    /// Reports whether an attempt from an address may be checked, without
    /// charging it.
    ///
    /// This is the address half of [`LoginRateLimiter::check`], for a request
    /// that carries no username: a bearer credential is only known to be one
    /// once it has been checked, so there is no username to key the limit on.
    ///
    /// # Parameters
    ///
    /// - `ip`: the address the request came from.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the address may proceed.
    ///
    /// # Errors
    ///
    /// Returns [`RateLimited`] when the address has spent its failure budget
    /// and it has not refilled yet.
    pub fn check_address(&self, ip: IpAddr) -> Result<(), RateLimited> {
        let state = self.lock();
        if state.is_blocked(&LoginKey::Ip(ip), self.clock.now()) {
            Err(RateLimited)
        } else {
            Ok(())
        }
    }

    /// Charges one failed attempt against an address only.
    ///
    /// # Parameters
    ///
    /// - `ip`: the address the request came from.
    pub fn record_address_failure(&self, ip: IpAddr) {
        let mut state = self.lock();
        let now = self.clock.now();
        state.blocked.retain(|_, until| *until > now);
        state.charge(LoginKey::Ip(ip));
    }

    /// Changes the rate, resetting the counters when it actually changes.
    ///
    /// # Parameters
    ///
    /// - `per_minute`: the new number of failed attempts allowed per minute;
    ///   zero is treated as one.
    pub fn set_per_minute(&self, per_minute: u32) {
        let per_minute = per_minute.max(1);
        if self.per_minute.load(Ordering::Relaxed) == per_minute {
            return;
        }
        let mut state = self.lock();
        if self.per_minute.load(Ordering::Relaxed) == per_minute {
            return;
        }
        *state = State::new(per_minute, self.clock.clone());
        self.per_minute.store(per_minute, Ordering::Relaxed);
    }

    /// Returns the rate currently enforced, per minute.
    pub fn per_minute(&self) -> u32 {
        self.per_minute.load(Ordering::Relaxed)
    }

    /// Locks the state, recovering from a poison rather than panicking.
    ///
    /// A panic while the lock is held would otherwise make every later login
    /// fail, so the state is taken back instead.
    fn lock(&self) -> std::sync::MutexGuard<'_, State<C>> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl<C: Clock> State<C> {
    /// Builds an empty state for a rate.
    ///
    /// # Parameters
    ///
    /// - `per_minute`: the number of failed attempts allowed per minute; zero
    ///   is treated as one.
    /// - `clock`: the clock the limiter reads.
    ///
    /// # Returns
    ///
    /// A state in which no key is blocked.
    fn new(per_minute: u32, clock: C) -> State<C> {
        let per_minute = NonZeroU32::new(per_minute.max(1)).unwrap_or(NonZeroU32::MIN);
        State {
            limiter: RateLimiter::dashmap_with_clock(Quota::per_minute(per_minute), clock)
                .with_middleware::<StateInformationMiddleware>(),
            blocked: HashMap::new(),
        }
    }

    /// Reports whether a key is refused at an instant.
    fn is_blocked(&self, key: &LoginKey, now: C::Instant) -> bool {
        self.blocked.get(key).is_some_and(|until| *until > now)
    }

    /// Charges one failure to a key, blocking the key when its budget is spent.
    ///
    /// When the charge takes the last cell, the key is checked once more: the
    /// refusal names the instant the next cell arrives, which is how long the
    /// key stays blocked. A refused check leaves `governor`'s state unchanged,
    /// so that second check costs nothing. Should a cell arrive between the two
    /// checks, the second one takes it and the key simply waits for the next.
    fn charge(&mut self, key: LoginKey) {
        let spent = match self.limiter.check_key(&key) {
            Ok(snapshot) => snapshot.remaining_burst_capacity() == 0,
            Err(_) => true,
        };
        if !spent {
            return;
        }
        if let Err(not_until) = self.limiter.check_key(&key) {
            self.blocked.insert(key, not_until.earliest_possible());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use governor::clock::FakeRelativeClock;

    use super::*;

    #[test]
    fn checking_alone_never_spends_the_budget() {
        let limiter = LoginRateLimiter::new(1);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        for _ in 0..100 {
            assert_eq!(limiter.check(ip, "echo"), Ok(()));
        }
    }

    #[test]
    fn limits_an_address_after_its_failures_are_spent() {
        let limiter = LoginRateLimiter::new(2);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        limiter.record_failure(ip, "echo");
        assert_eq!(limiter.check(ip, "echo"), Ok(()));
        limiter.record_failure(ip, "echo");
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));
    }

    #[test]
    fn limits_a_username_across_addresses() {
        let limiter = LoginRateLimiter::new(2);
        let first = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let second = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        let third = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3));

        limiter.record_failure(first, "echo");
        limiter.record_failure(first, "other");
        limiter.record_failure(second, "echo");
        // The first address has spent its two failures.
        assert_eq!(limiter.check(first, "third"), Err(RateLimited));
        // The username has spent its two failures, whatever the address.
        assert_eq!(limiter.check(third, "echo"), Err(RateLimited));
        // A fresh address and username are unaffected.
        assert_eq!(limiter.check(third, "other"), Ok(()));
    }

    #[test]
    fn counts_usernames_case_insensitively() {
        let limiter = LoginRateLimiter::new(1);
        let first = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let second = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        limiter.record_failure(first, "Echo");
        assert_eq!(limiter.check(second, "echo"), Err(RateLimited));
    }

    #[test]
    fn a_blocked_key_is_admitted_again_when_a_failure_refills() {
        let clock = FakeRelativeClock::default();
        let limiter = LoginRateLimiter::with_clock(2, clock.clone());
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        limiter.record_failure(ip, "echo");
        limiter.record_failure(ip, "echo");
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));

        // At two per minute, one failure refills every 30 seconds.
        clock.advance(Duration::from_secs(29));
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));
        clock.advance(Duration::from_secs(1));
        assert_eq!(limiter.check(ip, "echo"), Ok(()));

        // The refilled failure is the only one: the next one blocks again.
        limiter.record_failure(ip, "echo");
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));
    }

    #[test]
    fn changing_the_rate_resets_the_counters() {
        let limiter = LoginRateLimiter::new(1);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        limiter.record_failure(ip, "echo");
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));

        limiter.set_per_minute(2);
        assert_eq!(limiter.per_minute(), 2);
        assert_eq!(limiter.check(ip, "echo"), Ok(()));
    }

    #[test]
    fn zero_is_treated_as_one_failure() {
        let limiter = LoginRateLimiter::new(0);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        assert_eq!(limiter.per_minute(), 1);
        assert_eq!(limiter.check(ip, "echo"), Ok(()));
        limiter.record_failure(ip, "echo");
        assert_eq!(limiter.check(ip, "echo"), Err(RateLimited));
    }
}
