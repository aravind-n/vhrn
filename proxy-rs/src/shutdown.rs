//! Shared cancellation state for proxy lifetime ownership.

use tokio::sync::watch;

#[derive(Clone)]
pub struct Shutdown {
    sender: watch::Sender<bool>,
}

impl Shutdown {
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self { sender }
    }
    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.sender.borrow()
    }
    pub fn request(&self) {
        self.sender.send_replace(true);
    }
    #[must_use]
    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.sender.subscribe()
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.subscribe();
        while !*receiver.borrow() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}
impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}
