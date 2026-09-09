/// Notification delivery backend.
///
/// Implement this trait to add a new notification channel.
/// See `pushover.rs` and `webhook.rs` for examples.
pub trait Notifier: Send + Sync + 'static {
    /// Backend name for logging.
    fn name(&self) -> &'static str;

    /// Deliver a notification. `url` is an optional link for the notification.
    fn send(
        &self,
        title: &str,
        message: &str,
        url: Option<&str>,
    ) -> impl std::future::Future<Output = Result<(), NotifyError>> + Send;
}

#[derive(Debug, thiserror::Error)]
#[error("{backend}: {message}")]
pub struct NotifyError {
    pub backend: &'static str,
    pub message: String,
}

/// No-op notifier for localhost/Docker use where push notifications aren't needed.
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn name(&self) -> &'static str {
        "null"
    }

    async fn send(
        &self,
        _title: &str,
        _message: &str,
        _url: Option<&str>,
    ) -> Result<(), NotifyError> {
        Ok(())
    }
}
