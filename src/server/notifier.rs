/// Notification delivery backend.
///
/// Implement this trait to add a new notification channel.
/// See `pushover.rs` and `webhook.rs` for examples.
pub trait Notifier: Send + Sync + 'static {
    /// Backend name for logging.
    fn name(&self) -> &'static str;

    /// Deliver a notification.
    fn send(
        &self,
        title: &str,
        message: &str,
    ) -> impl std::future::Future<Output = Result<(), NotifyError>> + Send;
}

#[derive(Debug, thiserror::Error)]
#[error("{backend}: {message}")]
pub struct NotifyError {
    pub backend: &'static str,
    pub message: String,
}
