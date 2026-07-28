use tracing::{debug, info};
#[cfg(unix)]
use tracing::error;

use crate::config::Config;
use crate::context::Context;
use crate::error::GremlinError;
use crate::ollama::{Message, Ollama};
use crate::tools::{ToolRegistry, ToolResult};

// ── Socket path (platform-aware) ──

/// Get the Unix socket path for the daemon. Unix-only.
#[cfg(unix)]
pub fn socket_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::PathBuf::from(dir).join("gremlin.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        std::path::PathBuf::from(format!("/tmp/gremlin-{uid}.sock"))
    }
}

// ── Tool call parsing ──

fn parse_tool_call(response: &str) -> Option<(String, serde_json::Value)> {
    if let Some(start) = response.find("```tool") {
        let json_start = start + "```tool".len();
        let rest = &response[json_start..];
        if let Some(end) = rest.find("```") {
            let json_str = &rest[..end].trim();
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                let tool = parsed["tool"].as_str()?.to_string();
                let args = parsed.get("args").cloned().unwrap_or(serde_json::json!({}));
                return Some((tool, args));
            }
        }
    }

    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(response.trim()) {
        if let Some(tool) = parsed["tool"].as_str() {
            let args = parsed.get("args").cloned().unwrap_or(serde_json::json!({}));
            return Some((tool.to_string(), args));
        }
    }

    None
}

fn tool_result_to_string(result: &ToolResult) -> String {
    if result.success {
        format!("Tool `{}` returned:\n{}", result.tool, result.output)
    } else {
        format!("Tool `{}` failed:\n{}", result.tool, result.output)
    }
}

// ── Core query loop (cross-platform) ──

/// Run a single query through the tool-use loop.
pub async fn query(
    config: &Config,
    ollama: &Ollama,
    tools: &ToolRegistry,
    user_message: &str,
) -> Result<String, GremlinError> {
    let model = &config.model.name;

    let context = Context::collect(tools);
    let context_str = context.to_prompt_string();

    let mut messages = vec![
        Message::system(&config.model.system_prompt),
        Message::system(&tools.tools_prompt()),
    ];

    if !context_str.is_empty() {
        messages.push(Message::system(&context_str));
    }

    messages.push(Message::user(user_message));

    for _iteration in 0..5 {
        let response = ollama
            .chat(
                model,
                &messages,
                Some(config.model.temperature),
                Some(config.ollama.context_size),
            )
            .await?;

        debug!(response = %response, "model response");

        if let Some((tool_name, args)) = parse_tool_call(&response) {
            info!(tool = %tool_name, "executing tool");
            let result = tools.execute(&tool_name, args);
            messages.push(Message::assistant(&response));
            messages.push(Message::user(tool_result_to_string(&result)));
        } else {
            return Ok(response);
        }
    }

    messages.push(Message::user(
        "You've used several tools. Now provide your final answer based on the results above.",
    ));

    let final_response = ollama
        .chat(
            model,
            &messages,
            Some(config.model.temperature),
            Some(config.ollama.context_size),
        )
        .await?;

    Ok(final_response)
}

// ── Daemon (Unix only) ──

#[cfg(unix)]
mod unix_daemon {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tracing::{error, info};

    use super::*;

    /// Start the daemon — listens on a Unix socket for queries.
    pub async fn run(config: Config, ollama: Ollama, tools: ToolRegistry) -> Result<(), GremlinError> {
        let path = socket_path();

        if path.exists() {
            std::fs::remove_file(&path)?;
        }

        let listener = UnixListener::bind(&path).map_err(|e| {
            GremlinError::Tool(format!("Failed to bind to {}: {e}", path.display()))
        })?;

        info!("Gremlin daemon listening on {}", path.display());

        let config = std::sync::Arc::new(config);
        let ollama = std::sync::Arc::new(ollama);
        let tools = std::sync::Arc::new(tools);

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let config = config.clone();
                    let ollama = ollama.clone();
                    let tools = tools.clone();

                    tokio::spawn(async move {
                        if let Err(e) = handle(stream, &config, &ollama, &tools).await {
                            error!("Connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    error!("Accept error: {e}");
                }
            }
        }
    }

    async fn handle(
        stream: tokio::net::UnixStream,
        config: &Config,
        ollama: &Ollama,
        tools: &ToolRegistry,
    ) -> Result<(), GremlinError> {
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();

        buf_reader.read_line(&mut line).await?;

        let request: serde_json::Value = serde_json::from_str(line.trim())?;
        let message = request["message"]
            .as_str()
            .unwrap_or("(empty)")
            .to_string();

        info!(message = %message, "daemon query");

        let response = query(config, ollama, tools, &message).await?;

        let reply = serde_json::json!({"response": response});
        let mut reply_bytes = serde_json::to_vec(&reply)?;
        reply_bytes.push(b'\n');

        writer.write_all(&reply_bytes).await?;

        Ok(())
    }

    /// Try to send a query to the running daemon.
    pub async fn send(message: &str) -> Result<String, GremlinError> {
        let path = socket_path();

        let stream = tokio::net::UnixStream::connect(&path).await.map_err(|e| {
            GremlinError::Tool(format!("Daemon not running at {}: {e}", path.display()))
        })?;

        let (reader, mut writer) = stream.into_split();

        let request = serde_json::json!({"message": message});
        let mut request_bytes = serde_json::to_vec(&request)?;
        request_bytes.push(b'\n');
        writer.write_all(&request_bytes).await?;

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;

        let response: serde_json::Value = serde_json::from_str(line.trim())?;
        let text = response["response"]
            .as_str()
            .unwrap_or("(no response)")
            .to_string();

        Ok(text)
    }
}

// ── Public API (platform-dispatching) ──

/// Start the daemon. Unix-only; panics with a clear message on other platforms.
pub async fn run_daemon(config: Config, ollama: Ollama, tools: ToolRegistry) -> Result<(), GremlinError> {
    #[cfg(unix)]
    {
        unix_daemon::run(config, ollama, tools).await
    }
    #[cfg(not(unix))]
    {
        let _ = (config, ollama, tools);
        Err(GremlinError::Tool(
            "Daemon mode is only supported on Linux/Unix. Use `gremlin ask` for one-shot queries.".into(),
        ))
    }
}

/// Send a query to the daemon. Returns Err if daemon isn't running or platform doesn't support it.
pub async fn send_to_daemon(message: &str) -> Result<String, GremlinError> {
    #[cfg(unix)]
    {
        unix_daemon::send(message).await
    }
    #[cfg(not(unix))]
    {
        let _ = message;
        Err(GremlinError::Tool("Daemon not available on this platform.".into()))
    }
}