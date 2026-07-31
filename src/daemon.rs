use tracing::{debug, error, info};
use std::sync::Arc;

use crate::config::Config;
use crate::context::Context;
use crate::error::GremlinError;
use crate::memory::Memory;
use crate::ollama::{Message, Ollama};
use crate::tools::{ToolRegistry, ToolResult};

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
/// Auto-saves to ~/.config/gremlin/conversation.json on each message.
pub struct Conversation {
    pub messages: Vec<Message>,
    pub session_id: String,
    save_path: Option<std::path::PathBuf>,
}

impl Conversation {
    /// Create a new conversation. If a save file exists at the default path,
    /// loads previous messages from it.
    #[allow(dead_code)] // used by unix_daemon on Linux, dead on other platforms
    pub fn new() -> Self {
        let save_path = conversation_save_path();
        // Try to load existing conversation
        if let Some(ref path) = save_path {
            if path.exists() {
                if let Ok(data) = std::fs::read_to_string(path) {
                    if let Ok(saved) = serde_json::from_str::<ConversationData>(&data) {
                        info!(
                            session = %saved.session_id,
                            messages = saved.messages.len(),
                            "Loaded previous conversation"
                        );
                        return Self {
                            messages: saved.messages,
                            session_id: saved.session_id,
                            save_path: Some(path.clone()),
                        };
                    }
                }
            }
        }
        Self {
            messages: Vec::new(),
            session_id: generate_session_id(),
            save_path,
        }
    }

    /// Clear the conversation — reset to system prompt only.
    pub fn clear(&mut self) {
        self.messages.retain(|m| m.role == "system");
        self.session_id = generate_session_id();
        self.save();
        info!(session = %self.session_id, "Conversation cleared");
    }

    /// Add a message to the conversation.
    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
        self.save();
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
            self.save();
            debug!("Conversation trimmed to {} messages", self.messages.len());
        }
    }

    /// Persist conversation to disk (called after push, clear, trim).
    fn save(&self) {
        if let Some(ref path) = self.save_path {
            let data = ConversationData {
                session_id: self.session_id.clone(),
                messages: self.messages.clone(),
            };
            if let Ok(json) = serde_json::to_string_pretty(&data) {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, json);
            }
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

/// Serializable conversation data for disk persistence.
#[derive(serde::Serialize, serde::Deserialize)]
struct ConversationData {
    session_id: String,
    messages: Vec<Message>,
}

/// Path to the conversation save file.
fn conversation_save_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("gremlin").join("conversation.json"))
}

/// Generate a session identifier. NOT a real UUID (no `uuid` crate dependency) —
/// just a hex-encoded nanosecond timestamp. Good enough for a single-user daemon
/// where collisions would require two sessions starting in the same nanosecond,
/// but the name shouldn't imply RFC 4122 compliance.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default() // clock before epoch is absurd but shouldn't panic
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

    // Save the user's turn to conversation history too — previously only
    // assistant/tool-result messages were persisted, so reloaded conversations
    // had no user turns and looked incoherent to the model.
    if let Some(ref mut conv) = conversation {
        conv.push(Message::user(user_message));
    }

    // Tool-use loop
    for _iteration in 0..5 {
        let response = ollama
            .chat(
                model,
                &messages,
                Some(config.model.temperature),
                Some(config.ollama.context_size),
                Some(config.model.keep_alive.clone()),
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
            Some(config.model.keep_alive.clone()),
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
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tracing::{error, info};

    use super::*;

    pub async fn run(config: Config, ollama: Ollama, tools: ToolRegistry, memory: Arc<Memory>) -> Result<(), GremlinError> {
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

        // Bound the read with a timeout — a client that connects but never
        // sends a line (or sends one without a trailing newline) would
        // otherwise block this connection's task forever. It doesn't block
        // OTHER clients (each connection gets its own spawned task and the
        // conversation mutex isn't locked until after this read), but a
        // leaked task per hung client is still a resource leak worth capping.
        const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        match tokio::time::timeout(READ_TIMEOUT, buf_reader.read_line(&mut line)).await {
            Ok(Ok(0)) => return Err(GremlinError::Tool("client disconnected before sending a request".into())),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(GremlinError::Tool("client did not send a request within 30s".into())),
        }

        let request: serde_json::Value = serde_json::from_str(line.trim())?;

        // ── Direct tool invocation (bypasses LLM, ~microsecond latency) ──
        // Used by the sprite viewer and any other performance-critical consumer
        // that needs raw tool output without going through the LLM loop.
        if let Some(tool_name) = request["tool"].as_str() {
            let args = request.get("args").cloned().unwrap_or(serde_json::json!({}));
            let result = tools.execute(tool_name, args);
            let reply = serde_json::json!({
                "response": result.output,
                "success": result.success
            });
            let mut reply_bytes = serde_json::to_vec(&reply)?;
            reply_bytes.push(b'\n');
            writer.write_all(&reply_bytes).await?;
            return Ok(());
        }

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

pub async fn run_daemon(config: Config, ollama: Ollama, tools: ToolRegistry, memory: Arc<Memory>) -> Result<(), GremlinError> {
    #[cfg(unix)]
    {
        unix_daemon::run(config, ollama, tools, memory).await
    }
    #[cfg(not(unix))]
    {
        let _ = (config, ollama, tools, memory);
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