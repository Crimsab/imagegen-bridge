//! Generates checked-in schemas for the Imagegen Bridge wire contract.

use std::{env, fs, path::PathBuf, process::ExitCode};

const DEFAULT_PATH: &str = "schemas/imagegen-bridge-v1.schema.json";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("schema-gen: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut check = false;
    let mut output = PathBuf::from(DEFAULT_PATH);
    for argument in env::args().skip(1) {
        if argument == "--check" {
            check = true;
        } else if argument == "--help" || argument == "-h" {
            println!("Usage: imagegen-bridge-schema-gen [--check] [OUTPUT]");
            return Ok(());
        } else if output != std::path::Path::new(DEFAULT_PATH) {
            return Err("only one output path may be supplied".to_owned());
        } else {
            output = PathBuf::from(argument);
        }
    }

    let schema = imagegen_bridge_core::contract_schema();
    let mut rendered = serde_json::to_string_pretty(&schema)
        .map_err(|error| format!("could not serialize schema: {error}"))?;
    rendered.push('\n');

    if check {
        let existing = fs::read_to_string(&output)
            .map_err(|error| format!("could not read {}: {error}", output.display()))?;
        if existing != rendered {
            return Err(format!(
                "{} is stale; regenerate it without --check",
                output.display()
            ));
        }
        return Ok(());
    }

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    fs::write(&output, rendered)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    Ok(())
}
