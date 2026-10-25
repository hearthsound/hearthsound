//! The one-time link that creates the first administrator.
//!
//! A server with no accounts cannot be administered through the API, because
//! every administrative route requires an account. The composition root
//! therefore mints a token at startup when the users table is empty, logs the
//! URL that carries it, and hands the gate to the native API: the setup
//! endpoint accepts the token once, creates the administrator, and answers the
//! same sign-in every other client gets.
//!
//! The token is a session-grade random string, compared in constant time, and
//! it disappears the moment it is used, so a leaked log line is only useful
//! until the first administrator exists. A restart mints a new one, because the
//! gate lives in memory and dies with the process.

use subtle::ConstantTimeEq;
use tokio::sync::Mutex;

use crate::session::random_token;

/// The one-time token that opens the first-run setup.
///
/// The gate holds the token until a request consumes it. Consuming it is the
/// only way to pass: a request that presents the token takes it away from every
/// later one, so exactly one setup request can create the administrator.
///
/// The token that was spent is remembered until it is given back, so a setup
/// that failed after spending the link can restore it
/// ([`SetupGate::restore`]) without letting a stranger re-open the gate. A
/// restart mints a new token, because the gate lives in memory.
#[derive(Debug)]
pub struct SetupGate {
    /// The token, or `None` once it was consumed or when no setup is needed.
    token: Mutex<Option<String>>,
    /// The token a request consumed, kept until it is restored.
    ///
    /// It is the same exposure as before the spend - the token was in memory
    /// anyway - and it is what proves that the request asking for a restore is
    /// the request that spent the link.
    spent: Mutex<Option<String>>,
}

impl SetupGate {
    /// Builds the gate.
    ///
    /// # Parameters
    ///
    /// - `needed`: whether the server has no account yet. When it has one, the
    ///   gate holds no token and every setup request is refused.
    ///
    /// # Returns
    ///
    /// The gate, with a fresh token when one is needed.
    pub fn new(needed: bool) -> SetupGate {
        SetupGate {
            token: Mutex::new(needed.then(random_token)),
            spent: Mutex::new(None),
        }
    }

    /// Returns the token, when the setup is still open.
    ///
    /// This is what the composition root logs; nothing else reads it.
    ///
    /// # Returns
    ///
    /// The token, or `None` when the setup is not needed or already done.
    pub async fn token(&self) -> Option<String> {
        self.token.lock().await.clone()
    }

    /// Consumes the setup when the offered token matches.
    ///
    /// # Parameters
    ///
    /// - `offered`: the token the request presented.
    ///
    /// # Returns
    ///
    /// `true` when the token matched and was consumed; `false` when the setup
    /// is closed or the token is wrong, in which case the gate is unchanged.
    pub async fn consume(&self, offered: &str) -> bool {
        let mut token = self.token.lock().await;
        let Some(expected) = token.as_ref().cloned() else {
            return false;
        };
        if !bool::from(expected.as_bytes().ct_eq(offered.as_bytes())) {
            return false;
        }
        *token = None;
        *self.spent.lock().await = Some(expected);
        true
    }

    /// Gives a spent token back, when the request that spent it asks.
    ///
    /// This is what a setup that failed after spending the link uses: the
    /// administrator was not created, so the operator's retry has to work
    /// without restarting the server. The token is only restored when the gate
    /// holds none and `offered` is the token this gate handed out, compared in
    /// constant time, so a request that merely guessed cannot re-open the gate.
    /// A gate that never had a token stays closed.
    ///
    /// # Parameters
    ///
    /// - `offered`: the token the request presented.
    ///
    /// # Returns
    ///
    /// `true` when the token is back in place and can be consumed again.
    pub async fn restore(&self, offered: &str) -> bool {
        let mut token = self.token.lock().await;
        let mut spent = self.spent.lock().await;
        let Some(expected) = spent.as_ref().cloned() else {
            return false;
        };
        if token.is_some() || !bool::from(expected.as_bytes().ct_eq(offered.as_bytes())) {
            return false;
        }
        *token = Some(expected);
        *spent = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_gate_without_a_need_has_no_token() {
        let gate = SetupGate::new(false);
        assert!(gate.token().await.is_none());
        assert!(!gate.consume("anything").await);
    }

    #[tokio::test]
    async fn a_token_is_consumed_once() {
        let gate = SetupGate::new(true);
        let token = gate.token().await.unwrap();
        assert_eq!(token.len(), 52);

        assert!(!gate.consume("wrong").await);
        assert!(
            gate.token().await.is_some(),
            "a wrong token changes nothing"
        );
        assert!(gate.consume(&token).await);
        assert!(gate.token().await.is_none());
        assert!(!gate.consume(&token).await, "the token is spent");
    }

    #[tokio::test]
    async fn a_consumed_token_can_be_restored_and_consumed_again() {
        let gate = SetupGate::new(true);
        let token = gate.token().await.unwrap();
        assert!(gate.consume(&token).await);
        assert!(gate.restore(&token).await);
        assert_eq!(gate.token().await, Some(token.clone()));
        assert!(gate.consume(&token).await, "the restored token works");
        assert!(gate.token().await.is_none());
    }

    #[tokio::test]
    async fn restoring_a_wrong_token_changes_nothing() {
        let gate = SetupGate::new(true);
        let token = gate.token().await.unwrap();
        assert!(gate.consume(&token).await);
        assert!(!gate.restore("not-the-token").await);
        assert!(
            gate.token().await.is_none(),
            "the gate stays closed for a stranger"
        );
        assert!(gate.restore(&token).await, "the real token still restores");
    }

    #[tokio::test]
    async fn a_gate_that_never_had_a_token_restores_nothing() {
        let gate = SetupGate::new(false);
        assert!(!gate.restore("anything").await);
        assert!(gate.token().await.is_none());
    }

    #[tokio::test]
    async fn restoring_twice_is_a_no_op() {
        let gate = SetupGate::new(true);
        let token = gate.token().await.unwrap();
        assert!(gate.consume(&token).await);
        assert!(gate.restore(&token).await);
        assert!(
            !gate.restore(&token).await,
            "a gate that already holds a token is not re-opened twice"
        );
    }
}
