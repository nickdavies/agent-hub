//! Generate JSON Schema files from the protocol crate's wire-format types.
//!
//! Usage:
//!   cargo run -p protocol --bin generate_schemas -- <output_dir>
//!
//! This writes one `.schema.json` file per type into `<output_dir>/`.
//! The generated schemas are consumed by `json-schema-to-typescript` to produce
//! TypeScript type definitions for the provider plugins.

use std::{fs, path::Path, path::PathBuf};

use schemars::{JsonSchema, generate::SchemaSettings};

/// Generate a JSON Schema for type `T` and write it to `<dir>/<name>.schema.json`.
fn write_schema<T: JsonSchema>(dir: &Path, name: &str) {
    let settings = SchemaSettings::draft2020_12();
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<T>();
    let json = serde_json::to_string_pretty(&schema).expect("schema serialization failed");
    let path = dir.join(format!("{name}.schema.json"));
    fs::write(&path, format!("{json}\n")).unwrap_or_else(|e| {
        panic!("failed to write {}: {e}", path.display());
    });
    eprintln!("  wrote {}", path.display());
}

fn main() {
    let out_dir: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| {
            eprintln!("Usage: generate_schemas <output_dir>");
            std::process::exit(1);
        })
        .into();

    fs::create_dir_all(&out_dir).unwrap_or_else(|e| {
        panic!("failed to create {}: {e}", out_dir.display());
    });

    eprintln!("Generating JSON schemas into {}", out_dir.display());

    // -- OpenCode gateway types (plugin ↔ gateway boundary) --
    write_schema::<protocol::OpenCodeHookInput>(&out_dir, "OpenCodeHookInput");
    write_schema::<protocol::OpenCodeHookOutput>(&out_dir, "OpenCodeHookOutput");
    write_schema::<protocol::OpenCodeTool>(&out_dir, "OpenCodeTool");

    // -- Status reporting types --
    write_schema::<protocol::StatusReport>(&out_dir, "StatusReport");

    // -- Session types used in status payloads --
    write_schema::<protocol::SessionStatus>(&out_dir, "SessionStatus");
    write_schema::<protocol::Provider>(&out_dir, "Provider");

    eprintln!("Done.");
}
