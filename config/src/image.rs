use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
struct ImageTomlFile {
    image: ImageConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageConfig {
    pub name: String,
    pub tag: String,
    pub description: String,
    #[serde(default)]
    pub extends: Option<String>,
}

impl ImageConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read image config {}: {e}", path.display()))?;
        let wrapped: ImageTomlFile = toml::from_str(&raw)
            .map_err(|e| format!("failed to parse image config {}: {e}", path.display()))?;
        Ok(wrapped.image)
    }
}

pub fn resolve_build_order(configs: &[ImageConfig]) -> Result<Vec<String>, String> {
    let mut by_name = HashMap::with_capacity(configs.len());
    for cfg in configs {
        if by_name.contains_key(&cfg.name) {
            return Err(format!("duplicate image config '{}'", cfg.name));
        }
        by_name.insert(cfg.name.clone(), cfg);
    }

    let mut order = Vec::with_capacity(configs.len());
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();

    for cfg in configs {
        visit_image(&cfg.name, &by_name, &mut visiting, &mut visited, &mut order)?;
    }

    Ok(order)
}

fn visit_image(
    name: &str,
    by_name: &HashMap<String, &ImageConfig>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    order: &mut Vec<String>,
) -> Result<(), String> {
    if visited.contains(name) {
        return Ok(());
    }
    if visiting.contains(name) {
        return Err(format!("cyclic image extends chain detected at '{name}'"));
    }

    let cfg = by_name
        .get(name)
        .ok_or_else(|| format!("image '{name}' is not defined"))?;

    visiting.insert(name.to_string());
    if let Some(parent) = &cfg.extends {
        if !by_name.contains_key(parent) {
            return Err(format!(
                "image '{}' extends unknown image '{}'",
                cfg.name, parent
            ));
        }
        visit_image(parent, by_name, visiting, visited, order)?;
    }
    visiting.remove(name);

    visited.insert(name.to_string());
    order.push(name.to_string());
    Ok(())
}
