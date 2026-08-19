pub fn run(dir: String) -> i32 {
    let definitions = match load_definitions(&dir) {
        Ok(definitions) => definitions,
        Err(error) => {
            eprintln!("[FAIL] {error}");
            return 1;
        }
    };
    let report = protocol::validate_reactive_workflows(&definitions);
    if report.is_valid() {
        println!("All workflows valid ({} reactive_v2).", definitions.len());
        0
    } else {
        for error in report.errors {
            eprintln!("[FAIL] workflow {}: {}", error.workflow_id, error.message);
        }
        1
    }
}

fn load_definitions(dir: &str) -> Result<Vec<protocol::ReactiveWorkflowDefinition>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("cannot read directory {dir}: {error}"))?;
    let mut definitions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read entry in {dir}: {error}"))?;
        let entry_path = entry.path();
        let path = if entry_path.is_dir() {
            entry_path.join("workflow.toml")
        } else {
            entry_path
        };
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                return Err(format!("{} -> read error: {error}", path.display()));
            }
        };
        match protocol::parse_workflow_toml(&content) {
            Ok(definition) => definitions.push(definition),
            Err(error) => {
                return Err(format!("{} -> parse error: {error}", path.display()));
            }
        }
    }
    if definitions.is_empty() {
        return Err(format!("no installed workflows found in {dir}"));
    }
    Ok(definitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_missing_workflow_directories_fail() {
        let empty = std::env::temp_dir().join(format!("empty-workflows-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&empty).unwrap();
        assert!(load_definitions(empty.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&empty).unwrap();
        assert!(load_definitions(empty.to_str().unwrap()).is_err());
    }

    #[test]
    fn obsolete_timeout_and_unknown_fields_fail_cli_loading() {
        for field in ["timeout = 30", "timeout_sec = 30", "unknown = true"] {
            let root =
                std::env::temp_dir().join(format!("invalid-workflows-{}", uuid::Uuid::new_v4()));
            let workflow = root.join("invalid");
            std::fs::create_dir_all(&workflow).unwrap();
            std::fs::write(
                workflow.join("workflow.toml"),
                format!("[workflow]\nid = \"test\"\nschema = \"reactive_v2\"\ninitial_state = \"run\"\n[[workflow.states]]\nid = \"run\"\nkind = \"command\"\nexecutor = \"docker\"\nrepo_access = \"read_only\"\ncommand = \"true\"\n{field}\n[[workflow.states.on]]\nevent = \"job_succeeded\"\ntarget = \"done\"\n[[workflow.states]]\nid = \"done\"\nkind = \"terminal\"\n"),
            )
            .unwrap();
            assert!(load_definitions(root.to_str().unwrap()).is_err());
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
