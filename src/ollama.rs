use serde::{Deserialize, Serialize};

use crate::config::OllamaConfig;
use crate::error::OllamaError;

/// Ollama HTTP client — talks to the local Ollama instance
pub struct Ollama {
    client: reqwest::Client,
    base_url: String,
    default_keep_alive: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<ModelOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<String>,
}

#[derive(Debug, Serialize)]
struct ModelOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_ctx: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub message: Message,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    models: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ModelInfo {
    pub name: String,
}

/// A parsed tool call extracted from the model's response text
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
}

impl Ollama {
    pub fn new(config: &OllamaConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: config.url.clone(),
            default_keep_alive: config.keep_alive.clone(),
        }
    }

    /// Check if Ollama is reachable
    pub async fn health_check(&self) -> Result<(), OllamaError> {
        self.client
            .get(&self.base_url)
            .send()
            .await
            .map_err(|e| OllamaError::Unreachable {
                url: self.base_url.clone(),
                detail: e.to_string(),
            })?;
        Ok(())
    }

    /// List available models
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, OllamaError> {
        let resp: ListResponse = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await?
            .json()
            .await?;
        Ok(resp.models)
    }

    /// Send a chat completion request (non-streaming, for tool-use loops)
    pub async fn chat(
        &self,
        model: &str,
        messages: &[Message],
        temperature: Option<f32>,
        context_size: Option<usize>,
        keep_alive: Option<String>,
    ) -> Result<String, OllamaError> {
        let request = ChatRequest {
            model: model.to_string(),
            messages: messages.to_vec(),
            stream: false,
            options: Some(ModelOptions {
                temperature,
                num_ctx: context_size,
            }),
            keep_alive: keep_alive.or_else(|| Some(self.default_keep_alive.clone())),
        };

        let resp: ChatResponse = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&request)
            .send()
            .await?
            .json()
            .await?;

        Ok(resp.message.content)
    }

    /// Quick check: does a model exist locally?
    pub async fn model_exists(&self, model: &str) -> Result<bool, OllamaError> {
        let models = self.list_models().await?;
        Ok(models.iter().any(|m| m.name == model || m.name.starts_with(&format!("{model}:"))))
    }
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
}