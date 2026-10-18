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
#[derive(Debug)]
pub struct SetupGate {
    /// The token, or `None` once it was consumed or when no setup is needed.
    token: Mutex<Option<String>>,
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
        let Some(expected) = token.as_deref() else {
            return false;
        };
        if bool::from(expected.as_bytes().ct_eq(offered.as_bytes())) {
            *token = None;
            true
        } else {
            false
        }
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
}
