use anyhow::ensure;
use protocol::{
    AgentHubCatalogNetworkOptionV1, AgentHubRunNetworkClass, AgentHubRunToolApproval,
    NetworkResourceKind, ToolApprovalMode,
};

pub(crate) fn image_family(image: &str) -> String {
    let without_digest = image.split('@').next().unwrap_or(image);
    let slash = without_digest.rfind('/');
    match without_digest.rfind(':') {
        Some(colon) if slash.is_none_or(|slash| colon > slash) => {
            without_digest[..colon].to_string()
        }
        _ => without_digest.to_string(),
    }
}

pub(crate) fn tool_approval_mode(value: &str) -> anyhow::Result<ToolApprovalMode> {
    match value {
        "auto" => Ok(ToolApprovalMode::Auto),
        "gateway" => Ok(ToolApprovalMode::Dippy),
        other => anyhow::bail!("unsupported attended tool approval mode '{other}'"),
    }
}

pub(crate) fn requested_tool_approval_mode(value: &str) -> anyhow::Result<ToolApprovalMode> {
    match value {
        "auto" => Ok(ToolApprovalMode::Auto),
        "dippy" => Ok(ToolApprovalMode::Dippy),
        other => anyhow::bail!("unsupported attended tool approval selector '{other}'"),
    }
}

pub(crate) fn tool_approval_within_ceiling(
    selected: ToolApprovalMode,
    ceiling: ToolApprovalMode,
) -> bool {
    selected == ToolApprovalMode::Dippy || ceiling == ToolApprovalMode::Auto
}

pub(crate) fn tool_approval_name(value: ToolApprovalMode) -> &'static str {
    match value {
        ToolApprovalMode::Auto => "auto",
        ToolApprovalMode::Dippy => "dippy",
    }
}

pub(crate) fn compatible_tool_approvals(
    ceiling: &str,
    approval_bundle_available: bool,
) -> anyhow::Result<Vec<AgentHubRunToolApproval>> {
    let ceiling_mode = tool_approval_mode(ceiling)?;
    Ok([
        (AgentHubRunToolApproval::Auto, ToolApprovalMode::Auto),
        (AgentHubRunToolApproval::Dippy, ToolApprovalMode::Dippy),
    ]
    .into_iter()
    .filter_map(|(public, selected)| {
        (tool_approval_within_ceiling(selected, ceiling_mode)
            && match selected {
                ToolApprovalMode::Auto => !approval_bundle_available,
                ToolApprovalMode::Dippy => approval_bundle_available,
            })
        .then_some(public)
    })
    .collect())
}

fn canonical_resources(mut resources: Vec<NetworkResourceKind>) -> Vec<NetworkResourceKind> {
    resources.sort_by_key(ToString::to_string);
    resources.dedup();
    resources
}

pub(crate) fn ensure_unique_resources(resources: &[NetworkResourceKind]) -> anyhow::Result<()> {
    let mut canonical = resources.to_vec();
    canonical.sort_by_key(ToString::to_string);
    ensure!(
        !canonical.windows(2).any(|pair| pair[0] == pair[1]),
        "duplicate network resource selector"
    );
    Ok(())
}

pub(crate) fn selector_resources(selector: &str) -> anyhow::Result<Vec<NetworkResourceKind>> {
    let resources = match selector {
        "none" => vec![],
        "llm-only" => vec![NetworkResourceKind::Llm],
        "build" => vec![
            NetworkResourceKind::Llm,
            NetworkResourceKind::Github,
            NetworkResourceKind::GoRegistry,
            NetworkResourceKind::CratesRegistry,
            NetworkResourceKind::PypiRegistry,
            NetworkResourceKind::NpmRegistry,
        ],
        "guarded-web" => vec![NetworkResourceKind::GuardedWeb],
        other => anyhow::bail!("unsupported attended network selector '{other}'"),
    };
    Ok(canonical_resources(resources))
}

pub(crate) fn network_class_within_ceiling(selected: &str, ceiling: &str) -> anyhow::Result<bool> {
    let selected = selector_resources(selected)?;
    let ceiling = selector_resources(ceiling)?;
    Ok(selected.iter().all(|resource| ceiling.contains(resource)))
}

pub(crate) fn compatible_network_options(
    ceiling: &str,
) -> anyhow::Result<Vec<AgentHubCatalogNetworkOptionV1>> {
    let mut compatible = Vec::new();
    for class in [
        AgentHubRunNetworkClass::None,
        AgentHubRunNetworkClass::LlmOnly,
        AgentHubRunNetworkClass::Build,
        AgentHubRunNetworkClass::GuardedWeb,
    ] {
        if !network_class_within_ceiling(class.as_str(), ceiling)? {
            continue;
        }
        let available = selector_resources(class.as_str())?;
        for mask in 0..(1_usize << available.len()) {
            let requested = available
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, resource)| resource.clone())
                .collect::<Vec<_>>();
            compatible.push(AgentHubCatalogNetworkOptionV1 {
                network_class: class,
                resources: resolve_network_resources(class.as_str(), requested)?,
            });
        }
    }
    Ok(compatible)
}

pub(crate) fn resolve_network_resources(
    selector: &str,
    requested: Vec<NetworkResourceKind>,
) -> anyhow::Result<Vec<NetworkResourceKind>> {
    let ceiling = selector_resources(selector)?;
    ensure_unique_resources(&requested)?;
    let requested = canonical_resources(requested);
    ensure!(
        !requested.contains(&NetworkResourceKind::GuardedWeb) || requested.len() == 1,
        "guarded_web cannot be combined with other network resources"
    );
    ensure!(
        requested.iter().all(|resource| ceiling.contains(resource)),
        "requested network resources exceed '{selector}' environment ceiling"
    );
    Ok(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_network_options_use_exact_attended_resource_validation() {
        assert_eq!(compatible_network_options("build").unwrap().len(), 67);
        let guarded = compatible_network_options("guarded-web").unwrap();
        assert_eq!(guarded.len(), 3);
        assert_eq!(
            guarded
                .iter()
                .filter(|option| option.resources.is_empty())
                .count(),
            2
        );
        assert!(guarded.iter().any(|option| {
            option.network_class == AgentHubRunNetworkClass::GuardedWeb
                && option.resources == [NetworkResourceKind::GuardedWeb]
        }));
        assert!(guarded.iter().all(|option| {
            !option.resources.contains(&NetworkResourceKind::GuardedWeb)
                || option.resources.len() == 1
        }));
    }

    #[test]
    fn catalog_reports_only_materializable_approval_mode() {
        assert_eq!(
            compatible_tool_approvals("auto", false).unwrap(),
            vec![AgentHubRunToolApproval::Auto]
        );
        assert_eq!(
            compatible_tool_approvals("auto", true).unwrap(),
            vec![AgentHubRunToolApproval::Dippy]
        );
        assert!(
            compatible_tool_approvals("gateway", false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            compatible_tool_approvals("gateway", true).unwrap(),
            vec![AgentHubRunToolApproval::Dippy]
        );
    }
}
