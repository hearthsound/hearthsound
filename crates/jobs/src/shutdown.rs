//! The cooperative stop signal the runner and the scheduler share.
//!
//! A server shuts down by triggering one [`Shutdown`], which every long-lived
//! worker holds a clone of. Waiting on it is cancellation-safe, and checking it
//! is a single atomic read, so a worker can test it between jobs and still wake
//! promptly when it is idle.

use tokio::sync::watch;

/// A one-way stop signal.
///
/// Every clone refers to the same signal: triggering any one of them wakes all
/// the others. The signal only ever goes from running to stopping; a second
/// trigger does nothing.
///
/// # Examples
///
/// ```
/// use hearthsound_jobs::Shutdown;
///
/// # async fn example() {
/// let shutdown = Shutdown::new();
/// let waiter = shutdown.clone();
///
/// shutdown.trigger();
/// waiter.wait().await;
/// assert!(waiter.is_triggered());
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Shutdown {
    sender: watch::Sender<bool>,
    receiver: watch::Receiver<bool>,
}

impl Shutdown {
    /// Builds a signal that is not triggered yet.
    ///
    /// # Returns
    ///
    /// The signal.
    pub fn new() -> Shutdown {
        let (sender, receiver) = watch::channel(false);
        Shutdown { sender, receiver }
    }

    /// Triggers the signal and wakes every waiter.
    pub fn trigger(&self) {
        let _ = self.sender.send(true);
    }

    /// Returns whether the signal has been triggered.
    pub fn is_triggered(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Waits until the signal is triggered.
    ///
    /// Returns immediately when it was already triggered, so a worker that
    /// starts after the signal does not wait for a trigger that will never
    /// come again.
    pub async fn wait(&self) {
        let mut receiver = self.receiver.clone();
        if *receiver.borrow_and_update() {
            return;
        }
        let _ = receiver.changed().await;
    }
}

impl Default for Shutdown {
    /// Returns an untriggered signal.
    fn default() -> Shutdown {
        Shutdown::new()
    }
}
