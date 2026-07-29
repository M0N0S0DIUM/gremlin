use thiserror::Error;

#[derive(Error, Debug)]
pub enum GremlinError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    #[error("ollama error: {0}")]
    Ollama(#[from] OllamaError),

    #[error("tool error: {0}")]
    Tool(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("missing config dir: {0}")]
    MissingDir(String),

    #[error("missing required field: {0}")]
    MissingField(String),

    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Error, Debug)]
pub enum OllamaError {
    #[error("ollama not reachable at {url}: {detail}")]
    Unreachable { url: String, detail: String },

    #[error("model {model} not found")]
    ModelNotFound { model: String },

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
}