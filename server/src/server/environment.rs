use protocol::{EnvironmentDefinition, EnvironmentOverlay, EnvironmentSelection};

pub fn validate_overlay_exists(overlay_name: &str, catalog: &[EnvironmentOverlay]) -> bool {
    catalog.iter().any(|overlay| overlay.name == overlay_name)
}

pub fn validate_environment_selection<'a>(
    selection: &EnvironmentSelection,
    environments: &'a [EnvironmentDefinition],
    overlays_catalog: &[EnvironmentOverlay],
) -> Result<&'a EnvironmentDefinition, String> {
    let environment = environments
        .iter()
        .find(|environment| environment.name == selection.base)
        .ok_or_else(|| format!("unknown base environment '{}'", selection.base))?;

    for overlay in &selection.overlays {
        if !validate_overlay_exists(overlay, overlays_catalog) {
            return Err(format!("overlay '{}' does not exist in catalog", overlay));
        }
        if !environment
            .allowed_overlays
            .iter()
            .any(|allowed| allowed == overlay)
        {
            return Err(format!(
                "overlay '{}' is not allowed for environment '{}'",
                overlay, environment.name
            ));
        }
    }

    Ok(environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str, overlays: &[&str]) -> EnvironmentDefinition {
        EnvironmentDefinition {
            name: name.to_string(),
            image: agent_hub_config::DURABLE_OPENCODE_IMAGE.to_string(),
            network_class: "restricted".to_string(),
            approval_mode: "auto".to_string(),
            allowed_overlays: overlays.iter().map(|value| (*value).to_string()).collect(),
            description: None,
            runtime: None,
        }
    }

    fn overlay(name: &str) -> EnvironmentOverlay {
        EnvironmentOverlay {
            name: name.to_string(),
            provider: "gh".to_string(),
            config: serde_json::json!({}),
        }
    }

    #[test]
    fn validates_known_environment_with_allowed_overlays() {
        let environments = vec![env("code-auto", &["github"])];
        let catalog = vec![overlay("github")];
        let selection = EnvironmentSelection {
            base: "code-auto".to_string(),
            overlays: vec!["github".to_string()],
        };

        let resolved =
            validate_environment_selection(&selection, &environments, &catalog).expect("valid");
        assert_eq!(resolved.name, "code-auto");
    }

    #[test]
    fn rejects_unknown_environment() {
        let environments = vec![env("code-auto", &["github"])];
        let catalog = vec![overlay("github")];
        let selection = EnvironmentSelection {
            base: "missing".to_string(),
            overlays: vec![],
        };

        let error = validate_environment_selection(&selection, &environments, &catalog)
            .expect_err("unknown env");
        assert!(error.contains("unknown base environment"));
    }

    #[test]
    fn rejects_disallowed_overlay() {
        let environments = vec![env("code-auto", &["github"])];
        let catalog = vec![overlay("github"), overlay("aws:readonly")];
        let selection = EnvironmentSelection {
            base: "code-auto".to_string(),
            overlays: vec!["aws:readonly".to_string()],
        };

        let error = validate_environment_selection(&selection, &environments, &catalog)
            .expect_err("disallowed overlay");
        assert!(error.contains("not allowed"));
    }

    #[test]
    fn rejects_overlay_missing_from_catalog() {
        let environments = vec![env("code-auto", &["github"])];
        let catalog = Vec::new();
        let selection = EnvironmentSelection {
            base: "code-auto".to_string(),
            overlays: vec!["github".to_string()],
        };

        let error = validate_environment_selection(&selection, &environments, &catalog)
            .expect_err("missing catalog overlay");
        assert!(error.contains("does not exist in catalog"));
    }
}
