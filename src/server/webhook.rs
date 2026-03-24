use tracing::{error, info};

use super::notifier::{Notifier, NotifyError};

pub struct WebhookClient {
    client: reqwest::Client,
    url: String,
}

impl WebhookClient {
    pub fn new(url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
        }
    }
}

impl Notifier for WebhookClient {
    fn name(&self) -> &'static str {
        "webhook"
    }

    async fn send(&self, title: &str, message: &str) -> Result<(), NotifyError> {
        let resp = self
            .client
            .post(&self.url)
            .json(&serde_json::json!({
                "title": title,
                "message": message,
            }))
            .send()
            .await
            .map_err(|e| NotifyError {
                backend: "webhook",
                message: format!("request failed: {e}"),
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            error!(status = %status, body, "webhook error");
            return Err(NotifyError {
                backend: "webhook",
                message: format!("HTTP {status}: {body}"),
            });
        }

        info!("webhook notification sent");
        Ok(())
    }
}
