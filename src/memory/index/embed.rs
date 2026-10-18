//! Client for an OpenAI-compatible `/v1/embeddings` endpoint.
//!
//! Configured through `USER_MEMORY_EMBED_URL` (full endpoint URL),
//! `USER_MEMORY_EMBED_MODEL`, `USER_MEMORY_EMBED_DIMENSIONS`,
//! `USER_MEMORY_EMBED_QUERY_INSTRUCTION` and `USER_MEMORY_EMBED_API_KEY_FILE`
//! (absolute path of a file holding a bearer token for authenticated
//! endpoints). `USER_MEMORY_EMBED_ALLOW_HTTP` permits plain http to a
//! non-loopback host, for a private network such as a compose sidecar.
//! Each setting comes from the environment, otherwise from the settings a
//! local `user-memory setup` saved beside the installed binary.
//! Semantic recall is optional: every
//! problem disables it with one warning per process and recall falls back to
//! keyword search.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fmt::Display,
    fs,
    future::Future,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const DEFAULT_MODEL: &str = "qwen3-embedding:4b";
/// Inputs per request; keeps request bodies and model batches moderate.
const BATCH: usize = 32;

static WARNED: AtomicBool = AtomicBool::new(false);
static HINTED: AtomicBool = AtomicBool::new(false);

/// Reports, once per process, why semantic recall is not used.
pub(in crate::memory) fn warn(reason: impl Display) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "user-memory: semantic search unavailable ({reason}); searching by keywords only"
        );
    }
}

/// Tells the user, once per process, that some memories still lack vectors.
pub(in crate::memory) fn pending(memories: usize) {
    if !HINTED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "user-memory: {memories} memories are not embedded yet and are found by keyword only; run `user-memory reindex` to embed them now"
        );
    }
}

#[derive(Clone)]
pub(in crate::memory) struct Config {
    url: reqwest::Url,
    model: String,
    dimensions: Option<usize>,
    instruction: String,
    /// Sent only as a bearer token to the configured endpoint; never printed.
    api_key: Option<String>,
}

/// Boolean environment setting: 1/true/yes or 0/false/no (any case); empty is false.
fn flag(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "" | "0" | "false" | "no" => Ok(false),
        _ => bail!("USER_MEMORY_EMBED_ALLOW_HTTP must be 1, true, yes, 0, false or no"),
    }
}

fn read_api_key(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("USER_MEMORY_EMBED_API_KEY_FILE must be an absolute path");
    }
    let key = fs::read_to_string(path).context("USER_MEMORY_EMBED_API_KEY_FILE cannot be read")?;
    let key = key.trim();
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("USER_MEMORY_EMBED_API_KEY_FILE must contain a single printable token");
    }
    Ok(key.to_owned())
}

fn loopback(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

impl Config {
    /// Semantic search configuration, or `None` (after one warning when the
    /// settings are present but unusable) for keyword-only recall.
    pub(in crate::memory) fn from_env() -> Option<Self> {
        match Self::from_settings(&crate::setup::embedding_setting) {
            Ok(config) => config,
            Err(err) => {
                warn(format!("{err:#}"));
                None
            }
        }
    }

    /// Configuration from named settings; `None` when no URL is set.
    pub(in crate::memory) fn from_settings(
        setting: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>> {
        match setting("USER_MEMORY_EMBED_URL") {
            Some(url) if !url.trim().is_empty() => Self::parse(&url, setting).map(Some),
            _ => Ok(None),
        }
    }

    fn parse(url: &str, setting: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let url = reqwest::Url::parse(url.trim()).context("USER_MEMORY_EMBED_URL is not a URL")?;
        let allow_http = match setting("USER_MEMORY_EMBED_ALLOW_HTTP") {
            Some(value) => flag(&value)?,
            None => false,
        };
        let plain = url.scheme() == "http" && !loopback(&url);
        if url.scheme() != "https" && url.scheme() != "http" {
            bail!("USER_MEMORY_EMBED_URL must use HTTPS or HTTP");
        }
        if plain && !allow_http {
            bail!(
                "USER_MEMORY_EMBED_URL must use HTTPS unless it is a loopback address; set USER_MEMORY_EMBED_ALLOW_HTTP=1 only for a private network you control"
            );
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("USER_MEMORY_EMBED_URL must not contain credentials");
        }
        let model = setting("USER_MEMORY_EMBED_MODEL")
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        let dimensions = match setting("USER_MEMORY_EMBED_DIMENSIONS") {
            Some(value) if !value.trim().is_empty() => Some(
                value
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|&d| d > 0)
                    .context("USER_MEMORY_EMBED_DIMENSIONS must be a positive integer")?,
            ),
            _ => None,
        };
        let instruction = setting("USER_MEMORY_EMBED_QUERY_INSTRUCTION")
            .unwrap_or_default()
            .replace("\\n", "\n");
        let api_key = match setting("USER_MEMORY_EMBED_API_KEY_FILE") {
            Some(path) if !path.is_empty() => Some(read_api_key(Path::new(&path))?),
            _ => None,
        };
        if plain && api_key.is_some() {
            bail!(
                "USER_MEMORY_EMBED_API_KEY_FILE is never sent over plain http to a non-loopback host; use HTTPS"
            );
        }
        Ok(Self {
            url,
            model,
            dimensions,
            instruction,
            api_key,
        })
    }

    /// Model and requested dimensions that identify comparable vectors
    /// (0 = the model's default length).
    pub(in crate::memory) fn identity(&self) -> (&str, i64) {
        (&self.model, self.dimensions.map_or(0, |d| d as i64))
    }

    /// Text sent for a query; documents are embedded without the instruction.
    pub(in crate::memory) fn query(&self, topic: &str) -> String {
        format!("{}{topic}", self.instruction)
    }

    /// Embeds `inputs` in order. Blocks the calling thread.
    pub(in crate::memory) fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        // The first request may wait for the model to load.
        self.embed_within(inputs, Duration::from_secs(60))
    }

    /// Like `embed`, but each request gives up after `timeout`.
    pub(in crate::memory) fn embed_within(
        &self,
        inputs: &[String],
        timeout: Duration,
    ) -> Result<Vec<Vec<f32>>> {
        let config = self.clone();
        let inputs = inputs.to_vec();
        blocking(async move { config.request(inputs, timeout).await })?
    }

    async fn request(&self, inputs: Vec<String>, timeout: Duration) -> Result<Vec<Vec<f32>>> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut vectors = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(BATCH) {
            let mut body = json!({"model":self.model,"input":batch});
            if let Some(dimensions) = self.dimensions {
                body["dimensions"] = json!(dimensions);
            }
            let mut request = client.post(self.url.clone()).json(&body);
            if let Some(key) = &self.api_key {
                request = request.bearer_auth(key);
            }
            let reply: Value = request
                .send()
                .await
                .context("Embedding endpoint unreachable")?
                .error_for_status()
                .context("Embedding endpoint refused the request")?
                .json()
                .await
                .context("Embedding endpoint returned invalid JSON")?;
            vectors.extend(parse(&reply, batch.len())?);
        }
        Ok(vectors)
    }
}

/// Vectors from an embeddings response, ordered by their `index`.
fn parse(reply: &Value, expected: usize) -> Result<Vec<Vec<f32>>> {
    let data = reply["data"]
        .as_array()
        .context("Embedding response has no data")?;
    if data.len() != expected {
        bail!(
            "Embedding endpoint returned {} vectors for {expected} inputs",
            data.len()
        );
    }
    let mut vectors = vec![Vec::new(); expected];
    for (position, item) in data.iter().enumerate() {
        let index = item["index"].as_u64().map_or(position, |i| i as usize);
        let vector: Vec<f32> = item["embedding"]
            .as_array()
            .context("Embedding response item has no embedding")?
            .iter()
            .map(|v| v.as_f64().map(|v| v as f32))
            .collect::<Option<_>>()
            .context("Embedding contains a non-numeric value")?;
        if vector.is_empty() || !vector.iter().all(|v| v.is_finite()) {
            bail!("Embedding endpoint returned an empty or non-finite vector");
        }
        let slot = vectors
            .get_mut(index)
            .context("Embedding response index out of range")?;
        if !slot.is_empty() {
            bail!("Embedding response repeats index {index}");
        }
        *slot = vector;
    }
    let length = vectors[0].len();
    if vectors.iter().any(|v| v.len() != length) {
        bail!("Embedding endpoint returned vectors of different lengths");
    }
    Ok(vectors)
}

/// Runs `future` to completion from synchronous code, whether or not the
/// caller is inside a Tokio runtime (MCP tool handlers and the CLI are; unit
/// tests are not). A dedicated thread owns a fresh runtime, and on a
/// multi-threaded runtime the caller's worker is handed off while it waits.
fn blocking<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Result<T> {
    let run = move || {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|runtime| runtime.block_on(future))
        })
        .join()
    };
    let joined = match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(run)
        }
        _ => run(),
    };
    match joined {
        Ok(result) => Ok(result.context("Cannot start embedding client")?),
        Err(_) => bail!("Embedding client panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    /// Answers every embeddings request with one fixed vector per input.
    fn endpoint() -> Config {
        slow_endpoint(Duration::ZERO)
    }

    /// Like `endpoint`, but waits `delay` before each answer.
    fn slow_endpoint(delay: Duration) -> Config {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                std::thread::sleep(delay);
                let data: Vec<Value> = (0..request["input"].as_array().unwrap().len())
                    .map(|i| json!({"index":i,"embedding":[1.0,0.0]}))
                    .collect();
                let reply = json!({"data":data}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        Config {
            url: reqwest::Url::parse(&url).unwrap(),
            model: "test".into(),
            dimensions: None,
            instruction: String::new(),
            api_key: None,
        }
    }

    fn embeds(config: &Config) {
        let vectors = config.embed(&["a".into(), "b".into()]).unwrap();
        assert_eq!(vectors, vec![vec![1.0, 0.0], vec![1.0, 0.0]]);
    }

    #[test]
    fn embeds_outside_any_runtime() {
        embeds(&endpoint());
    }

    // block_in_place would panic here; the client must detect the flavor.
    #[tokio::test(flavor = "current_thread")]
    async fn embeds_from_a_current_thread_runtime() {
        embeds(&endpoint());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn embeds_from_a_multi_thread_runtime() {
        embeds(&endpoint());
    }

    // A recall waiting on the endpoint must hand its worker back so other
    // requests of a long-running server keep being served.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn waiting_for_the_endpoint_does_not_stall_other_tasks() {
        let config = slow_endpoint(Duration::from_millis(400));
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        tokio::spawn(async move { embeds(&config) }).await.unwrap();
        ticker.abort();
        let ticks = ticks.load(Ordering::Relaxed);
        assert!(
            ticks >= 10,
            "other tasks ran only {ticks} times while embedding"
        );
    }

    #[test]
    fn rejects_mismatched_or_malformed_responses() {
        let ok =
            json!({"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]});
        assert_eq!(parse(&ok, 2).unwrap(), vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        for (reply, expected) in [
            (json!({"data":[{"embedding":[1.0]}]}), 2),
            (
                json!({"data":[{"index":0,"embedding":[1.0]},{"index":0,"embedding":[1.0]}]}),
                2,
            ),
            (json!({"data":[{"index":5,"embedding":[1.0]}]}), 1),
            (json!({"data":[{"index":0,"embedding":[]}]}), 1),
            (json!({"data":[{"index":0,"embedding":["x"]}]}), 1),
            (
                json!({"data":[{"index":0,"embedding":[1.0]},{"index":1,"embedding":[1.0,2.0]}]}),
                2,
            ),
            (json!({"error":"nope"}), 1),
        ] {
            assert!(parse(&reply, expected).is_err(), "{reply}");
        }
    }
}
