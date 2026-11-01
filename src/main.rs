use std::io::Write;

use clap::Parser;
use clap::builder::BoolishValueParser;

mod cassette_cli;
mod ctl;
mod import_cli;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(version, about = "An in-memory, OpenAI-compatible LLM API server for tests")]
struct Opt {
    /// Talk to a running server's control plane instead of serving.
    #[command(subcommand)]
    command: Option<Cmd>,

    #[arg(long, default_value = "127.0.0.1", env = "FAKELLM_HOST")]
    host: String,

    /// Port to listen on; 0 picks a free port. The bound address is printed to stdout.
    #[arg(long, default_value_t = 4010, env = "FAKELLM_PORT")]
    port: u16,

    /// Require this exact API key (Bearer token or x-api-key). Without it any key is accepted.
    #[arg(long, env = "FAKELLM_API_KEY")]
    api_key: Option<String>,

    /// Models listed by GET /v1/models, comma-separated. Any model name is accepted by the chat
    /// endpoint regardless.
    #[arg(long, env = "FAKELLM_MODELS", value_delimiter = ',')]
    models: Vec<String>,

    /// Fail requests that no script rule matches instead of answering with the default reply.
    #[arg(long, env = "FAKELLM_STRICT", value_parser = BoolishValueParser::new(), num_args = 0..=1,
        default_missing_value = "true", default_value_t = false)]
    strict: bool,

    /// Script rules to load at start: a JSON or YAML file (see docs/scripts.md).
    #[arg(long, env = "FAKELLM_SCRIPTS")]
    scripts: Option<std::path::PathBuf>,

    /// Trajectories to load at start (repeatable): a JSON, YAML or JSONL file, see docs/agent.md.
    #[arg(long = "trajectory", env = "FAKELLM_TRAJECTORIES", value_delimiter = ',', value_name = "FILE")]
    trajectories: Vec<std::path::PathBuf>,

    /// Policy scripts to load at start (repeatable): an ES module exporting `decide(request, context)`;
    /// the policy is named after the file stem.
    #[arg(long = "policy", env = "FAKELLM_POLICIES", value_delimiter = ',', value_name = "FILE")]
    policies: Vec<std::path::PathBuf>,

    /// Runtime for policy scripts: tjs (embedded in this binary, runs in-process), node, bun or a path to an executable.
    /// Default: tjs, or node then bun from PATH in a build without the `tjs` feature.
    #[arg(long, env = "FAKELLM_POLICY_RUNTIME")]
    policy_runtime: Option<fakellm::Runtime>,

    /// How long a policy may take to decide, in milliseconds (host clock).
    #[arg(long, default_value_t = 5000, env = "FAKELLM_POLICY_TIMEOUT_MS")]
    policy_timeout_ms: u64,

    /// Also serve the API under this path prefix, e.g. /anthropic for /anthropic/v1/messages.
    #[arg(long, env = "FAKELLM_PATH_PREFIX")]
    path_prefix: Option<String>,

    /// Cassettes to load at start (repeatable): a JSON, YAML or JSONL file, see docs/cassette.md. Requests no script
    /// rule answers are looked up in them by meaning, not by bytes.
    #[arg(long = "cassette", env = "FAKELLM_CASSETTES", value_delimiter = ',', value_name = "FILE")]
    cassettes: Vec<std::path::PathBuf>,

    /// Relay requests that no script rule and no cassette answers to this OpenAI-compatible or Anthropic server
    /// (http:// or https://), in the request's own protocol, and record what it answers.
    #[arg(long, env = "FAKELLM_UPSTREAM", value_name = "URL")]
    upstream: Option<http::Uri>,

    /// API key sent to the upstream instead of the client's own credentials. Without it the client's
    /// `authorization` and `x-api-key` headers are passed through.
    #[arg(long, env = "FAKELLM_UPSTREAM_API_KEY", requires = "upstream")]
    upstream_api_key: Option<String>,

    /// Append every exchange recorded from the upstream to this file as a JSONL cassette.
    #[arg(long, env = "FAKELLM_RECORD", requires = "upstream", value_name = "FILE")]
    record: Option<std::path::PathBuf>,

    /// Also record the upstream's 401, 403, 408, 409, 429 and 5xx answers (by default they are relayed but not
    /// recorded, so a retry that succeeded is not shadowed on replay).
    #[arg(long, requires = "record")]
    record_errors: bool,

    /// Replace every match of this regex in the recorded texts (messages, tool arguments and results, extra,
    /// replies) with "<redacted>". Repeatable.
    #[arg(long, value_name = "REGEX")]
    record_redact: Vec<String>,

    /// Seconds the upstream may take to send response headers; 0 means no limit.
    #[arg(long, env = "FAKELLM_UPSTREAM_TIMEOUT", requires = "upstream", default_value_t = 600)]
    upstream_timeout: u64,

    /// Do not reload --scripts, --trajectory, --policy and --cassette files when they change.
    #[arg(long, env = "FAKELLM_NO_WATCH", value_parser = BoolishValueParser::new(), num_args = 0..=1,
        default_missing_value = "true", default_value_t = false)]
    no_watch: bool,

    /// Length of the default reply, in tokens.
    #[arg(long, default_value_t = 32, env = "FAKELLM_FALLBACK_TOKENS")]
    fallback_tokens: u32,

    /// Default time to first token, e.g. 300ms (server clock). Rules can override it.
    #[arg(long, default_value = "0", env = "FAKELLM_TTFT")]
    ttft: String,

    /// Default streaming speed in tokens per second; 0 means no gap between chunks.
    #[arg(long, default_value_t = 0.0, env = "FAKELLM_TOKENS_PER_SEC")]
    tokens_per_sec: f64,

    /// Tokens per streamed content chunk; 1 mirrors the real APIs.
    #[arg(long, default_value_t = 1, env = "FAKELLM_CHUNK_TOKENS", value_parser = at_least_one)]
    chunk_tokens: usize,

    /// Bytes of tool call arguments per streamed fragment.
    #[arg(long, default_value_t = 16, env = "FAKELLM_ARGS_CHUNK_BYTES", value_parser = at_least_one)]
    args_chunk_bytes: usize,

    /// Do not record API requests for the control plane's request log.
    #[arg(long, env = "FAKELLM_NO_REQUEST_LOG", value_parser = BoolishValueParser::new(), num_args = 0..=1,
        default_missing_value = "true", default_value_t = false)]
    no_request_log: bool,

    /// Requests kept by the request log; the oldest are dropped first.
    #[arg(long, default_value_t = fakellm::DEFAULT_REQUEST_LOG_CAPACITY, env = "FAKELLM_REQUEST_LOG_CAPACITY")]
    request_log_capacity: usize,

    /// Store "<redacted>" instead of authorization, proxy-authorization and x-api-key values in
    /// the request log.
    #[arg(long, env = "FAKELLM_REDACT_AUTH", value_parser = BoolishValueParser::new(), num_args = 0..=1,
        default_missing_value = "true", default_value_t = false)]
    redact_auth: bool,

    /// Most named snapshots held at once; more get a 409.
    #[arg(long, default_value_t = fakellm::MAX_SNAPSHOTS, env = "FAKELLM_MAX_SNAPSHOTS")]
    max_snapshots: usize,
}

fn at_least_one(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1".into()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

#[allow(clippy::large_enum_variant)] // parsed once per process
#[derive(clap::Subcommand)]
enum Cmd {
    /// Control a running server: reset, scripts, trajectories, policies, cassettes, snapshots, clock, request log.
    Ctl(ctl::Ctl),
    /// Convert Hugging Face dataset rows (or a local dump of them) into a cassette or trajectories.
    Import(import_cli::Import),
    /// Offline tools for cassette files.
    Cassette(cassette_cli::Cassette),
    /// Print the configuration that points a client at a running fakellm.
    Wire {
        /// One of: claude, codex, opencode, pi, openai-sdk, ai-sdk, anthropic-sdk.
        target: String,
        /// Server to point the client at.
        #[arg(long, default_value = "http://127.0.0.1:4010", env = "FAKELLM_ENDPOINT")]
        endpoint: String,
        /// env, json, toml or shell (default: shell, or toml/json for targets without env vars).
        #[arg(long)]
        format: Option<fakellm::wire::Format>,
        /// Print one generated file raw (e.g. models.json, config.toml, opencode.json) instead.
        #[arg(long, conflicts_with = "format", value_name = "NAME")]
        file: Option<String>,
    },
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fakellm: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> std::io::Result<std::process::ExitCode> {
    let opt = Opt::parse();
    match opt.command {
        Some(Cmd::Ctl(c)) => {
            return Ok(tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(ctl::run(c)));
        }
        Some(Cmd::Import(i)) => {
            return Ok(tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(import_cli::run(i)));
        }
        Some(Cmd::Cassette(c)) => return Ok(cassette_cli::run(c)),
        Some(Cmd::Wire {
            target,
            endpoint,
            format,
            file,
        }) => return Ok(run_wire(&target, &endpoint, format, file.as_deref())),
        None => {}
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,fakellm::watch=info,fakellm::upstream=info".into()),
        )
        .init();
    let invalid = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    let scripts = match &opt.scripts {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
            let format = match path.extension().and_then(|e| e.to_str()) {
                Some("json") => fakellm::ScriptFormat::Json,
                Some("yaml" | "yml") => fakellm::ScriptFormat::Yaml,
                _ => fakellm::ScriptFormat::Auto,
            };
            fakellm::Rule::parse_many(&text, format).map_err(|e| invalid(format!("{}: {e}", path.display())))?
        }
        None => Vec::new(),
    };
    let mut trajectories = Vec::new();
    for path in &opt.trajectories {
        let text = std::fs::read_to_string(path).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
        let format = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => fakellm::TrajectoryFormat::Json,
            Some("jsonl" | "ndjson") => fakellm::TrajectoryFormat::Jsonl,
            Some("yaml" | "yml") => fakellm::TrajectoryFormat::Yaml,
            _ => fakellm::TrajectoryFormat::Auto,
        };
        trajectories.extend(fakellm::Trajectory::parse_many(&text, format).map_err(|e| invalid(format!("{}: {e}", path.display())))?);
    }
    let policies = opt
        .policies
        .iter()
        .map(|path| fakellm::PolicySpec::from_path(path).map_err(|e| invalid(format!("{}: {e}", path.display()))))
        .collect::<Result<Vec<_>, _>>()?;
    let mut cassettes = Vec::new();
    for path in &opt.cassettes {
        let text = std::fs::read_to_string(path).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
        let format = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => fakellm::CassetteFormat::Json,
            Some("jsonl" | "ndjson") => fakellm::CassetteFormat::Jsonl,
            Some("yaml" | "yml") => fakellm::CassetteFormat::Yaml,
            _ => fakellm::CassetteFormat::Auto,
        };
        cassettes.extend(fakellm::Cassette::parse_many(&text, format).map_err(|e| invalid(format!("{}: {e}", path.display())))?);
    }
    let watch = (!opt.no_watch).then(|| fakellm::WatchConfig {
        scripts: opt.scripts.clone(),
        trajectories: opt.trajectories.clone(),
        policies: opt.policies.clone(),
        cassettes: opt.cassettes.clone(),
    });
    let ttft = fakellm::parse_duration(&opt.ttft).map_err(invalid)?;
    if ttft < 0 {
        return Err(invalid("--ttft must not be negative".into()));
    }
    let listen = format!("{}:{}", opt.host, opt.port);
    let config = fakellm::Config {
        host: opt.host,
        port: opt.port,
        api_key: opt.api_key,
        models: opt.models.into_iter().filter(|m| !m.is_empty()).collect(),
        strict: opt.strict,
        fallback: fakellm::FallbackConfig {
            tokens: opt.fallback_tokens,
        },
        delivery: fakellm::Delivery {
            ttft: std::time::Duration::from_nanos(ttft as u64),
            token_interval: if opt.tokens_per_sec > 0.0 {
                std::time::Duration::try_from_secs_f64(1.0 / opt.tokens_per_sec).unwrap_or(std::time::Duration::MAX)
            } else {
                std::time::Duration::ZERO
            },
            chunk_tokens: opt.chunk_tokens,
            args_chunk_bytes: opt.args_chunk_bytes,
            ..Default::default()
        },
        scripts,
        request_log: !opt.no_request_log,
        request_log_capacity: opt.request_log_capacity,
        redact_auth: opt.redact_auth,
        max_snapshots: opt.max_snapshots,
        path_prefix: opt.path_prefix,
        trajectories,
        policies,
        policy_runtime: opt.policy_runtime,
        policy_timeout_ms: opt.policy_timeout_ms,
        cassettes,
        upstream: opt.upstream,
        upstream_api_key: opt.upstream_api_key,
        record: opt.record,
        record_errors: opt.record_errors,
        record_redact: opt.record_redact,
        upstream_timeout_secs: opt.upstream_timeout,
        watch,
    };
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        let mut server = fakellm::Server::start(config).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrInUse => std::io::Error::new(e.kind(), format!("cannot listen on {listen}: address already in use")),
            _ => e,
        })?;
        println!("{}", server.endpoint());
        std::io::stdout().flush()?;
        tokio::select! {
            () = server.wait_ref() => {}
            () = shutdown_signal() => server.shutdown().await,
        }
        Ok(std::process::ExitCode::SUCCESS)
    })
}

/// Resolves on SIGINT or SIGTERM, so policy subprocesses are killed and their temp dirs removed on the way out.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Prints the client configuration; usage errors exit with 2.
fn run_wire(target: &str, endpoint: &str, format: Option<fakellm::wire::Format>, file: Option<&str>) -> std::process::ExitCode {
    let result = fakellm::wire::wire(target, endpoint).and_then(|out| {
        if let Some(name) = file {
            return fakellm::wire::file(&out, name).map(str::to_owned);
        }
        let format = format.unwrap_or_else(|| fakellm::wire::default_format(&out));
        fakellm::wire::format(&out, format)
    });
    match result {
        Ok(text) => {
            print!("{text}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fakellm wire: {e}");
            std::process::ExitCode::from(2)
        }
    }
}
