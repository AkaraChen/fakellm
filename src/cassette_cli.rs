//! `fakellm cassette`: offline tools for cassette files (docs/m4-spec.md section 3.5).
//!
//! ```text
//! fakellm cassette to-trajectory recording.jsonl -o fix-bug.jsonl --with-expect
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use fakellm::CassetteFormat;
use fakellm::ToTrajectory;

#[derive(Args)]
pub struct Cassette {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Turn the assistant replies of a cassette (JSON, JSONL or YAML) into a trajectory.
    ToTrajectory {
        file: PathBuf,
        /// Output file; `-` writes to stdout.
        #[arg(short, long)]
        output_file: PathBuf,
        /// Trajectory name; default: the cassette's.
        #[arg(long)]
        name: Option<String>,
        /// Add `expect.tool_results` (tool name and call id) to every step that follows tool calls.
        #[arg(long)]
        with_expect: bool,
        /// Keep error outcomes as error steps.
        #[arg(long)]
        keep_errors: bool,
        /// Only exchanges whose request model matches this glob.
        #[arg(long)]
        model: Option<String>,
        /// Write one JSON document instead of JSONL.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cassette: Cassette) -> ExitCode {
    match execute(cassette.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("fakellm cassette: {message}");
            ExitCode::FAILURE
        }
    }
}

fn execute(command: Command) -> Result<(), String> {
    let Command::ToTrajectory {
        file,
        output_file,
        name,
        with_expect,
        keep_errors,
        model,
        json,
    } = command;
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let format = match file.extension().and_then(|e| e.to_str()) {
        Some("json") => CassetteFormat::Json,
        Some("jsonl" | "ndjson") => CassetteFormat::Jsonl,
        Some("yaml" | "yml") => CassetteFormat::Yaml,
        _ => CassetteFormat::Auto,
    };
    let mut cassettes = fakellm::Cassette::parse_many(&text, format).map_err(|e| format!("{}: {e}", file.display()))?;
    if cassettes.len() != 1 {
        return Err(format!("{}: expected one cassette, found {}", file.display(), cassettes.len()));
    }
    let cassette = cassettes.remove(0);
    let trajectory = cassette
        .to_trajectory(&ToTrajectory {
            name,
            with_expect,
            keep_errors,
            model,
        })
        .map_err(|e| e.to_string())?;
    eprintln!("{} of {} exchanges kept", trajectory.steps.len(), cassette.exchanges.len());
    let out = if json {
        let mut text = serde_json::to_string_pretty(&trajectory.to_json()).map_err(|e| e.to_string())?;
        text.push('\n');
        text
    } else {
        trajectory.to_jsonl()
    };
    if output_file.as_os_str() == "-" {
        print!("{out}");
        Ok(())
    } else {
        std::fs::write(&output_file, out).map_err(|e| format!("{}: {e}", output_file.display()))
    }
}
