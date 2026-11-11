use tokio::sync::broadcast;

/// Listens for the server shutdown signal.
///
/// Shutdown is signalled using a `broadcast::Receiver`. Only a single value is
/// ever sent. Once a value has been sent via the broadcast channel, the server
/// should shutdown.
///
/// The `Shutdown` struct listens for the signal and tracks that the signal has
/// been received. Callers may query for whether the shutdown signal has been
/// received or not.
#[derive(Debug)]
pub(crate) struct Shutdown {
    /// `true` if the shutdown signal has been received
    shutdown: bool,

    /// The receive half of the channel used to listen for shutdown.
    notify: broadcast::Receiver<()>,
}

impl Shutdown {
    /// Create a new `Shutdown` backed by the given `broadcast::Receiver`.
    pub(crate) fn new(notify: broadcast::Receiver<()>) -> Shutdown {
        Shutdown {
            shutdown: false,
            notify,
        }
    }

    /// Returns `true` if the shutdown signal has been received.
    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Whether the shutdown signal has been sent, without waiting for it.
    pub(crate) fn is_signalled(&mut self) -> bool {
        if !self.shutdown {
            self.shutdown = !matches!(
                self.notify.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            );
        }
        self.shutdown
    }

    /// Receive the shutdown notice, waiting if necessary.
    pub(crate) async fn recv(&mut self) {
        // If the shutdown signal has already been received, then return
        // immediately.
        if self.shutdown {
            return;
        }

        // Cannot receive a "lag error" as only one value is ever sent.
        let _ = self.notify.recv().await;

        // Remember that the signal has been received.
        self.shutdown = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signal_is_seen_without_waiting() {
        let (notify, receiver) = broadcast::channel(1);
        let mut shutdown = Shutdown::new(receiver);
        assert!(!shutdown.is_signalled());
        notify.send(()).unwrap();
        assert!(shutdown.is_signalled());
        assert!(shutdown.is_shutdown());
    }
}
