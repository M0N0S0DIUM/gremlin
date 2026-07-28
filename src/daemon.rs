use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::config::Config;
use crate::context::Context;
use crate::error::GremlinError;
use crate::ollama::{Message, Ollama};
use crate::tools::{ToolRegistry, ToolResult};

#[cfg(unix)]
use tracing::error;

// ── Socket path ──

#[cfg(unix)]
pub fn socket_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::PathBuf::from(dir).join("gremlin.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        std::path::PathBuf::from(format!("/tmp/gremlin-{uid}.sock"))
    }
}

// ── Conversation state ──

/// A conversation session — persists between queries in daemon mode.
/// Single-user: one conversation per daemon. Cleared with `/clear`.
pub struct Conversation {
    pub messages: Vec<Message>,
    pub session_id: String,
}

impl Conversation {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            session_id: uuid_v4(),
        }
    }

    /// Clear the conversation — reset to system prompt only.
    pub fn clear(&mut self) {
        // Keep system messages, drop the rest
        self.messages.retain(|m| m.role == "system");
        self.session_id = uuid_v4();
        info!(session = %self.session_id, "Conversation cleared");
    }

    /// Add a message to the conversation.
    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    /// Trim old messages if the conversation gets too long (keep last ~40 messages).
    pub fn trim(&mut self) {
        let system_count = self.messages.iter().filter(|m| m.role == "system").count();
        let max_total = system_count + 40;
        if self.messages.len() > max_total {
            let keep_system: Vec<Message> = self
                .messages
                .iter()
                .filter(|m| m.role == "system")
                .cloned()
                .collect();
            let recent: Vec<Message> = self
                .messages
                .iter()
                .filter(|m| m.role != "system")
                .rev()
                .take(40)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            self.messages = keep_system;
            self.messages.extend(recent);
            debug!("Conversation trimmed to {} messages", self.messages.len());
        }
    }

    /// Get the full message list for the LLM.
    pub fn messages_for_llm(&self, system_prompt: &str, tools_prompt: &str, context_str: &str) -> Vec<Message> {
        let mut msgs = vec![
            Message::system(system_prompt),
            Message::system(tools_prompt),
        ];
        if !context_str.is_empty() {
            msgs.push(Message::system(context_str));
        }
        msgs.extend(self.messages.clone());
        msgs
    }
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:016x}", t)
}

// ── Tool call parsing ──

fn parse_tool_call(response: &str) -> Option<(String, serde_json::Value)> {
    // Look for ```tool ... ``` blocks
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

    // Try pure JSON (whole response)
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(response.trim()) {
        if let Some(tool) = parsed["tool"].as_str() {
            let args = parsed.get("args").cloned().unwrap_or(serde_json::json!({}));
            return Some((tool.to_string(), args));
        }
    }

    // Try finding JSON object embedded anywhere in the response text
    // Look for {"tool": pattern
    if let Some(start) = response.find("{\"tool\"") {
        let rest = &response[start..];
        // Find the matching closing brace
        let mut depth = 0;
        let mut end = 0;
        for (i, c) in rest.char_indices() {
            if c == '{' { depth += 1; }
            if c == '}' { depth -= 1; if depth == 0 { end = i + 1; break; } }
        }
        if end > 0 {
            let json_str = &rest[..end];
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(tool) = parsed["tool"].as_str() {
                    let args = parsed.get("args").cloned().unwrap_or(serde_json::json!({}));
                    return Some((tool.to_string(), args));
                }
            }
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

// ── Core query loop ──

/// Run a single query through the tool-use loop.
/// If `conversation` is provided, maintains conversation history between calls.
pub async fn query(
    config: &Config,
    ollama: &Ollama,
    tools: &ToolRegistry,
    user_message: &str,
    mut conversation: Option<&mut Conversation>,
) -> Result<String, GremlinError> {
    let model = &config.model.name;

    // Handle /clear command
    if user_message.trim() == "/clear" {
        if let Some(conv) = conversation {
            conv.clear();
            return Ok("Conversation cleared. What's next?".to_string());
        }
        return Ok("(one-shot mode has no conversation to clear)".to_string());
    }

    let context = Context::collect(tools);
    let context_str = context.to_prompt_string();

    // Build messages — conversation mode includes history
    let mut messages = if let Some(ref conv) = conversation {
        conv.messages_for_llm(&config.model.system_prompt, &tools.tools_prompt(), &context_str)
    } else {
        let mut msgs = vec![
            Message::system(&config.model.system_prompt),
            Message::system(&tools.tools_prompt()),
        ];
        if !context_str.is_empty() {
            msgs.push(Message::system(&context_str));
        }
        msgs
    };

    messages.push(Message::user(user_message));

    // Tool-use loop
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
            let result_str = tool_result_to_string(&result);

            // In conversation mode, save the exchange
            if let Some(ref mut conv) = conversation {
                conv.push(Message::assistant(&response));
                conv.push(Message::user(&result_str));
                conv.trim();
            }

            messages.push(Message::assistant(&response));
            messages.push(Message::user(&result_str));
        } else {
            // Final answer — save to conversation
            if let Some(ref mut conv) = conversation {
                conv.push(Message::assistant(&response));
                conv.trim();
            }

            return Ok(response);
        }
    }

    // Final summary after tool loop exhaustion
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

    if let Some(ref mut conv) = conversation {
        conv.push(Message::assistant(&final_response));
        conv.trim();
    }

    Ok(final_response)
}

// ── Daemon (Unix only) ──

#[cfg(unix)]
mod unix_daemon {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tracing::{error, info};

    use super::*;

    pub async fn run(config: Config, ollama: Ollama, tools: ToolRegistry) -> Result<(), GremlinError> {
        let path = socket_path();

        if path.exists() {
            std::fs::remove_file(&path)?;
        }

        let listener = UnixListener::bind(&path).map_err(|e| {
            GremlinError::Tool(format!("Failed to bind to {}: {e}", path.display()))
        })?;

        info!("Gremlin daemon listening on {}", path.display());

        let config = Arc::new(config);
        let ollama = Arc::new(ollama);
        let tools = Arc::new(tools);
        let conversation = Arc::new(Mutex::new(Conversation::new()));

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let config = config.clone();
                    let ollama = ollama.clone();
                    let tools = tools.clone();
                    let conversation = conversation.clone();

                    tokio::spawn(async move {
                        if let Err(e) = handle(stream, &config, &ollama, &tools, &conversation).await {
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
        conversation: &Arc<Mutex<Conversation>>,
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

        // Lock conversation for the duration of this query
        let mut conv = conversation.lock().await;
        let response = query(config, ollama, tools, &message, Some(&mut conv)).await?;
        drop(conv); // Release lock before writing response

        let reply = serde_json::json!({"response": response});
        let mut reply_bytes = serde_json::to_vec(&reply)?;
        reply_bytes.push(b'\n');

        writer.write_all(&reply_bytes).await?;

        Ok(())
    }

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

// ── Public API ──

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