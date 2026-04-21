pub fn run(dir: String) -> i32 {
    let mut legacy_defs = Vec::new();
    let mut reactive_defs = Vec::new();
    let mut per_doc_failures = Vec::new();
    let read = std::fs::read_dir(&dir);
    let entries = match read {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[FAIL] cannot read directory {}: {}", dir, e);
            return 1;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .and_then(|v| v.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("toml"))
            .unwrap_or(false)
        {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[FAIL] {} -> read error: {}", path.display(), e);
                return 1;
            }
        };

        match protocol::parse_workflow_toml_document(&content) {
            Ok(protocol::WorkflowDocument::LegacyV1 { workflow: def }) => {
                println!("[OK]   {} -> legacy workflow {}", path.display(), def.id);
                per_doc_failures.extend(
                    protocol::validate_workflow_document(&protocol::WorkflowDocument::LegacyV1 {
                        workflow: def.clone(),
                    })
                    .errors,
                );
                legacy_defs.push(def);
            }
            Ok(protocol::WorkflowDocument::ReactiveV2 { workflow: def }) => {
                println!("[OK]   {} -> reactive workflow {}", path.display(), def.id);
                per_doc_failures.extend(
                    protocol::validate_workflow_document(&protocol::WorkflowDocument::ReactiveV2 {
                        workflow: def.clone(),
                    })
                    .errors,
                );
                reactive_defs.push(def);
            }
            Err(e) => {
                eprintln!("[FAIL] {} -> parse error: {}", path.display(), e);
                return 1;
            }
        }
    }

    let mut failures = Vec::new();
    failures.extend(per_doc_failures);
    let legacy_report = protocol::validate_workflows(&legacy_defs);
    failures.extend(legacy_report.errors);
    failures.extend(protocol::validate_reactive_workflows(&reactive_defs, &legacy_defs).errors);

    if failures.is_empty() {
        println!(
            "All workflows valid ({} legacy, {} reactive).",
            legacy_defs.len(),
            reactive_defs.len()
        );
        0
    } else {
        for e in failures {
            eprintln!("[FAIL] workflow {}: {}", e.workflow_id, e.message);
        }
        1
    }
}
