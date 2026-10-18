//! Choosing the embedding model and endpoint: the settings a choice saves,
//! the check of a new endpoint before anything is saved, and bringing a
//! store in line with the setting in effect.

use super::{
    calibration::{self, Calibration},
    connect,
    index::{embed::Config, reindex_with},
};
use anyhow::{Context, Result, bail};
use clap::Args;
use std::{collections::BTreeMap, path::Path};

/// The recommended model, with the settings it is calibrated at.
pub const RECOMMENDED_MODEL: &str = "qwen3-embedding:4b";
/// Vector length the recommended model is used at: calibrated, and a
/// quarter of its native 2,560 in storage and comparison cost.
const RECOMMENDED_DIMENSIONS: usize = 1024;
/// Query instruction the recommended model is trained to expect; `\n` is
/// written as two characters, as the setting stores it.
const RECOMMENDED_INSTRUCTION: &str =
    r"Instruct: Given a question, retrieve the user's matching memory\nQuery:";

/// Path an OpenAI-compatible embeddings endpoint ends with.
const ENDPOINT: &str = "embeddings";

/// Settings by name, as the receipt, the environment and compose hold them.
pub type Settings = BTreeMap<String, String>;

/// An embedding model and endpoint the user chose.
#[derive(Args, Debug, Clone)]
pub struct Choice {
    /// Endpoint: a host such as http://localhost:11434 (/v1/embeddings is
    /// added), a base URL such as https://api.openai.com/v1 (/embeddings is
    /// added), or the full embeddings URL.
    #[arg(long)]
    pub url: String,
    /// Embedding model name, such as qwen3-embedding:4b.
    #[arg(long)]
    pub model: String,
    /// Absolute path of a file holding the API key, for an endpoint that needs one.
    #[arg(long, value_name = "FILE")]
    pub key_file: Option<std::path::PathBuf>,
    /// Vector length to request, for models that can shorten their vectors
    /// (1024 for qwen3-embedding:4b unless set).
    #[arg(long)]
    pub dimensions: Option<usize>,
    /// Allow plain http to a host other than this machine, only on a private
    /// network you control.
    #[arg(long)]
    pub allow_http: bool,
}

impl Choice {
    /// The settings this choice saves: the endpoint URL with `/embeddings`
    /// added when missing, and the recommended model's dimensions and query
    /// instruction unless given.
    pub fn settings(&self) -> Result<Settings> {
        let url = embeddings_endpoint(&self.url)?;
        let model = self.model.trim();
        if model.is_empty() {
            bail!("--model must name a model");
        }
        let mut settings = Settings::from([
            ("USER_MEMORY_EMBED_URL".to_owned(), url),
            ("USER_MEMORY_EMBED_MODEL".to_owned(), model.to_owned()),
        ]);
        let dimensions = self
            .dimensions
            .or((model == RECOMMENDED_MODEL).then_some(RECOMMENDED_DIMENSIONS));
        if let Some(dimensions) = dimensions {
            settings.insert(
                "USER_MEMORY_EMBED_DIMENSIONS".to_owned(),
                dimensions.to_string(),
            );
        }
        if model == RECOMMENDED_MODEL {
            settings.insert(
                "USER_MEMORY_EMBED_QUERY_INSTRUCTION".to_owned(),
                RECOMMENDED_INSTRUCTION.to_owned(),
            );
        }
        if let Some(key) = &self.key_file {
            settings.insert(
                "USER_MEMORY_EMBED_API_KEY_FILE".to_owned(),
                key.display().to_string(),
            );
        }
        if self.allow_http {
            settings.insert("USER_MEMORY_EMBED_ALLOW_HTTP".to_owned(), "1".to_owned());
        }
        // Refuses what the runtime would refuse, before anything is saved.
        config(&settings)?;
        Ok(settings)
    }
}

/// The embeddings endpoint `given` names, as `embedding set` and the
/// installer save it: a URL whose path ends in `/embeddings` as is, without
/// a trailing slash; a bare
/// host, with no path or only `/`, with `/v1/embeddings`, where Ollama and
/// most OpenAI-compatible servers answer (Ollama has no `/embeddings`); any
/// other path, such as `https://api.openai.com/v1`, with `/embeddings`.
pub fn embeddings_endpoint(given: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(given.trim()).context("--url is not a URL")?;
    let path = url.path().trim_end_matches('/').to_owned();
    if path.is_empty() {
        url.set_path(&format!("/v1/{ENDPOINT}"));
    } else if path.ends_with(&format!("/{ENDPOINT}")) {
        url.set_path(&path);
    } else {
        url.set_path(&format!("{path}/{ENDPOINT}"));
    }
    Ok(url.to_string())
}

fn config(settings: &Settings) -> Result<Config> {
    Config::from_settings(&|name| settings.get(name).cloned())?.context("no embedding URL is set")
}

/// What a checked endpoint gives for the store.
pub struct Prepared {
    pub model: String,
    /// Length of the vectors the endpoint returns.
    pub length: usize,
    pub calibration: Calibration,
    /// Why the labelled pairs could not be used, when they could not.
    pub reason: Option<String>,
}

impl Prepared {
    /// One line for the user on what duplicate detection will use.
    pub fn describe(&self) -> String {
        let model = format!("{} ({} dimensions)", self.model, self.length);
        match (&self.calibration, &self.reason) {
            (Calibration::BuiltIn, _) => format!("{model}: built-in cutoffs"),
            (Calibration::Calibrated { duplicate, related }, _) => format!(
                "{model}: calibrated on the labelled pairs (duplicates from {duplicate:.3}, related from {related:.3})"
            ),
            (_, Some(reason)) => format!(
                "{model}: {reason}, so duplicate detection uses keywords; search still uses the model"
            ),
            _ => format!("{model}: not calibrated; duplicate detection uses keywords"),
        }
    }
}

/// Embeds one probe text with `settings`' endpoint, refusing one that
/// errors or returns no vector, then calibrates its model for the store
/// at `db` unless its cutoffs are built in.
pub fn prepare(db: &Path, settings: &Settings) -> Result<Prepared> {
    let config = config(settings)?;
    let probe = config
        .embed(&["user-memory embedding probe".to_owned()])
        .context("The embedding endpoint did not answer the probe")?;
    if probe.first().is_none_or(Vec::is_empty) {
        bail!("The embedding endpoint returned no vector for the probe");
    }
    let found = calibration::calibrate(&connect(db)?, &config)?;
    Ok(Prepared {
        model: config.identity().0.to_owned(),
        length: found.length,
        calibration: found.state,
        reason: found.reason,
    })
}

/// Re-embeds every memory with `settings`' model, replacing vectors from any
/// other: scores from different models cannot be compared. Returns how many
/// memories were embedded.
pub fn reembed(db: &Path, settings: &Settings) -> Result<u64> {
    let config = config(settings)?;
    let done = reindex_with(db, Some(&config))?;
    Ok(done["embedded"].as_u64().unwrap_or(0))
}

/// Drops every stored vector, for keyword-only search; they can be made
/// again from the memories. Returns how many were dropped.
pub fn drop_vectors(db: &Path) -> Result<usize> {
    let conn = connect(db)?;
    let tx = super::schema::write(&conn)?;
    let dropped = tx.execute("DELETE FROM memory_vectors", [])?;
    tx.commit()?;
    Ok(dropped)
}

/// What `embedding show` prints for `settings` and the store at `db`: the
/// endpoint and model, then the vector length and calibration from one
/// probe, or why the probe failed.
pub fn show(db: &Path, settings: &Settings) -> Result<String> {
    let Some(url) = settings.get("USER_MEMORY_EMBED_URL") else {
        return Ok("Embedding: off; search uses keywords only".into());
    };
    let config = config(settings)?;
    let model = config.identity().0.to_owned();
    let mut lines = vec![format!("Endpoint: {url}"), format!("Model: {model}")];
    match config.embed(&["user-memory embedding probe".to_owned()]) {
        Ok(vectors) => {
            let length = vectors.first().map_or(0, Vec::len);
            let calibration = super::calibration(db, &model, length)?;
            lines.push(format!("Vector length: {length}"));
            lines.push(format!(
                "Calibration: {}",
                match calibration {
                    Calibration::BuiltIn => "built in".to_owned(),
                    Calibration::Calibrated { duplicate, related } => format!(
                        "computed from the labelled pairs (duplicates from {duplicate:.3}, related from {related:.3})"
                    ),
                    Calibration::Unseparable => "the model cannot separate the labelled pairs; duplicate detection uses keywords".to_owned(),
                    Calibration::Missing => "none yet; duplicate detection uses keywords until `user-memory embedding set` calibrates it".to_owned(),
                }
            ));
        }
        Err(err) => lines.push(format!("Probe: failed ({err:#})")),
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(url: &str, model: &str) -> Choice {
        Choice {
            url: url.into(),
            model: model.into(),
            key_file: None,
            dimensions: None,
            allow_http: false,
        }
    }

    // A full embeddings URL stays as given, a bare host gets the
    // OpenAI-compatible /v1/embeddings, and any other path gets /embeddings;
    // a trailing slash changes nothing. `embedding set` saves the same.
    #[test]
    fn the_url_becomes_the_embeddings_endpoint() {
        for (given, saved) in [
            (
                "http://localhost:11434",
                "http://localhost:11434/v1/embeddings",
            ),
            (
                "http://localhost:11434/",
                "http://localhost:11434/v1/embeddings",
            ),
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/embeddings",
            ),
            (
                "https://api.openai.com/v1/",
                "https://api.openai.com/v1/embeddings",
            ),
            (
                "https://example.com/v1/embeddings",
                "https://example.com/v1/embeddings",
            ),
            (
                "https://example.com/v1/embeddings/",
                "https://example.com/v1/embeddings",
            ),
            (
                "https://example.com/api",
                "https://example.com/api/embeddings",
            ),
        ] {
            assert_eq!(embeddings_endpoint(given).unwrap(), saved, "{given}");
            let settings = choice(given, "other-model").settings().unwrap();
            assert_eq!(settings["USER_MEMORY_EMBED_URL"], saved, "{given}");
        }
    }

    // The recommended model gets the length and query instruction it is
    // calibrated at, unless given; another model gets neither.
    #[test]
    fn the_recommended_model_gets_its_calibrated_settings() {
        let qwen = choice("http://localhost:11434", RECOMMENDED_MODEL)
            .settings()
            .unwrap();
        assert_eq!(qwen["USER_MEMORY_EMBED_DIMENSIONS"], "1024");
        assert_eq!(
            qwen["USER_MEMORY_EMBED_QUERY_INSTRUCTION"],
            r"Instruct: Given a question, retrieve the user's matching memory\nQuery:"
        );
        let shorter = Choice {
            dimensions: Some(512),
            ..choice("http://localhost:11434", RECOMMENDED_MODEL)
        };
        assert_eq!(
            shorter.settings().unwrap()["USER_MEMORY_EMBED_DIMENSIONS"],
            "512"
        );
        let other = choice("http://localhost:11434", "other-model")
            .settings()
            .unwrap();
        assert_eq!(
            other.keys().map(String::as_str).collect::<Vec<_>>(),
            ["USER_MEMORY_EMBED_MODEL", "USER_MEMORY_EMBED_URL"]
        );
    }

    // What the runtime would refuse is refused before anything is saved:
    // plain http to another host unless allowed, and a blank model.
    #[test]
    fn a_choice_the_runtime_would_refuse_is_refused() {
        let plain = choice("http://192.168.1.9:11434", "other-model");
        assert!(plain.settings().is_err());
        let allowed = Choice {
            allow_http: true,
            ..plain
        };
        assert_eq!(
            allowed.settings().unwrap()["USER_MEMORY_EMBED_ALLOW_HTTP"],
            "1"
        );
        assert!(choice("http://localhost:11434", " ").settings().is_err());
        assert!(choice("not a url", "other-model").settings().is_err());
    }
}
