//! `fakellm wire <target>`: the configuration that points a real client at a running fakellm.
//! Pure functions from an endpoint to environment variables, config files and a code snippet, so
//! the CLI, tests and future language bindings share one source of truth. Formats per target are
//! in docs/m1-spec.md ("fakellm wire").

use serde_json::{Map, Value, json};

/// Targets accepted by [`wire`], in help-text order.
pub const TARGETS: &[&str] = &["claude", "codex", "opencode", "pi", "openai-sdk", "ai-sdk", "anthropic-sdk"];

/// What a client needs to talk to the server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WireOutput {
    /// Environment variables to export, in a stable order.
    pub env: Vec<(String, String)>,
    /// `(file name, contents)` of config files to write; names are suggestions such as
    /// `opencode.json` or `config.toml`, relative to the client's config directory.
    pub files: Vec<(String, String)>,
    /// Human-oriented text: a code snippet or a usage note, ending with a newline.
    pub snippet: String,
}

/// Output formats of the CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `KEY=value` lines (what `--format env` prints; nothing else).
    Env,
    /// One JSON object `{"env": {..}, "files": {name: contents}, "snippet": ".."}`.
    Json,
    /// The contents of the first `.toml` file; an error when the target has none.
    Toml,
    /// `export KEY='value'` lines (values single-quoted, quotes escaped).
    Shell,
}

impl std::str::FromStr for Format {
    type Err = String;

    /// `env`, `json`, `toml` or `shell`.
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "env" => Ok(Self::Env),
            "json" => Ok(Self::Json),
            "toml" => Ok(Self::Toml),
            "shell" => Ok(Self::Shell),
            _ => Err(format!("unknown format {s:?}, expected env, json, toml or shell")),
        }
    }
}

/// The format the CLI uses without `--format`: `shell` when there are environment variables, else
/// `toml` when there is a `.toml` file, else `json`.
pub fn default_format(out: &WireOutput) -> Format {
    if !out.env.is_empty() {
        Format::Shell
    } else if out.files.iter().any(|(name, _)| name.ends_with(".toml")) {
        Format::Toml
    } else {
        Format::Json
    }
}

/// Strips whitespace, a trailing `/` and a trailing `/v1`, so the same endpoint can be given
/// with or without the OpenAI base path.
fn normalize(endpoint: &str) -> Result<String, String> {
    let e = endpoint.trim().trim_end_matches('/');
    let e = e.strip_suffix("/v1").unwrap_or(e).trim_end_matches('/');
    let host = e.strip_prefix("http://").or_else(|| e.strip_prefix("https://"));
    match host {
        Some(h) if !h.is_empty() => Ok(e.to_owned()),
        _ => Err(format!(
            "invalid endpoint {endpoint:?}, expected http://host:port or https://host:port"
        )),
    }
}

/// Builds the output for `target` (one of [`TARGETS`]) and a server `endpoint` such as
/// `http://127.0.0.1:4010`. A trailing `/` or `/v1` on the endpoint is stripped first. Errors
/// name the valid targets for an unknown one, and reject endpoints that do not start with
/// `http://` or `https://`.
pub fn wire(target: &str, endpoint: &str) -> Result<WireOutput, String> {
    if !TARGETS.contains(&target) {
        return Err(format!("unknown target {target:?}, expected one of: {}", TARGETS.join(", ")));
    }
    let e = normalize(endpoint)?;
    let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    Ok(match target {
        "claude" => WireOutput {
            env: env(&[("ANTHROPIC_BASE_URL", &e), ("ANTHROPIC_API_KEY", "fakellm")]),
            files: Vec::new(),
            snippet: "# Claude Code prefers ANTHROPIC_AUTH_TOKEN (sent as a Bearer token) when it is set.\n\
                      # fakellm accepts both headers; unset it, or set it to fakellm.\n\
                      # Model names such as sonnet or glm-5.2[1m] are accepted as they are.\n"
                .to_owned(),
        },
        "codex" => WireOutput {
            env: env(&[("FAKELLM_API_KEY", "fakellm")]),
            files: vec![("config.toml".into(), codex_toml(&e))],
            snippet: format!(
                "FAKELLM_API_KEY=fakellm codex -c model=gpt-5.5 -c model_provider=fakellm \
                 -c 'model_providers.fakellm={{ name = \"fakellm\", base_url = \"{e}/v1\", env_key = \"FAKELLM_API_KEY\", \
                 wire_api = \"responses\", request_max_retries = 0, stream_max_retries = 0, stream_idle_timeout_ms = 5000 }}'\n"
            ),
        },
        "opencode" => {
            let config = opencode_json(&e);
            WireOutput {
                env: env(&[("OPENCODE_CONFIG_CONTENT", &config.to_string())]),
                files: vec![("opencode.json".into(), pretty(&config))],
                snippet: "# OPENCODE_CONFIG_CONTENT above is the whole config. For a file instead, save the output of \
                          `fakellm wire opencode --file opencode.json` as opencode.json in the project root or \
                          ~/.config/opencode/.\n"
                    .to_owned(),
            }
        }
        "pi" => {
            let models = json!({"providers": {"fakellm": {
                "baseUrl": format!("{e}/v1"),
                "api": "openai-completions",
                "apiKey": "fakellm",
                "models": [{"id": "fake-model"}],
            }}});
            WireOutput {
                env: Vec::new(),
                files: vec![("models.json".into(), pretty(&models))],
                snippet: "# Save as ~/.pi/agent/models.json, then pick the fakellm/fake-model model.\n".to_owned(),
            }
        }
        "openai-sdk" => WireOutput {
            snippet: format!(
                "// JavaScript\n\
                 import OpenAI from \"openai\";\n\
                 const client = new OpenAI({{ baseURL: \"{e}/v1\", apiKey: \"fakellm\" }});\n\n\
                 # Python\n\
                 from openai import OpenAI\n\
                 client = OpenAI(base_url=\"{e}/v1\", api_key=\"fakellm\")\n"
            ),
            ..WireOutput::default()
        },
        "ai-sdk" => WireOutput {
            snippet: format!(
                "import {{ createOpenAI }} from \"@ai-sdk/openai\";\n\
                 import {{ createAnthropic }} from \"@ai-sdk/anthropic\";\n\
                 const openai = createOpenAI({{ baseURL: \"{e}/v1\", apiKey: \"fakellm\" }});\n\
                 const anthropic = createAnthropic({{ baseURL: \"{e}/v1\", apiKey: \"fakellm\" }});\n"
            ),
            ..WireOutput::default()
        },
        "anthropic-sdk" => WireOutput {
            snippet: format!(
                "// JavaScript\n\
                 import Anthropic from \"@anthropic-ai/sdk\";\n\
                 const client = new Anthropic({{ baseURL: \"{e}\", apiKey: \"fakellm\" }});\n\n\
                 # Python\n\
                 from anthropic import Anthropic\n\
                 client = Anthropic(base_url=\"{e}\", api_key=\"fakellm\")\n"
            ),
            ..WireOutput::default()
        },
        _ => unreachable!("checked against TARGETS"),
    })
}

fn codex_toml(e: &str) -> String {
    format!(
        "model = \"gpt-5.5\"\n\
         model_provider = \"fakellm\"\n\n\
         [model_providers.fakellm]\n\
         name = \"fakellm\"\n\
         base_url = \"{e}/v1\"\n\
         # env_key is optional: with it codex sends that variable as the bearer token and refuses to\n\
         # start when it is unset. Only needed when fakellm runs with --api-key.\n\
         # env_key = \"FAKELLM_API_KEY\"\n\
         wire_api = \"responses\"\n\
         request_max_retries = 0\n\
         stream_max_retries = 0\n\
         stream_idle_timeout_ms = 5000\n"
    )
}

fn opencode_json(e: &str) -> Value {
    json!({
        "$schema": "https://opencode.ai/config.json",
        "model": "fakellm/fake-model",
        "provider": {"fakellm": {
            "npm": "@ai-sdk/openai-compatible",
            "name": "fakellm",
            "options": {"baseURL": format!("{e}/v1"), "apiKey": "fakellm"},
            "models": {"fake-model": {"name": "fake-model", "tool_call": true}},
        }},
    })
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    s
}

fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', r"'\''"))
}

/// The raw contents of the generated file called `name` (`--file`); the error lists the names.
pub fn file<'a>(out: &'a WireOutput, name: &str) -> Result<&'a str, String> {
    out.files
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, text)| text.as_str())
        .ok_or_else(|| {
            let names: Vec<&str> = out.files.iter().map(|(n, _)| n.as_str()).collect();
            if names.is_empty() {
                format!("this target has no files, so there is no {name:?}")
            } else {
                format!("no file {name:?}, expected one of: {}", names.join(", "))
            }
        })
}

/// Renders `out` for the CLI in `format`.
pub fn format(out: &WireOutput, format: Format) -> Result<String, String> {
    match format {
        Format::Env => Ok(out.env.iter().map(|(k, v)| format!("{k}={v}\n")).collect()),
        Format::Json => {
            let env: Map<String, Value> = out.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            let files: Map<String, Value> = out.files.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            Ok(pretty(&json!({"env": env, "files": files, "snippet": out.snippet})))
        }
        Format::Toml => out
            .files
            .iter()
            .find(|(name, _)| name.ends_with(".toml"))
            .map(|(_, text)| text.clone())
            .ok_or_else(|| "this target has no TOML file".to_owned()),
        Format::Shell => {
            let mut s: String = out.env.iter().map(|(k, v)| format!("export {k}={}\n", shell_quote(v))).collect();
            for (name, text) in &out.files {
                // A file whose JSON is already in an exported variable (opencode.json / OPENCODE_CONFIG_CONTENT) is not printed twice.
                let doc = serde_json::from_str::<Value>(text).ok();
                if doc.is_some() && out.env.iter().any(|(_, v)| serde_json::from_str::<Value>(v).ok() == doc) {
                    continue;
                }
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&format!("# {name}\n{text}"));
            }
            if !out.snippet.is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&out.snippet);
            }
            Ok(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const E: &str = "http://127.0.0.1:4010";

    fn get(target: &str) -> WireOutput {
        wire(target, E).unwrap()
    }

    #[test]
    fn claude() {
        let o = get("claude");
        assert_eq!(
            o.env,
            vec![
                ("ANTHROPIC_BASE_URL".to_owned(), E.to_owned()),
                ("ANTHROPIC_API_KEY".into(), "fakellm".into())
            ]
        );
        assert!(o.files.is_empty());
        assert!(o.snippet.contains("ANTHROPIC_AUTH_TOKEN"));
    }

    #[test]
    fn codex_config_is_exact() {
        let o = get("codex");
        assert_eq!(o.env, vec![("FAKELLM_API_KEY".to_owned(), "fakellm".to_owned())]);
        assert_eq!(
            o.files,
            vec![(
                "config.toml".to_owned(),
                "model = \"gpt-5.5\"\nmodel_provider = \"fakellm\"\n\n[model_providers.fakellm]\nname = \"fakellm\"\n\
                 base_url = \"http://127.0.0.1:4010/v1\"\n# env_key is optional: with it codex sends that variable as the bearer token and refuses to\n\
                 # start when it is unset. Only needed when fakellm runs with --api-key.\n# env_key = \"FAKELLM_API_KEY\"\nwire_api = \"responses\"\n\
                 request_max_retries = 0\nstream_max_retries = 0\nstream_idle_timeout_ms = 5000\n"
                    .to_owned()
            )]
        );
        assert_eq!(
            o.snippet,
            "FAKELLM_API_KEY=fakellm codex -c model=gpt-5.5 -c model_provider=fakellm -c 'model_providers.fakellm={ name = \"fakellm\", \
             base_url = \"http://127.0.0.1:4010/v1\", env_key = \"FAKELLM_API_KEY\", wire_api = \"responses\", request_max_retries = 0, \
             stream_max_retries = 0, stream_idle_timeout_ms = 5000 }'\n"
        );
    }

    #[test]
    fn opencode_and_pi_are_json() {
        let o = get("opencode");
        let from_env: Value = serde_json::from_str(&o.env[0].1).unwrap();
        let from_file: Value = serde_json::from_str(&o.files[0].1).unwrap();
        assert_eq!(o.env[0].0, "OPENCODE_CONFIG_CONTENT");
        assert_eq!(from_env, from_file);
        assert!(!o.env[0].1.contains('\n'));
        assert_eq!(from_file["provider"]["fakellm"]["options"]["baseURL"], "http://127.0.0.1:4010/v1");
        assert_eq!(from_file["provider"]["fakellm"]["models"]["fake-model"]["tool_call"], true);
        assert_eq!(from_file["model"], "fakellm/fake-model");

        let o = get("pi");
        assert!(o.env.is_empty());
        let v: Value = serde_json::from_str(&o.files[0].1).unwrap();
        assert_eq!(o.files[0].0, "models.json");
        assert_eq!(v["providers"]["fakellm"]["baseUrl"], "http://127.0.0.1:4010/v1");
        assert_eq!(v["providers"]["fakellm"]["api"], "openai-completions");
        assert_eq!(v["providers"]["fakellm"]["models"][0]["id"], "fake-model");
        assert!(o.snippet.contains("~/.pi/agent/models.json"));
    }

    #[test]
    fn shell_prints_the_opencode_config_once() {
        let o = get("opencode");
        let shell = format(&o, Format::Shell).unwrap();
        assert_eq!(shell.matches("\"$schema\"").count(), 1, "{shell}");
        assert!(shell.starts_with("export OPENCODE_CONFIG_CONTENT="));
        assert!(shell.contains("--file opencode.json"));
        // Targets whose files are not in a variable keep printing them.
        let pi = format(&get("pi"), Format::Shell).unwrap();
        assert!(pi.contains("# models.json\n{"), "{pi}");
        let codex = format(&get("codex"), Format::Shell).unwrap();
        assert!(codex.contains("# config.toml"), "{codex}");
    }

    #[test]
    fn sdk_snippets() {
        assert!(get("openai-sdk").snippet.contains("baseURL: \"http://127.0.0.1:4010/v1\""));
        assert!(get("openai-sdk").snippet.contains("base_url=\"http://127.0.0.1:4010/v1\""));
        let a = get("anthropic-sdk").snippet;
        assert!(a.contains("baseURL: \"http://127.0.0.1:4010\""));
        assert!(!a.contains("/v1"));
        let ai = get("ai-sdk").snippet;
        assert!(ai.contains("createOpenAI({ baseURL: \"http://127.0.0.1:4010/v1\""));
        assert!(ai.contains("createAnthropic({ baseURL: \"http://127.0.0.1:4010/v1\""));
    }

    #[test]
    fn endpoint_normalization_and_errors() {
        for e in ["http://h:1", " http://h:1/ ", "http://h:1/v1", "http://h:1/v1/"] {
            assert_eq!(wire("claude", e).unwrap().env[0].1, "http://h:1", "{e}");
        }
        assert_eq!(wire("claude", "https://x.test").unwrap().env[0].1, "https://x.test");
        for e in ["", "localhost:4010", "ftp://h", "http://", "http:///v1"] {
            assert!(wire("claude", e).unwrap_err().contains("invalid endpoint"), "{e}");
        }
        let err = wire("nope", E).unwrap_err();
        assert!(err.contains("nope") && err.contains("anthropic-sdk"));
        for t in TARGETS {
            wire(t, E).unwrap();
        }
    }

    #[test]
    fn formats() {
        let o = get("codex");
        assert_eq!(format(&o, Format::Env).unwrap(), "FAKELLM_API_KEY=fakellm\n");
        assert_eq!(format(&o, Format::Toml).unwrap(), o.files[0].1);
        let sh = format(&o, Format::Shell).unwrap();
        assert!(sh.starts_with("export FAKELLM_API_KEY='fakellm'\n\n# config.toml\nmodel = "));
        assert!(sh.ends_with(&o.snippet));
        let j: Value = serde_json::from_str(&format(&o, Format::Json).unwrap()).unwrap();
        assert_eq!(j["env"]["FAKELLM_API_KEY"], "fakellm");
        assert_eq!(j["files"]["config.toml"], o.files[0].1);
        assert_eq!(j["snippet"], o.snippet);
        assert!(format(&get("claude"), Format::Toml).is_err());
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!("shell".parse::<Format>(), Ok(Format::Shell));
        assert!("yaml".parse::<Format>().is_err());
    }

    #[test]
    fn default_formats() {
        assert_eq!(default_format(&get("claude")), Format::Shell);
        assert_eq!(default_format(&get("codex")), Format::Shell);
        assert_eq!(default_format(&get("pi")), Format::Json);
        assert_eq!(default_format(&get("openai-sdk")), Format::Json);
        assert_eq!(
            default_format(&WireOutput {
                files: vec![("a.toml".into(), String::new())],
                ..WireOutput::default()
            }),
            Format::Toml
        );
    }
}
