use tracing::{error, info};

use super::notifier::{Notifier, NotifyError};

pub struct PushoverClient {
    client: reqwest::Client,
    token: String,
    user: String,
}

impl PushoverClient {
    pub fn new(token: String, user: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            token,
            user,
        }
    }
}

impl Notifier for PushoverClient {
    fn name(&self) -> &'static str {
        "pushover"
    }

    async fn send(&self, title: &str, message: &str) -> Result<(), NotifyError> {
        let resp = self
            .client
            .post("https://api.pushover.net/1/messages.json")
            .form(&[
                ("token", self.token.as_str()),
                ("user", self.user.as_str()),
                ("title", title),
                ("message", message),
            ])
            .send()
            .await
            .map_err(|e| NotifyError {
                backend: "pushover",
                message: format!("request failed: {e}"),
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            error!(status = %status, body, "pushover API error");
            return Err(NotifyError {
                backend: "pushover",
                message: format!("API returned {status}: {body}"),
            });
        }

        info!(title, "pushover notification sent");
        Ok(())
    }
}
