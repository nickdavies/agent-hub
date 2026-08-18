use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use protocol::{
    ReactiveWorkflowDefinition, WorkflowDefinition, WorkflowDocument, WorkflowValidationError,
    WorkflowValidationReport, parse_workflow_toml_document, validate_reactive_workflows,
    validate_workflow_document, validate_workflows,
};

pub struct WorkflowCatalog {
    definitions: HashMap<String, WorkflowDefinition>,
    reactive_definitions: HashMap<String, protocol::ReactiveWorkflowDefinition>,
    /// Free-form `triage-hint.md` content keyed by workflow id. The triage
    /// prompt builder inlines this so the LLM has a description of each
    /// available workflow rather than just an id.
    triage_hints: HashMap<String, String>,
    validation_errors: Vec<WorkflowValidationError>,
}

impl WorkflowCatalog {
    pub fn empty() -> Self {
        Self {
            definitions: HashMap::new(),
            reactive_definitions: HashMap::new(),
            triage_hints: HashMap::new(),
            validation_errors: Vec::new(),
        }
    }

    pub fn load_from_dir(dir: &str) -> anyhow::Result<Self> {
        Self::load_from_dir_with_installed(dir, None)
    }

    pub fn load_from_dir_with_installed(
        dir: &str,
        installed: Option<&[String]>,
    ) -> anyhow::Result<Self> {
        let path = PathBuf::from(dir);
        if !path.exists() {
            return Ok(Self {
                definitions: HashMap::new(),
                reactive_definitions: HashMap::new(),
                triage_hints: HashMap::new(),
                validation_errors: vec![],
            });
        }

        let mut definitions = HashMap::new();
        let mut reactive_definitions = HashMap::new();
        let mut triage_hints: HashMap<String, String> = HashMap::new();
        let mut validation_errors = Vec::new();

        let workflow_paths: Vec<PathBuf> = if let Some(installed) = installed {
            installed
                .iter()
                .map(|name| path.join(name).join("workflow.toml"))
                .collect()
        } else {
            let mut paths = Vec::new();
            for entry in std::fs::read_dir(&path)? {
                let entry = entry?;
                let entry_path = entry.path();
                if !entry_path.is_dir() {
                    continue;
                }
                let workflow_toml_path = entry_path.join("workflow.toml");
                if workflow_toml_path.is_file() {
                    paths.push(workflow_toml_path);
                }
            }
            paths
        };

        for workflow_path in workflow_paths {
            if !workflow_path.is_file() {
                validation_errors.push(WorkflowValidationError {
                    workflow_id: workflow_path
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "unknown".to_string()),
                    message: format!("missing workflow definition at {}", workflow_path.display()),
                });
                continue;
            }

            parse_workflow_file(
                &workflow_path,
                &mut definitions,
                &mut reactive_definitions,
                &mut validation_errors,
            )?;

            // A present triage hint is deployment configuration. Failure to
            // read it must stop startup rather than silently changing routing
            // context.
            if let Some(workflow_dir) = workflow_path.parent() {
                let hint_path = workflow_dir.join("triage-hint.md");
                if hint_path.is_file() {
                    let content = std::fs::read_to_string(&hint_path).map_err(|error| {
                        anyhow::anyhow!(
                            "failed to read configured triage hint '{}': {error}",
                            hint_path.display()
                        )
                    })?;
                    let trimmed = content.trim().to_string();
                    if !trimmed.is_empty() {
                        if let Some(workflow_id) = workflow_dir
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                        {
                            triage_hints.insert(workflow_id, trimmed);
                        }
                    }
                }
            }
        }

        loop {
            let defs_vec = definitions.values().cloned().collect::<Vec<_>>();
            let cross_report = validate_workflows(&defs_vec);
            if cross_report.is_valid() {
                break;
            }

            let invalid_ids = cross_report
                .errors
                .iter()
                .map(|err| err.workflow_id.clone())
                .filter(|id| definitions.contains_key(id))
                .collect::<HashSet<_>>();
            validation_errors.extend(cross_report.errors);

            if invalid_ids.is_empty() {
                break;
            }

            for id in invalid_ids {
                definitions.remove(&id);
            }
        }

        let reactive_vec = reactive_definitions.values().cloned().collect::<Vec<_>>();
        let legacy_vec = definitions.values().cloned().collect::<Vec<_>>();
        let reactive_report = validate_reactive_workflows(&reactive_vec, &legacy_vec);
        let invalid_reactive_ids = reactive_report
            .errors
            .iter()
            .map(|err| err.workflow_id.clone())
            .filter(|id| reactive_definitions.contains_key(id))
            .collect::<HashSet<_>>();
        validation_errors.extend(reactive_report.errors);
        for id in invalid_reactive_ids {
            reactive_definitions.remove(&id);
        }

        // Drop hints for workflow ids that didn't survive validation so the
        // triage prompt only mentions live workflows.
        triage_hints
            .retain(|id, _| definitions.contains_key(id) || reactive_definitions.contains_key(id));

        Ok(Self {
            definitions,
            reactive_definitions,
            triage_hints,
            validation_errors,
        })
    }

    pub fn get(&self, id: &str) -> Option<&WorkflowDefinition> {
        self.definitions.get(id)
    }

    pub fn get_reactive(&self, id: &str) -> Option<&ReactiveWorkflowDefinition> {
        self.reactive_definitions.get(id)
    }

    pub fn list(&self) -> Vec<WorkflowDefinition> {
        let mut defs = self.definitions.values().cloned().collect::<Vec<_>>();
        defs.sort_by(|a, b| a.id.cmp(&b.id));
        defs
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn list_reactive(&self) -> Vec<ReactiveWorkflowDefinition> {
        let mut defs = self
            .reactive_definitions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        defs.sort_by(|a, b| a.id.cmp(&b.id));
        defs
    }

    pub fn validation_report(&self) -> WorkflowValidationReport {
        WorkflowValidationReport {
            errors: self.validation_errors.clone(),
        }
    }

    /// Returns the loaded `triage-hint.md` body for the given workflow id,
    /// if one exists.
    pub fn triage_hint(&self, id: &str) -> Option<&str> {
        self.triage_hints.get(id).map(String::as_str)
    }
}

fn parse_workflow_file(
    path: &Path,
    definitions: &mut HashMap<String, WorkflowDefinition>,
    reactive_definitions: &mut HashMap<String, ReactiveWorkflowDefinition>,
    validation_errors: &mut Vec<WorkflowValidationError>,
) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)?;
    match parse_workflow_toml_document(&content) {
        Ok(WorkflowDocument::LegacyV1 { workflow: def }) => {
            let report = validate_workflow_document(&WorkflowDocument::LegacyV1 {
                workflow: def.clone(),
            });
            if report.is_valid() {
                if definitions.contains_key(&def.id) {
                    validation_errors.push(WorkflowValidationError {
                        workflow_id: def.id.clone(),
                        message: format!("duplicate workflow id '{}'", def.id),
                    });
                } else {
                    definitions.insert(def.id.clone(), def);
                }
            } else {
                validation_errors.extend(report.errors);
            }
        }
        Ok(WorkflowDocument::ReactiveV2 { workflow: def }) => {
            let report = validate_workflow_document(&WorkflowDocument::ReactiveV2 {
                workflow: def.clone(),
            });
            if report.is_valid() {
                if reactive_definitions.contains_key(&def.id) {
                    validation_errors.push(WorkflowValidationError {
                        workflow_id: def.id.clone(),
                        message: format!("duplicate reactive workflow id '{}'", def.id),
                    });
                } else {
                    reactive_definitions.insert(def.id.clone(), def);
                }
            } else {
                validation_errors.extend(report.errors);
            }
        }
        Err(e) => {
            validation_errors.push(WorkflowValidationError {
                workflow_id: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".to_string()),
                message: format!("failed to parse TOML: {e}"),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_cross_workflow_child_cycle() {
        let dir = std::env::temp_dir().join(format!("wf-catalog-cycle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let a = r#"
[workflow]
id = "a"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "b"
command = ""
"#;
        let b = r#"
[workflow]
id = "b"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "a"
command = ""
"#;
        let a_dir = dir.join("a");
        let b_dir = dir.join("b");
        std::fs::create_dir_all(&a_dir).expect("mkdir a");
        std::fs::create_dir_all(&b_dir).expect("mkdir b");
        std::fs::write(a_dir.join("workflow.toml"), a).expect("write a");
        std::fs::write(b_dir.join("workflow.toml"), b).expect("write b");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list().is_empty());
        assert!(!catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_cascades_cross_validation_after_removal() {
        let dir = std::env::temp_dir().join(format!("wf-catalog-cascade-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let a = r#"
[workflow]
id = "a"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "b"
command = ""
"#;
        let b = r#"
[workflow]
id = "b"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "missing"
command = ""
"#;
        let a_dir = dir.join("a");
        let b_dir = dir.join("b");
        std::fs::create_dir_all(&a_dir).expect("mkdir a");
        std::fs::create_dir_all(&b_dir).expect("mkdir b");
        std::fs::write(a_dir.join("workflow.toml"), a).expect("write a");
        std::fs::write(b_dir.join("workflow.toml"), b).expect("write b");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list().is_empty());
        assert!(!catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_accepts_reactive_v2_workflow() {
        let dir =
            std::env::temp_dir().join(format!("wf-catalog-reactive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let reactive = r#"
[workflow]
id = "reactive_demo"
schema = "reactive_v2"
initial_state = "draft"

[[workflow.states]]
id = "draft"
kind = "human"

[[workflow.states.on]]
event = "submit"
target = "running"

[[workflow.states]]
id = "running"
kind = "auto"

[[workflow.states.on]]
event = "start"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        let reactive_dir = dir.join("reactive_demo");
        std::fs::create_dir_all(&reactive_dir).expect("mkdir reactive_demo");
        std::fs::write(reactive_dir.join("workflow.toml"), reactive).expect("write reactive");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(
            catalog
                .list_reactive()
                .iter()
                .any(|w| w.id == "reactive_demo")
        );
        assert!(catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_rejects_dot_in_legacy_step_id() {
        let dir =
            std::env::temp_dir().join(format!("wf-catalog-dot-step-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let wf = r#"
[workflow]
id = "wf_dot_step"
entry_step = "build.v1"

[[workflow.steps]]
id = "build.v1"
kind = "agent"
executor = "raw"
command = "echo"
args = ["ok"]
"#;
        let wf_dir = dir.join("wf_dot_step");
        std::fs::create_dir_all(&wf_dir).expect("mkdir wf_dot_step");
        std::fs::write(wf_dir.join("workflow.toml"), wf).expect("write");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list().is_empty());
        assert!(!catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_picks_up_triage_hint_alongside_workflow() {
        let dir = std::env::temp_dir().join(format!("wf-catalog-hint-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let wf = r#"
[workflow]
id = "with_hint"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "agent"
executor = "raw"
command = "echo"
args = ["ok"]
"#;
        let wf_dir = dir.join("with_hint");
        std::fs::create_dir_all(&wf_dir).expect("mkdir with_hint");
        std::fs::write(wf_dir.join("workflow.toml"), wf).expect("write workflow");
        std::fs::write(
            wf_dir.join("triage-hint.md"),
            "Pick this when the task wants a friendly hint.\n",
        )
        .expect("write hint");

        // A second workflow with no hint, to confirm we don't fabricate one.
        let bare = r#"
[workflow]
id = "no_hint"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "agent"
executor = "raw"
command = "echo"
args = ["ok"]
"#;
        let bare_dir = dir.join("no_hint");
        std::fs::create_dir_all(&bare_dir).expect("mkdir no_hint");
        std::fs::write(bare_dir.join("workflow.toml"), bare).expect("write bare workflow");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert_eq!(
            catalog.triage_hint("with_hint"),
            Some("Pick this when the task wants a friendly hint."),
            "triage hint should be loaded and trimmed",
        );
        assert!(catalog.triage_hint("no_hint").is_none());
        assert!(catalog.triage_hint("totally_unknown").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn configured_unreadable_triage_hint_fails_catalog_load() {
        let tmp = std::env::temp_dir().join(format!("wf-hint-{}", uuid::Uuid::new_v4()));
        let workflow_dir = tmp.join("wf");
        std::fs::create_dir_all(&workflow_dir).expect("workflow dir");
        std::fs::write(
            workflow_dir.join("workflow.toml"),
            "[workflow]\nid = \"wf\"\nentry_step = \"done\"\n[[workflow.steps]]\nid = \"done\"\nkind = \"terminal\"\n",
        )
        .expect("workflow");
        std::fs::write(workflow_dir.join("triage-hint.md"), [0xff, 0xfe])
            .expect("invalid utf8 hint");
        let error = match WorkflowCatalog::load_from_dir(tmp.to_str().expect("tmp path")) {
            Ok(_) => panic!("configured unreadable hint must fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("failed to read configured triage hint")
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn load_drops_triage_hint_for_invalid_workflow() {
        let dir =
            std::env::temp_dir().join(format!("wf-catalog-hint-invalid-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Workflow id contains an invalid `.` in step id; the workflow should
        // be dropped during validation, and so should the hint.
        let bad = r#"
[workflow]
id = "broken"
entry_step = "build.v1"

[[workflow.steps]]
id = "build.v1"
kind = "agent"
executor = "raw"
command = "echo"
args = ["ok"]
"#;
        let bad_dir = dir.join("broken");
        std::fs::create_dir_all(&bad_dir).expect("mkdir broken");
        std::fs::write(bad_dir.join("workflow.toml"), bad).expect("write");
        std::fs::write(bad_dir.join("triage-hint.md"), "should not surface").expect("write hint");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list().is_empty());
        assert!(catalog.triage_hint("broken").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_rejects_dot_in_reactive_state_id() {
        let dir = std::env::temp_dir().join(format!(
            "wf-catalog-dot-reactive-state-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let wf = r#"
[workflow]
id = "reactive_dot_state"
schema = "reactive_v2"
initial_state = "draft.v1"

[[workflow.states]]
id = "draft.v1"
kind = "human"

[[workflow.states.on]]
event = "submit"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        let wf_dir = dir.join("reactive_dot_state");
        std::fs::create_dir_all(&wf_dir).expect("mkdir reactive_dot_state");
        std::fs::write(wf_dir.join("workflow.toml"), wf).expect("write");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list_reactive().is_empty());
        assert!(!catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }
}
