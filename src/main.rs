use std::io::Write;

use clap::Parser;

mod ctl;

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
    #[arg(long, env = "FAKELLM_STRICT")]
    strict: bool,

    /// Script rules to load at start: a JSON or YAML file (see docs/scripts.md).
    #[arg(long, env = "FAKELLM_SCRIPTS")]
    scripts: Option<std::path::PathBuf>,

    /// Length of the default reply, in tokens.
    #[arg(long, default_value_t = 32)]
    fallback_tokens: u32,

    /// Default time to first token, e.g. 300ms (server clock). Rules can override it.
    #[arg(long, default_value = "0")]
    ttft: String,

    /// Default streaming speed in tokens per second; 0 means no gap between chunks.
    #[arg(long, default_value_t = 0.0)]
    tokens_per_sec: f64,

    /// Do not record API requests for the control plane's request log.
    #[arg(long)]
    no_request_log: bool,

    /// Requests kept by the request log; the oldest are dropped first.
    #[arg(long, default_value_t = fakellm::DEFAULT_REQUEST_LOG_CAPACITY)]
    request_log_capacity: usize,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Control a running server: reset, scripts, clock, request log.
    Ctl(ctl::Ctl),
}

fn main() -> std::io::Result<std::process::ExitCode> {
    let opt = Opt::parse();
    if let Some(Cmd::Ctl(c)) = opt.command {
        return Ok(tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(ctl::run(c)));
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let invalid = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    let scripts = match &opt.scripts {
        Some(path) => {
            let text = std::fs::read_to_string(path)?;
            let format = match path.extension().and_then(|e| e.to_str()) {
                Some("json") => fakellm::ScriptFormat::Json,
                Some("yaml" | "yml") => fakellm::ScriptFormat::Yaml,
                _ => fakellm::ScriptFormat::Auto,
            };
            fakellm::Rule::parse_many(&text, format).map_err(|e| invalid(format!("{}: {e}", path.display())))?
        }
        None => Vec::new(),
    };
    let ttft = fakellm::parse_duration(&opt.ttft).map_err(invalid)?;
    if ttft < 0 {
        return Err(invalid("--ttft must not be negative".into()));
    }
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
            ..Default::default()
        },
        scripts,
        request_log: !opt.no_request_log,
        request_log_capacity: opt.request_log_capacity,
    };
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        let server = fakellm::Server::start(config).await?;
        println!("{}", server.endpoint());
        std::io::stdout().flush()?;
        server.wait().await;
        Ok(std::process::ExitCode::SUCCESS)
    })
}
