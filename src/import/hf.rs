//! Fetching rows from the Hugging Face datasets-server (`/rows`, paged by 100) with the shared HTTP client.
//! `HF_TOKEN` (when set) goes out as a bearer token, which gated datasets need. See docs/m4-spec.md section 6.1.

use std::time::Duration;

use serde_json::Value;

use crate::import::ImportError;
use crate::upstream::client::Fetched;
pub use crate::upstream::client::HttpClient;

/// The size limit of one response.
const BODY_LIMIT: usize = 32 << 20;

/// The longest a `429`'s `retry-after` is honoured.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5);

/// The datasets-server; `FAKELLM_HF_ENDPOINT` replaces it (tests point it at a local server).
pub const DEFAULT_ENDPOINT: &str = "https://datasets-server.huggingface.co";

/// Rows per request; the server's maximum.
pub const PAGE: usize = 100;

/// Which rows to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// `owner/name`.
    pub dataset: String,
    /// `None`: the first config `/splits` lists for the dataset (the one that has `split`).
    pub config: Option<String>,
    pub split: String,
    pub offset: usize,
    /// Rows wanted; paging stops here or at the end of the split.
    pub limit: usize,
}

/// What was fetched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    /// The `row` objects, in order.
    pub rows: Vec<Value>,
    /// Rows left out because the server truncated some of their cells (`truncated_cells` non-empty).
    pub truncated: usize,
    /// `num_rows_total` of the split.
    pub total: Option<usize>,
}

/// The endpoint to use: `FAKELLM_HF_ENDPOINT` or [`DEFAULT_ENDPOINT`], without a trailing slash.
pub fn endpoint() -> String {
    std::env::var("FAKELLM_HF_ENDPOINT")
        .ok()
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

/// Fetches `source` page by page from `endpoint`. A 401/403 says the dataset is gated and mentions `HF_TOKEN`; a
/// 404 says the dataset, config or split was not found (listing what `/splits` knows); other non-200 statuses carry
/// the server's `error`. A page that returns no rows ends the fetch.
pub async fn fetch_rows(client: &HttpClient, endpoint: &str, source: &Source, token: Option<&str>) -> Result<Rows, ImportError> {
    let endpoint = endpoint.trim_end_matches('/');
    let config = match &source.config {
        Some(c) => c.clone(),
        None => first_config(client, endpoint, source, token).await?,
    };
    let mut out = Rows::default();
    let mut offset = source.offset;
    let mut wanted = source.limit;
    while wanted > 0 {
        let length = wanted.min(PAGE);
        let url = format!(
            "{endpoint}/rows?{}",
            form_urlencoded::Serializer::new(String::new())
                .append_pair("dataset", &source.dataset)
                .append_pair("config", &config)
                .append_pair("split", &source.split)
                .append_pair("offset", &offset.to_string())
                .append_pair("length", &length.to_string())
                .finish()
        );
        let page = get_json(client, &url, token, source, endpoint).await?;
        out.total = page.get("num_rows_total").and_then(Value::as_u64).map(|n| n as usize).or(out.total);
        let rows = page.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
        if rows.is_empty() {
            break;
        }
        for entry in &rows {
            let truncated = entry
                .get("truncated_cells")
                .and_then(Value::as_array)
                .is_some_and(|c| !c.is_empty());
            if truncated {
                out.truncated += 1;
            } else if let Some(row) = entry.get("row") {
                out.rows.push(row.clone());
            }
        }
        offset += rows.len();
        wanted = wanted.saturating_sub(rows.len());
        if out.total.is_some_and(|t| offset >= t) {
            break;
        }
    }
    Ok(out)
}

/// The first config `/splits` lists that has the wanted split.
async fn first_config(client: &HttpClient, endpoint: &str, source: &Source, token: Option<&str>) -> Result<String, ImportError> {
    let url = format!(
        "{endpoint}/splits?{}",
        form_urlencoded::Serializer::new(String::new())
            .append_pair("dataset", &source.dataset)
            .finish()
    );
    let body = get_json(client, &url, token, source, endpoint).await?;
    let splits = body.get("splits").and_then(Value::as_array).cloned().unwrap_or_default();
    splits
        .iter()
        .find(|s| s.get("split").and_then(Value::as_str) == Some(source.split.as_str()))
        .and_then(|s| s.get("config").and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| {
            ImportError(format!(
                "dataset {} has no split {:?}; it has {}",
                source.dataset,
                source.split,
                describe_splits(&splits)
            ))
        })
}

fn describe_splits(splits: &[Value]) -> String {
    let list: Vec<String> = splits
        .iter()
        .filter_map(|s| Some(format!("{}/{}", s.get("config")?.as_str()?, s.get("split")?.as_str()?)))
        .collect();
    if list.is_empty() {
        "no splits".to_owned()
    } else {
        format!("config/split {}", list.join(", "))
    }
}

/// GET `url`, retrying once after a 429; non-200 statuses become errors that say what to do.
async fn get_json(client: &HttpClient, url: &str, token: Option<&str>, source: &Source, endpoint: &str) -> Result<Value, ImportError> {
    let auth = token.filter(|t| !t.is_empty()).map(|t| format!("Bearer {t}"));
    let mut headers = vec![("accept", "application/json")];
    if let Some(auth) = &auth {
        headers.push(("authorization", auth));
    }
    let mut fetched = request(client, url, &headers).await?;
    if fetched.status == 429 {
        let wait = fetched
            .headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1))
            .min(MAX_RETRY_WAIT);
        tokio::time::sleep(wait).await;
        fetched = request(client, url, &headers).await?;
    }
    let message = server_error(&fetched);
    match fetched.status {
        200 => serde_json::from_slice(&fetched.body).map_err(|e| ImportError(format!("{url}: invalid JSON in the response: {e}"))),
        401 | 403 => Err(ImportError(format!(
            "dataset {} is gated or private; set HF_TOKEN to a token that can read it",
            source.dataset
        ))),
        404 => {
            let known = known_splits(client, endpoint, source, &headers).await;
            Err(ImportError(format!(
                "dataset, config or split not found ({}){}",
                message.as_deref().unwrap_or("404"),
                known.map(|k| format!("; the dataset has {k}")).unwrap_or_default()
            )))
        }
        429 => Err(ImportError(
            "the datasets-server is rate limiting (429) even after a retry".to_owned(),
        )),
        status => Err(ImportError(format!(
            "datasets-server answered {status}: {}",
            message.unwrap_or_else(|| String::from_utf8_lossy(&fetched.body).chars().take(200).collect())
        ))),
    }
}

async fn request(client: &HttpClient, url: &str, headers: &[(&str, &str)]) -> Result<Fetched, ImportError> {
    client
        .get(url, headers, BODY_LIMIT)
        .await
        .map_err(|e| ImportError(format!("fetching {url}: {e}")))
}

fn server_error(fetched: &Fetched) -> Option<String> {
    let body: Value = serde_json::from_slice(&fetched.body).ok()?;
    body.get("error").and_then(Value::as_str).map(str::to_owned)
}

/// What `/splits` lists, for a 404's message; `None` when it does not answer either.
async fn known_splits(client: &HttpClient, endpoint: &str, source: &Source, headers: &[(&str, &str)]) -> Option<String> {
    let url = format!(
        "{endpoint}/splits?{}",
        form_urlencoded::Serializer::new(String::new())
            .append_pair("dataset", &source.dataset)
            .finish()
    );
    let fetched = client.get(&url, headers, BODY_LIMIT).await.ok()?;
    if fetched.status != 200 {
        return None;
    }
    let body: Value = serde_json::from_slice(&fetched.body).ok()?;
    Some(describe_splits(body.get("splits")?.as_array()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reaches the real datasets-server; runs only with `FAKELLM_HF_TEST=1`.
    #[tokio::test]
    async fn real_datasets_server() {
        if std::env::var("FAKELLM_HF_TEST").as_deref() != Ok("1") {
            eprintln!("skipped: set FAKELLM_HF_TEST=1 to fetch rows from the Hugging Face datasets-server");
            return;
        }
        let source = Source {
            dataset: "allenai/WildChat-1M".into(),
            config: None,
            split: "train".into(),
            offset: 0,
            limit: 5,
        };
        let token = std::env::var("HF_TOKEN").ok();
        let rows = fetch_rows(&HttpClient::new(), &endpoint(), &source, token.as_deref())
            .await
            .unwrap();
        assert_eq!(rows.rows.len() + rows.truncated, 5);
        let converted = crate::import::convert_rows(&rows.rows, crate::import::Format::Auto);
        assert!(!converted.conversations.is_empty(), "{:?}", converted.reasons);
    }
}
