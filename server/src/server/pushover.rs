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

    async fn send(&self, title: &str, message: &str, url: Option<&str>) -> Result<(), NotifyError> {
        let mut form = vec![
            ("token", self.token.as_str()),
            ("user", self.user.as_str()),
            ("title", title),
            ("message", message),
        ];

        let url_owned;
        let url_title;
        if let Some(u) = url {
            url_owned = u.to_string();
            url_title = "Open in browser".to_string();
            form.push(("url", &url_owned));
            form.push(("url_title", &url_title));
        }

        let resp = self
            .client
            .post("https://api.pushover.net/1/messages.json")
            .form(&form)
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
