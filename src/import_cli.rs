//! `fakellm import`: convert dataset rows into a cassette or trajectories file (docs/m4-spec.md section 6).
//!
//! ```text
//! fakellm import hf nebius/SWE-agent-trajectories --split train --limit 200 --format swe-agent -o fixtures/swe.jsonl
//! fakellm import file rows.jsonl --format sharegpt --as cassette -o fixtures/chat.jsonl
//! ```

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use fakellm::import::hf::{self, HttpClient, Source as HfSource};
use fakellm::import::{BuildOptions, Converted, Format, ImportError, Output, convert_rows, parse_rows, to_cassette, to_trajectories};
use serde_json::Value;

#[derive(Args)]
pub struct Import {
    #[command(subcommand)]
    source: Source,
}

#[derive(Subcommand)]
enum Source {
    /// Fetch rows of a Hugging Face dataset through the datasets-server API (HF_TOKEN is used for gated datasets).
    Hf {
        /// `owner/name`, e.g. allenai/WildChat-1M.
        dataset: String,
        /// Dataset config; default: the first one that has the split.
        #[arg(long)]
        config: Option<String>,
        #[arg(long, default_value = "train")]
        split: String,
        /// Rows to fetch.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// First row.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Convert rows from a local file: a JSON array, `{"rows": [...]}`, or JSONL with one row per line (`-` reads stdin).
    File {
        path: PathBuf,
        #[command(flatten)]
        out: OutputArgs,
    },
}

#[derive(Args)]
struct OutputArgs {
    /// auto, sharegpt, wildchat, hermes-fc, swe-agent, openhands or xlam.
    #[arg(long, default_value = "auto")]
    format: Format,
    /// cassette or trajectory; default: cassette for sharegpt and wildchat, trajectory for the agent formats.
    #[arg(long = "as")]
    output: Option<Output>,
    /// Cassette name, or prefix of the trajectory names; default: the dataset name or file stem.
    #[arg(long)]
    name: Option<String>,
    /// Model recorded in the exchanges of rows that name none.
    #[arg(long, default_value = "imported")]
    model: String,
    /// Output file (JSONL); `-` writes to stdout.
    #[arg(short, long)]
    output_file: PathBuf,
}

pub async fn run(import: Import) -> ExitCode {
    match run_inner(import).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fakellm import: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_inner(import: Import) -> Result<(), ImportError> {
    match import.source {
        Source::Hf {
            dataset,
            config,
            split,
            limit,
            offset,
            out,
        } => {
            let source = HfSource {
                dataset: dataset.clone(),
                config,
                split,
                offset,
                limit,
            };
            let token = std::env::var("HF_TOKEN").ok().filter(|t| !t.is_empty());
            let fetched = hf::fetch_rows(&HttpClient::new(), &hf::endpoint(), &source, token.as_deref()).await?;
            let head = format!(
                "fetched {} rows ({} truncated)",
                fetched.rows.len() + fetched.truncated,
                fetched.truncated
            );
            convert_and_write(&fetched.rows, &out, &head, &format!("hf:{dataset}"), &dataset.replace('/', "-"))
        }
        Source::File { path, out } => {
            let text = if path == Path::new("-") {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .map_err(|e| ImportError(format!("reading stdin: {e}")))?;
                text
            } else {
                std::fs::read_to_string(&path).map_err(|e| ImportError(format!("reading {}: {e}", path.display())))?
            };
            let rows = parse_rows(&text)?;
            let stem = if path == Path::new("-") {
                "stdin".to_owned()
            } else {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "import".into())
            };
            let head = format!("read {} rows", rows.len());
            convert_and_write(&rows, &out, &head, &format!("file:{}", path.display()), &stem)
        }
    }
}

fn convert_and_write(rows: &[Value], args: &OutputArgs, head: &str, source: &str, default_name: &str) -> Result<(), ImportError> {
    let converted = convert_rows(rows, args.format);
    // With `--format auto` the rows decide: conversations that all call tools are agent runs.
    let output = args.output.unwrap_or_else(|| match args.format {
        Format::Auto
            if !converted.conversations.is_empty()
                && converted
                    .conversations
                    .iter()
                    .all(|c| c.messages.iter().any(|m| !m.tool_calls.is_empty())) =>
        {
            Output::Trajectory
        }
        f => f.default_output(),
    });
    let name = args.name.clone().unwrap_or_else(|| default_name.to_owned()).replace('/', "-");
    let opts = BuildOptions {
        name: if name == "recording" { "recording-import".to_owned() } else { name },
        source: source.to_owned(),
        model: args.model.clone(),
    };
    if converted.conversations.is_empty() {
        return Err(ImportError(format!("{head}, but nothing converted: {}", skipped_text(&converted))));
    }
    let (text, produced) = match output {
        Output::Cassette => {
            let cassette = to_cassette(&converted.conversations, &opts);
            (cassette.to_jsonl(), format!("{} exchanges", cassette.exchanges.len()))
        }
        Output::Trajectory => {
            let trajectories = to_trajectories(&converted.conversations, &opts);
            for t in &trajectories {
                t.validate()
                    .map_err(|e| ImportError(format!("the converted trajectory is invalid: {e}")))?;
            }
            let steps: usize = trajectories.iter().map(|t| t.steps.len()).sum();
            (
                trajectories.iter().map(|t| t.to_jsonl()).collect::<String>(),
                format!("{steps} steps"),
            )
        }
    };
    if args.output_file == Path::new("-") {
        std::io::stdout()
            .write_all(text.as_bytes())
            .map_err(|e| ImportError(format!("writing stdout: {e}")))?;
    } else {
        std::fs::write(&args.output_file, &text).map_err(|e| ImportError(format!("writing {}: {e}", args.output_file.display())))?;
    }
    eprintln!(
        "{head}, converted {} conversations -> {produced}, skipped {} {}",
        converted.conversations.len(),
        converted.skipped,
        skipped_reasons(&converted)
    );
    Ok(())
}

fn skipped_reasons(c: &Converted) -> String {
    if c.reasons.is_empty() {
        return "(none)".to_owned();
    }
    let list: Vec<String> = c.reasons.iter().map(|(r, n)| format!("{r}: {n}")).collect();
    format!("({})", list.join(", "))
}

fn skipped_text(c: &Converted) -> String {
    format!("skipped {} {}", c.skipped, skipped_reasons(c))
}
