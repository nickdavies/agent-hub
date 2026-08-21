use std::collections::HashMap;
use std::path::PathBuf;

use protocol::{
    ReactiveWorkflowDefinition, WorkflowValidationReport, parse_workflow_toml,
    validate_reactive_workflows,
};

pub struct WorkflowCatalog {
    definitions: HashMap<String, ReactiveWorkflowDefinition>,
    triage_hints: HashMap<String, String>,
}

impl WorkflowCatalog {
    #[cfg(test)]
    pub fn empty_for_test() -> Self {
        Self {
            definitions: HashMap::new(),
            triage_hints: HashMap::new(),
        }
    }
    #[cfg(test)]
    pub fn load_from_dir(dir: &str) -> anyhow::Result<Self> {
        Self::load_from_dir_with_installed(dir, None)
    }

    pub fn load_from_dir_with_installed(
        dir: &str,
        installed: Option<&[String]>,
    ) -> anyhow::Result<Self> {
        let root = PathBuf::from(dir);
        if !root.is_dir() {
            anyhow::bail!("workflow directory '{}' does not exist", root.display());
        }

        let paths = if let Some(installed) = installed {
            installed
                .iter()
                .map(|name| root.join(name).join("workflow.toml"))
                .collect::<Vec<_>>()
        } else {
            std::fs::read_dir(&root)?
                .map(|entry| entry.map(|entry| entry.path().join("workflow.toml")))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|path| path.is_file())
                .collect()
        };

        let mut definitions = HashMap::new();
        let mut triage_hints = HashMap::new();
        for path in paths {
            let content = std::fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("failed to read '{}': {error}", path.display()))?;
            let definition = parse_workflow_toml(&content).map_err(|error| {
                anyhow::anyhow!("failed to parse '{}': {error}", path.display())
            })?;
            if definitions
                .insert(definition.id.clone(), definition.clone())
                .is_some()
            {
                anyhow::bail!("duplicate workflow id '{}'", definition.id);
            }
            if let Some(directory) = path.parent() {
                let hint_path = directory.join("triage-hint.md");
                if hint_path.exists() {
                    let hint = std::fs::read_to_string(&hint_path).map_err(|error| {
                        anyhow::anyhow!(
                            "failed to read configured triage hint '{}': {error}",
                            hint_path.display()
                        )
                    })?;
                    if !hint.trim().is_empty() {
                        triage_hints.insert(definition.id, hint.trim().to_string());
                    }
                }
            }
        }

        let report =
            validate_reactive_workflows(&definitions.values().cloned().collect::<Vec<_>>());
        if !report.is_valid() {
            let errors = report
                .errors
                .iter()
                .map(|error| format!("{}: {}", error.workflow_id, error.message))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!("invalid installed workflows: {errors}");
        }

        Ok(Self {
            definitions,
            triage_hints,
        })
    }

    pub fn get(&self, id: &str) -> Option<&ReactiveWorkflowDefinition> {
        self.definitions.get(id)
    }

    pub fn list(&self) -> Vec<ReactiveWorkflowDefinition> {
        let mut definitions = self.definitions.values().cloned().collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.id.cmp(&right.id));
        definitions
    }

    pub fn validation_report(&self) -> WorkflowValidationReport {
        validate_reactive_workflows(&self.list())
    }

    pub fn triage_hint(&self, id: &str) -> Option<&str> {
        self.triage_hints.get(id).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_workflow(body: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("workflow-{}", uuid::Uuid::new_v4()));
        let directory = root.join("test");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("workflow.toml"), body).unwrap();
        root
    }

    #[test]
    fn rejects_missing_and_obsolete_schema() {
        for schema in ["", "schema = \"legacy_v1\""] {
            let root = write_workflow(&format!(
                "[workflow]\nid = \"test\"\n{schema}\ninitial_state = \"done\"\n[[workflow.states]]\nid = \"done\"\nkind = \"terminal\"\n"
            ));
            assert!(WorkflowCatalog::load_from_dir(root.to_str().unwrap()).is_err());
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rejects_invalid_installed_workflow_instead_of_dropping_it() {
        let root = write_workflow(
            "[workflow]\nid = \"test\"\nschema = \"reactive_v2\"\ninitial_state = \"missing\"\n",
        );
        assert!(WorkflowCatalog::load_from_dir(root.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn catalog_rejects_obsolete_timeout_and_unknown_fields() {
        for field in ["timeout = 30", "timeout_sec = 30", "unknown = true"] {
            let root = write_workflow(&format!(
                "[workflow]\nid = \"test\"\nschema = \"reactive_v2\"\ninitial_state = \"run\"\n[[workflow.states]]\nid = \"run\"\nkind = \"command\"\nexecutor = \"docker\"\nrepo_access = \"read_only\"\ncommand = \"true\"\n{field}\n[[workflow.states.on]]\nevent = \"job_succeeded\"\ntarget = \"done\"\n[[workflow.states]]\nid = \"done\"\nkind = \"terminal\"\n"
            ));
            assert!(WorkflowCatalog::load_from_dir(root.to_str().unwrap()).is_err());
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
