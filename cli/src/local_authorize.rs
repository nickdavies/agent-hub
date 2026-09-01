use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;

use crate::attended::{AuthorizationRecord, AuthorizationSelectors, revalidate_authorization};
use crate::state::StateStore;

pub(crate) async fn authorize(
    store: &StateStore,
    path: &Path,
    selectors: AuthorizationSelectors<'_>,
) -> anyhow::Result<()> {
    let digest = selectors.digest.to_string();
    let revalidated = revalidate_authorization(path, selectors).await?;
    let _lock = store.lock(&PathBuf::from("locks").join(format!("{digest}.lock")))?;
    store
        .create_json(
            &PathBuf::from("authorizations")
                .join("records")
                .join(format!("{digest}.json")),
            &AuthorizationRecord {
                artifact: revalidated.artifact,
                authorization_path: revalidated.authorization_path,
                authorized_at: Utc::now(),
                authorization_id: uuid::Uuid::new_v4().to_string(),
                receipt_id: uuid::Uuid::new_v4().to_string(),
                domain: "attended".into(),
                state: crate::attended::LaunchState::Authorized,
                attempts: Vec::new(),
                active_attempt_id: None,
            },
        )
        .context("authorization already exists or was replayed")
}
