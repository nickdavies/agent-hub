use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;

use crate::attended::{AuthorizationRecord, AuthorizationSelectors, revalidate_authorization};
use crate::state::StateStore;

#[derive(Debug, serde::Serialize)]
pub(crate) struct AuthorizationReceipt {
    schema: &'static str,
    authorization_id: String,
    digest: String,
    authorized_at: chrono::DateTime<Utc>,
    retry_eligible: bool,
    canonical_run_command: String,
}

pub(crate) async fn authorize(
    store: &StateStore,
    path: &Path,
    selectors: AuthorizationSelectors<'_>,
) -> anyhow::Result<AuthorizationReceipt> {
    let digest = selectors.digest.to_string();
    let canonical_run_command = canonical_run_command(path, &selectors);
    let revalidated = revalidate_authorization(path, selectors).await?;
    let authorization_id = uuid::Uuid::new_v4().to_string();
    let authorized_at = Utc::now();
    let _lock = store.lock(&PathBuf::from("locks").join(format!("{digest}.lock")))?;
    store
        .create_json(
            &PathBuf::from("authorizations")
                .join("records")
                .join(format!("{digest}.json")),
            &AuthorizationRecord {
                authorization_id: authorization_id.clone(),
                artifact: revalidated.artifact,
                authorization_path: revalidated.authorization_path,
                authorized_at,
                receipt_id: uuid::Uuid::new_v4().to_string(),
                domain: "attended".into(),
                state: crate::attended::LaunchState::Authorized,
                attempts: Vec::new(),
                active_attempt_id: None,
            },
        )
        .context("authorization already exists or was replayed")?;
    Ok(AuthorizationReceipt {
        schema: "attended_authorization_receipt_v1",
        authorization_id,
        digest,
        authorized_at,
        retry_eligible: true,
        canonical_run_command,
    })
}

fn canonical_run_command(path: &Path, selectors: &AuthorizationSelectors<'_>) -> String {
    let mut command = format!(
        "agent-hub local run --environment {} --image-family {} --network-class {}",
        shell_word(selectors.environment),
        shell_word(selectors.image_family),
        shell_word(selectors.network_class),
    );
    for resource in selectors.network_resources {
        command.push_str(&format!(
            " --network-resource {}",
            shell_word(&resource.to_string())
        ));
    }
    command.push_str(&format!(
        " --tool-approval {}",
        shell_word(selectors.tool_approval)
    ));
    for overlay in selectors.overlays {
        command.push_str(&format!(" --overlay {}", shell_word(overlay)));
    }
    command.push_str(&format!(
        " --timeout-secs {} --authorization {} --digest {}",
        selectors.timeout_secs,
        shell_word(&path.display().to_string()),
        shell_word(selectors.digest),
    ));
    command
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._/:@".contains(&byte))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
